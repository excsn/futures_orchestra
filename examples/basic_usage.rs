use futures_orchestra::{FuturePoolManager, ShutdownMode, TaskHandle};
use std::collections::HashSet;
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::info;

async fn my_task_fn(id: usize, delay_ms: u64) -> String {
  info!("Task {} starting, will sleep for {}ms", id, delay_ms);
  tokio::time::sleep(Duration::from_millis(delay_ms)).await;
  let result = format!("Task {} finished successfully after {}ms", id, delay_ms);
  info!("{}", result);
  result
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .with_target(false) // Disable module paths for cleaner example output
    .init();

  info!("--- Basic Usage Example ---");

  let pool_name = "basic_pool";
  let manager = FuturePoolManager::<String>::new(
    2,  // Concurrency limit
    10, // Queue capacity
    Handle::current(),
    pool_name,
  );

  let mut handles: Vec<TaskHandle<String>> = Vec::new();

  for i in 0..5 {
    let task_id: usize = i;
    // Alternate sleep times for variety
    let sleep_duration: u64 = 500 + (i as u64 % 3 * 250);
    let future = Box::pin(async move { my_task_fn(task_id, sleep_duration).await });
    match manager.submit(HashSet::new(), future).await {
      Ok(handle) => {
        info!("Submitted task {} with handle id {}", task_id, handle.id());
        handles.push(handle);
      }
      Err(e) => {
        tracing::error!("Failed to submit task {}: {:?}", task_id, e);
      }
    }
  }

  info!("All tasks submitted. Awaiting results...");

  for handle in handles {
    let task_id = handle.id();
    match handle.await_result().await {
      Ok(result) => info!("Result for task {}: {}", task_id, result),
      Err(e) => info!("Error for task {}: {:?}", task_id, e),
    }
  }

  info!("All task results processed. Shutting down pool.");
  manager
    .shutdown(ShutdownMode::Graceful)
    .await
    .expect("Pool shutdown failed");
  info!("Pool shutdown complete.");
  info!("--- Basic Usage Example End ---");
}
