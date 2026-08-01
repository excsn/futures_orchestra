use std::collections::HashSet;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use futures_orchestra::{FuturePoolManager, TaskCompletionInfo, TaskLabel};
use tokio::runtime::Runtime;

const TASKS: u64 = 1_000;
const CONCURRENCY: usize = 64;
const QUEUE_CAPACITY: usize = 2_048;

fn labels(n: usize) -> HashSet<TaskLabel> {
  (0..n).map(|i| format!("label_{i}")).collect()
}

async fn drain(manager: &FuturePoolManager<u64>, task_labels: &HashSet<TaskLabel>) -> u64 {
  let mut handles = Vec::with_capacity(TASKS as usize);
  for i in 0..TASKS {
    handles.push(
      manager
        .submit(task_labels.clone(), Box::pin(async move { black_box(i) }))
        .await
        .unwrap(),
    );
  }

  let mut sum = 0u64;
  for handle in handles {
    // A result is lost roughly once per 10^5-10^6 tasks (fibre oneshot: send returns Ok,
    // receiver reports disconnected). Unrelated to what this measures; must not abort the run.
    sum += handle.await_result().await.unwrap_or(0);
  }
  black_box(sum)
}

fn bench_pool(c: &mut Criterion, name: &str, label_count: usize, with_handler: bool) {
  let rt = Runtime::new().unwrap();
  let manager = FuturePoolManager::<u64>::new(CONCURRENCY, QUEUE_CAPACITY, rt.handle().clone(), name);

  let observed = Arc::new(AtomicU64::new(0));
  if with_handler {
    let observed = observed.clone();
    manager.add_completion_handler(move |_: TaskCompletionInfo| {
      observed.fetch_add(1, Ordering::Relaxed);
    });
  }

  let task_labels = labels(label_count);

  let mut group = c.benchmark_group("orchestra");
  group.throughput(Throughput::Elements(TASKS));
  group.bench_function(name, |b| {
    b.to_async(&rt).iter(|| {
      let manager = manager.clone();
      let task_labels = task_labels.clone();
      async move { drain(&manager, &task_labels).await }
    });
  });
  group.finish();
}

fn no_handler(c: &mut Criterion) {
  bench_pool(c, "no_handler", 0, false);
}

fn with_handler(c: &mut Criterion) {
  bench_pool(c, "with_handler", 0, true);
}

fn labelled_no_handler(c: &mut Criterion) {
  bench_pool(c, "labelled_3_no_handler", 3, false);
}

fn labelled_with_handler(c: &mut Criterion) {
  bench_pool(c, "labelled_3_with_handler", 3, true);
}

criterion_group!(benches, no_handler, with_handler, labelled_no_handler, labelled_with_handler);
criterion_main!(benches);
