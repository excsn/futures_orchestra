use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tokio::runtime::Runtime;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const TASKS: u64 = 1_000;
const CONCURRENCY: usize = 64;

async fn trivial(i: u64) -> u64 {
  black_box(i)
}

fn spawn_joinset(c: &mut Criterion) {
  let rt = Runtime::new().unwrap();
  let mut group = c.benchmark_group("tokio_baseline");
  group.throughput(Throughput::Elements(TASKS));

  group.bench_function("spawn_joinset", |b| {
    b.to_async(&rt).iter(|| async {
      let mut set = JoinSet::new();
      for i in 0..TASKS {
        set.spawn(trivial(i));
      }
      let mut sum = 0u64;
      while let Some(res) = set.join_next().await {
        sum += res.unwrap();
      }
      black_box(sum)
    });
  });

  group.finish();
}

fn spawn_semaphore_gated(c: &mut Criterion) {
  let rt = Runtime::new().unwrap();
  let mut group = c.benchmark_group("tokio_baseline");
  group.throughput(Throughput::Elements(TASKS));

  group.bench_function("spawn_semaphore_gated", |b| {
    b.to_async(&rt).iter(|| async {
      let gate = Arc::new(Semaphore::new(CONCURRENCY));
      let mut set = JoinSet::new();
      for i in 0..TASKS {
        let gate = gate.clone();
        set.spawn(async move {
          let _permit = gate.acquire_owned().await.unwrap();
          trivial(i).await
        });
      }
      let mut sum = 0u64;
      while let Some(res) = set.join_next().await {
        sum += res.unwrap();
      }
      black_box(sum)
    });
  });

  group.finish();
}

/// Semaphore-gated spawn plus the two things almost every caller ends up adding: a
/// channel to get the result back, and panic isolation so one task can't kill the caller.
fn spawn_with_result_and_panic_guard(c: &mut Criterion) {
  use fibre::oneshot::exclusive;
  use futures::FutureExt;
  use std::panic::AssertUnwindSafe;

  let rt = Runtime::new().unwrap();
  let mut group = c.benchmark_group("tokio_baseline");
  group.throughput(Throughput::Elements(TASKS));

  group.bench_function("spawn_result_panic_guard", |b| {
    b.to_async(&rt).iter(|| async {
      let gate = Arc::new(Semaphore::new(CONCURRENCY));
      let mut receivers = Vec::with_capacity(TASKS as usize);
      for i in 0..TASKS {
        let gate = gate.clone();
        let (tx, rx) = exclusive::<Result<u64, ()>>();
        tokio::spawn(async move {
          let _permit = gate.acquire_owned().await.unwrap();
          let outcome = AssertUnwindSafe(trivial(i)).catch_unwind().await.map_err(|_| ());
          let _ = tx.send(outcome);
        });
        receivers.push(rx);
      }
      let mut sum = 0u64;
      for mut rx in receivers {
        sum += rx.recv().await.unwrap().unwrap();
      }
      black_box(sum)
    });
  });

  group.finish();
}

/// The above plus per-task cancellation and a registry to find live tasks by id, which
/// is the minimum needed to offer anything like `cancel_tasks_by_label`.
fn spawn_full_featured(c: &mut Criterion) {
  use fibre::oneshot::exclusive;
  use futures::FutureExt;
  use parking_lot::RwLock;
  use std::collections::HashMap;
  use std::panic::AssertUnwindSafe;
  use tokio_util::sync::CancellationToken;

  let rt = Runtime::new().unwrap();
  let mut group = c.benchmark_group("tokio_baseline");
  group.throughput(Throughput::Elements(TASKS));

  group.bench_function("spawn_full_featured", |b| {
    b.to_async(&rt).iter(|| async {
      let gate = Arc::new(Semaphore::new(CONCURRENCY));
      let active: Arc<RwLock<HashMap<u64, CancellationToken>>> = Arc::new(RwLock::new(HashMap::new()));
      let mut receivers = Vec::with_capacity(TASKS as usize);

      for i in 0..TASKS {
        let gate = gate.clone();
        let active = active.clone();
        let token = CancellationToken::new();
        let task_token = token.clone();

        let (tx, rx) = exclusive::<Result<u64, ()>>();
        tokio::spawn(async move {
          let _permit = gate.acquire_owned().await.unwrap();
          // Registered only once running, so the map holds at most CONCURRENCY entries,
          // matching what the pool's dispatcher does.
          active.write().insert(i, token);
          let outcome = tokio::select! {
            _ = task_token.cancelled() => Err(()),
            r = AssertUnwindSafe(trivial(i)).catch_unwind() => r.map_err(|_| ()),
          };
          let _ = tx.send(outcome);
          active.write().remove(&i);
        });
        receivers.push(rx);
      }

      let mut sum = 0u64;
      for mut rx in receivers {
        sum += rx.recv().await.unwrap().unwrap();
      }
      black_box(sum)
    });
  });

  group.finish();
}

criterion_group!(
  benches,
  spawn_joinset,
  spawn_semaphore_gated,
  spawn_with_result_and_panic_guard,
  spawn_full_featured
);
criterion_main!(benches);
