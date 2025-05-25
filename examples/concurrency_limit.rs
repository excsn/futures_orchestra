use futures_orchestra::{FuturePoolManager, ShutdownMode, TaskHandle};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::info;

async fn long_task_fn(id: usize) -> String {
  info!("Task {} starting (concurrency test - should take 1s)", id);
  tokio::time::sleep(Duration::from_secs(1)).await;
  let result = format!("Task {} finished", id);
  info!("{}", result);
  result
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .with_target(false)
    .init();

  info!("--- Concurrency Limit Example (Limit: 2) ---");

  let concurrency_limit = 2;
  let manager = FuturePoolManager::<String>::new(concurrency_limit, 10, Handle::current(), "concurrency_pool");

  let num_tasks = 5;
  let mut handles: Vec<TaskHandle<String>> = Vec::new();

  info!(
    "Submitting {} tasks, each takes 1 sec. With concurrency {}, this should take ~{} secs.",
    num_tasks,
    concurrency_limit,
    (num_tasks as f32 / concurrency_limit as f32).ceil()
  );

  for i in 0..num_tasks {
    let future = Box::pin(async move { long_task_fn(i).await });
    match manager.submit(HashSet::new(), future).await {
      Ok(handle) => {
        handles.push(handle);
      }
      Err(e) => {
        tracing::error!("Failed to submit task {}: {:?}", i, e);
      }
    }
  }

  // Await all results
  for handle in handles {
    let task_id = handle.id();
    match handle.await_result().await {
      Ok(result) => info!("Task {} main: Received result: {}", task_id, result),
      Err(e) => info!("Task {} main: Received error: {:?}", task_id, e),
    }
  }

  info!("All tasks processed. Shutting down.");
  manager
    .shutdown(ShutdownMode::Graceful)
    .await
    .expect("Pool shutdown failed");
  info!("Pool shutdown complete.");
  info!("--- Concurrency Limit Example End ---");
}
