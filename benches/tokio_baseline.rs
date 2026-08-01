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

criterion_group!(benches, spawn_joinset, spawn_semaphore_gated);
criterion_main!(benches);
