use futures_orchestra::{FuturePoolManager, ShutdownMode};
use std::collections::HashSet;
use tokio::runtime::Handle;

const PER_ROUND: u64 = 1_000;
const DEFAULT_TASKS: u64 = 5_000_000;

fn task_budget() -> u64 {
  std::env::var("FO_STRESS_TASKS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(DEFAULT_TASKS)
}

/// Ignored because it needs millions of tasks to be conclusive and takes several seconds.
///
/// It currently fails: a result is lost roughly once per 700k tasks. Stage counters show
/// every task is submitted, dequeued, spawned, and its result accepted by
/// `fibre::oneshot::Sender::send` (returning `Ok`), yet the paired receiver reports
/// `channel disconnected (empty and all senders dropped)`. The loss is below this crate,
/// in the oneshot itself, and is not reproducible with the oneshot in isolation.
///
/// Run with: `cargo test --release --test result_delivery -- --ignored`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn no_task_result_is_lost_under_load() {
  let manager = FuturePoolManager::<u64>::new(64, 2048, Handle::current(), "result_delivery");

  let budget = task_budget();
  let rounds = budget.div_ceil(PER_ROUND);
  let mut lost = 0u64;
  let mut total = 0u64;

  for _ in 0..rounds {
    let mut handles = Vec::with_capacity(PER_ROUND as usize);
    for i in 0..PER_ROUND {
      handles.push(
        manager
          .submit(HashSet::new(), Box::pin(async move { i }))
          .await
          .expect("submit failed"),
      );
    }
    for handle in handles {
      total += 1;
      if handle.await_result().await.is_err() {
        lost += 1;
      }
    }
  }

  manager.shutdown(ShutdownMode::Graceful).await.unwrap();

  assert_eq!(
    lost, 0,
    "lost {lost} of {total} task results; every send returned Ok, so the value was dropped inside the result oneshot"
  );
}
