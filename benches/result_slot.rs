use std::cell::UnsafeCell;
use std::hint::black_box;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use futures::task::AtomicWaker;
use tokio::runtime::Runtime;

const ELEMS: u64 = 1_000;

const VALUE: u8 = 1;
const SENDER_GONE: u8 = 2;
const RECEIVER_GONE: u8 = 4;
const TAKEN: u8 = 8;

/// Prototype of an embeddable single-shot result slot: lives inside a caller's
/// allocation instead of allocating its own. Single sender, single receiver.
struct Slot<T> {
  state: AtomicU8,
  waker: AtomicWaker,
  value: UnsafeCell<MaybeUninit<T>>,
}

unsafe impl<T: Send> Sync for Slot<T> {}

impl<T> Slot<T> {
  fn new() -> Self {
    Slot {
      state: AtomicU8::new(0),
      waker: AtomicWaker::new(),
      value: UnsafeCell::new(MaybeUninit::uninit()),
    }
  }

  /// Single-call: the sender writes the value before publishing VALUE, so the cell
  /// write is unsynchronized-exclusive and the fetch_or releases it.
  fn send(&self, value: T) -> Result<(), T> {
    unsafe { (*self.value.get()).write(value) };
    let prev = self.state.fetch_or(VALUE, Ordering::AcqRel);
    if prev & RECEIVER_GONE != 0 {
      return Err(unsafe { (*self.value.get()).assume_init_read() });
    }
    self.waker.wake();
    Ok(())
  }

  fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<T, ()>> {
    let state = self.state.load(Ordering::Acquire);
    if state & VALUE != 0 {
      self.state.fetch_or(TAKEN, Ordering::Relaxed);
      return Poll::Ready(Ok(unsafe { (*self.value.get()).assume_init_read() }));
    }
    if state & SENDER_GONE != 0 {
      return Poll::Ready(Err(()));
    }

    self.waker.register(cx.waker());
    // Recheck after registering, or a send between the load and the register would
    // never wake this receiver.
    let state = self.state.load(Ordering::Acquire);
    if state & VALUE != 0 {
      self.state.fetch_or(TAKEN, Ordering::Relaxed);
      return Poll::Ready(Ok(unsafe { (*self.value.get()).assume_init_read() }));
    }
    if state & SENDER_GONE != 0 {
      return Poll::Ready(Err(()));
    }
    Poll::Pending
  }

  async fn recv(&self) -> Result<T, ()> {
    std::future::poll_fn(|cx| self.poll_recv(cx)).await
  }
}

impl<T> Drop for Slot<T> {
  fn drop(&mut self) {
    let state = *self.state.get_mut();
    if state & VALUE != 0 && state & TAKEN == 0 {
      unsafe { (*self.value.get()).assume_init_drop() };
    }
  }
}

/// Stand-in for `TaskCore`: some control state plus, in the slot arm, the result slot.
struct CoreWithSlot {
  task_id: u64,
  slot: Slot<u64>,
}

struct CoreBare {
  task_id: u64,
}

fn bench_same_task(c: &mut Criterion) {
  let rt = Runtime::new().unwrap();
  let mut group = c.benchmark_group("result_slot");
  group.throughput(Throughput::Elements(ELEMS));

  group.bench_function("fibre_oneshot_same_task", |b| {
    b.to_async(&rt).iter(|| async {
      let mut sum = 0u64;
      for i in 0..ELEMS {
        let core = Arc::new(CoreBare { task_id: i });
        let (tx, mut rx) = fibre::oneshot::exclusive::<u64>();
        tx.send(core.task_id).unwrap();
        sum += rx.recv().await.unwrap();
        black_box(&core);
      }
      black_box(sum)
    });
  });

  group.bench_function("slot_same_task", |b| {
    b.to_async(&rt).iter(|| async {
      let mut sum = 0u64;
      for i in 0..ELEMS {
        let core = Arc::new(CoreWithSlot {
          task_id: i,
          slot: Slot::new(),
        });
        core.slot.send(core.task_id).unwrap();
        sum += core.slot.recv().await.unwrap();
      }
      black_box(sum)
    });
  });

  group.finish();
}

fn bench_cross_task(c: &mut Criterion) {
  let rt = Runtime::new().unwrap();
  let mut group = c.benchmark_group("result_slot");
  group.throughput(Throughput::Elements(ELEMS));

  group.bench_function("fibre_oneshot_cross_task", |b| {
    b.to_async(&rt).iter(|| async {
      let mut receivers = Vec::with_capacity(ELEMS as usize);
      for i in 0..ELEMS {
        let (tx, rx) = fibre::oneshot::exclusive::<u64>();
        tokio::spawn(async move {
          let _ = tx.send(i);
        });
        receivers.push(rx);
      }
      let mut sum = 0u64;
      for mut rx in receivers {
        sum += rx.recv().await.unwrap();
      }
      black_box(sum)
    });
  });

  group.bench_function("slot_cross_task", |b| {
    b.to_async(&rt).iter(|| async {
      let mut cores = Vec::with_capacity(ELEMS as usize);
      for i in 0..ELEMS {
        let core = Arc::new(CoreWithSlot {
          task_id: i,
          slot: Slot::new(),
        });
        let sender_core = core.clone();
        tokio::spawn(async move {
          let _ = sender_core.slot.send(sender_core.task_id);
        });
        cores.push(core);
      }
      let mut sum = 0u64;
      for core in cores {
        sum += core.slot.recv().await.unwrap();
      }
      black_box(sum)
    });
  });

  group.finish();
}

criterion_group!(benches, bench_same_task, bench_cross_task);
criterion_main!(benches);
