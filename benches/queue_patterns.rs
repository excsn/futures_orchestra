use std::collections::HashSet;
use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tokio::runtime::Runtime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MSGS: u64 = 1_000;
const QUEUE_CAPACITY: usize = 2_048;

/// Mirrors the pool's `QueueMessage`: a boxed future, ids and labels, and the
/// capacity permit that travels with the message and releases on dequeue.
struct Msg {
  id: u64,
  labels: Arc<HashSet<String>>,
  _future: Pin<Box<dyn Future<Output = u64> + Send>>,
  _permit: OwnedSemaphorePermit,
}

async fn make_msg(id: u64, labels: &Arc<HashSet<String>>, gate: &Arc<Semaphore>) -> Msg {
  Msg {
    id,
    labels: labels.clone(),
    _future: Box::pin(async move { id }),
    _permit: gate.clone().acquire_owned().await.unwrap(),
  }
}

fn consume(msg: Msg) -> u64 {
  black_box(msg.id.wrapping_add(msg.labels.len() as u64))
}

fn bench_mpsc_single(c: &mut Criterion) {
  let rt = Runtime::new().unwrap();
  let mut group = c.benchmark_group("queue_patterns");
  group.throughput(Throughput::Elements(MSGS));

  group.bench_function("mpsc_1_consumer", |b| {
    b.to_async(&rt).iter(|| async {
      let (tx, mut rx) = fibre::mpsc::unbounded_async::<Msg>();
      let gate = Arc::new(Semaphore::new(QUEUE_CAPACITY));
      let labels: Arc<HashSet<String>> = Arc::new(HashSet::new());

      let consumer = tokio::spawn(async move {
        let mut sum = 0u64;
        while let Ok(msg) = rx.recv().await {
          sum += consume(msg);
        }
        sum
      });

      let mut tx = tx;
      for i in 0..MSGS {
        let msg = make_msg(i, &labels, &gate).await;
        tx.send(msg).await.unwrap();
      }
      drop(tx);

      black_box(consumer.await.unwrap())
    });
  });

  group.finish();
}

fn bench_mpmc(c: &mut Criterion, consumers: usize) {
  let rt = Runtime::new().unwrap();
  let mut group = c.benchmark_group("queue_patterns");
  group.throughput(Throughput::Elements(MSGS));

  group.bench_function(format!("mpmc_{consumers}_consumers"), |b| {
    b.to_async(&rt).iter(|| async {
      let (tx, rx) = fibre::mpmc::unbounded_async::<Msg>();
      let gate = Arc::new(Semaphore::new(QUEUE_CAPACITY));
      let labels: Arc<HashSet<String>> = Arc::new(HashSet::new());

      let mut handles = Vec::with_capacity(consumers);
      for _ in 0..consumers {
        let mut rx = rx.clone();
        handles.push(tokio::spawn(async move {
          let mut sum = 0u64;
          while let Ok(msg) = rx.recv().await {
            sum += consume(msg);
          }
          sum
        }));
      }
      drop(rx);

      let mut tx = tx;
      for i in 0..MSGS {
        let msg = make_msg(i, &labels, &gate).await;
        tx.send(msg).await.unwrap();
      }
      drop(tx);

      let mut total = 0u64;
      for handle in handles {
        total += handle.await.unwrap();
      }
      black_box(total)
    });
  });

  group.finish();
}

fn bench_mpsc_routed(c: &mut Criterion, queues: usize) {
  let rt = Runtime::new().unwrap();
  let mut group = c.benchmark_group("queue_patterns");
  group.throughput(Throughput::Elements(MSGS));

  group.bench_function(format!("mpsc_routed_{queues}_queues"), |b| {
    b.to_async(&rt).iter(|| async {
      let gate = Arc::new(Semaphore::new(QUEUE_CAPACITY));
      let labels: Arc<HashSet<String>> = Arc::new(HashSet::new());

      let mut txs = Vec::with_capacity(queues);
      let mut handles = Vec::with_capacity(queues);
      for _ in 0..queues {
        let (tx, mut rx) = fibre::mpsc::unbounded_async::<Msg>();
        txs.push(tx);
        handles.push(tokio::spawn(async move {
          let mut sum = 0u64;
          while let Ok(msg) = rx.recv().await {
            sum += consume(msg);
          }
          sum
        }));
      }

      for i in 0..MSGS {
        let msg = make_msg(i, &labels, &gate).await;
        txs[(i as usize) % queues].send(msg).await.unwrap();
      }
      drop(txs);

      let mut total = 0u64;
      for handle in handles {
        total += handle.await.unwrap();
      }
      black_box(total)
    });
  });

  group.finish();
}

fn mpsc_1(c: &mut Criterion) {
  bench_mpsc_single(c);
}

fn mpmc_1(c: &mut Criterion) {
  bench_mpmc(c, 1);
}

fn mpmc_2(c: &mut Criterion) {
  bench_mpmc(c, 2);
}

fn mpmc_4(c: &mut Criterion) {
  bench_mpmc(c, 4);
}

fn mpsc_routed_2(c: &mut Criterion) {
  bench_mpsc_routed(c, 2);
}

fn mpsc_routed_4(c: &mut Criterion) {
  bench_mpsc_routed(c, 4);
}

criterion_group!(benches, mpsc_1, mpmc_1, mpmc_2, mpmc_4, mpsc_routed_2, mpsc_routed_4);
criterion_main!(benches);
