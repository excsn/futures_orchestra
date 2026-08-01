use futures_orchestra::{FuturePoolManager, ShutdownMode, TaskCompletionInfo, TaskLabel};
use std::collections::HashSet;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::runtime::{Handle, Runtime};

const CONCURRENCY: usize = 64;
const QUEUE_CAPACITY: usize = 2048;
const BATCH: u64 = 1_000;

fn labels(n: usize) -> HashSet<TaskLabel> {
  (0..n).map(|i| format!("label_{i}")).collect()
}

async fn run(mode: &str, total: u64) -> f64 {
  let manager = FuturePoolManager::<u64>::new(CONCURRENCY, QUEUE_CAPACITY, Handle::current(), "profile");

  let observed = Arc::new(AtomicU64::new(0));
  if mode == "handler" || mode == "labelled_handler" {
    let observed = observed.clone();
    manager.add_completion_handler(move |_: TaskCompletionInfo| {
      observed.fetch_add(1, Ordering::Relaxed);
    });
  }

  let task_labels = match mode {
    "labelled" | "labelled_handler" => labels(3),
    _ => HashSet::new(),
  };

  let submitters: u64 = std::env::var("FO_SUBMITTERS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(1);

  let started = Instant::now();
  let mut submitter_tasks = Vec::new();
  for _ in 0..submitters {
    let manager = manager.clone();
    let task_labels = task_labels.clone();
    let share = total / submitters;
    submitter_tasks.push(tokio::spawn(async move {
      let mut done = 0u64;
      while done < share {
        let batch = BATCH.min(share - done);
        let mut handles = Vec::with_capacity(batch as usize);
        for i in 0..batch {
          handles.push(
            manager
              .submit(task_labels.clone(), Box::pin(async move { black_box(i) }))
              .await
              .expect("submit failed"),
          );
        }
        for handle in handles {
          black_box(handle.await_result().await.expect("task failed"));
          done += 1;
        }
      }
    }));
  }
  for t in submitter_tasks {
    t.await.unwrap();
  }
  let elapsed = started.elapsed();

  manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  total as f64 / elapsed.as_secs_f64()
}

fn main() {
  let mut args = std::env::args().skip(1);
  let mode = args.next().unwrap_or_else(|| "plain".to_string());
  let total: u64 = args
    .next()
    .and_then(|v| v.parse().ok())
    .unwrap_or(2_000_000);

  let rt = Runtime::new().unwrap();
  let per_sec = rt.block_on(run(&mode, total));

  println!(
    "{mode}: {total} tasks, {:.0} tasks/s, {:.0} ns/task",
    per_sec,
    1e9 / per_sec
  );
}
