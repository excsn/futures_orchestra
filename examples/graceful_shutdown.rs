use futures_orchestra::{FuturePoolManager, ShutdownMode, TaskHandle};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::info;

async fn work_task_fn(id: usize, duration_s: u64) -> String {
  info!("Task {} starting (will run for {}s)", id, duration_s);
  tokio::time::sleep(Duration::from_secs(duration_s)).await;
  let result = format!("Task {} finished after {}s", id, duration_s);
  info!("{}", result);
  result
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .with_target(false)
    .init();
  info!("--- Graceful Shutdown Example ---");

  let manager = FuturePoolManager::<String>::new(
    2, // Concurrency limit of 2
    10,
    Handle::current(),
    "graceful_shutdown_pool",
  );

  let mut handles: Vec<TaskHandle<String>> = Vec::new();

  // Submit 5 tasks, each takes 2 seconds.
  // With concurrency 2:
  // Tasks 0, 1 start.
  // Tasks 2, 3, 4 are queued.
  for i in 0..5 {
    let future = Box::pin(async move { work_task_fn(i, 2).await });
    match manager.submit(HashSet::new(), future).await {
      Ok(handle) => {
        info!("Submitted task {} (handle id {})", i, handle.id());
        handles.push(handle);
      }
      Err(e) => tracing::error!("Failed to submit task {}: {:?}", i, e),
    }
  }

  info!(
    "All 5 tasks submitted. Queue size: {}, Active: {}",
    manager.queued_task_count(),
    manager.active_task_count()
  );
  info!("Initiating GRACEFUL shutdown shortly...");
  tokio::time::sleep(Duration::from_millis(100)).await; // Let some tasks start

  // Clone manager for shutdown, as it takes Arc<Self>
  let manager_for_shutdown = manager.clone();
  let shutdown_jh = tokio::spawn(async move {
    info!("Calling pool.shutdown(Graceful)...");
    manager_for_shutdown
      .shutdown(ShutdownMode::Graceful)
      .await
      .expect("Graceful shutdown failed");
    info!("Pool shutdown call completed.");
  });

  // Try submitting another task after shutdown initiated (should fail)
  tokio::time::sleep(Duration::from_millis(50)).await; // Ensure shutdown signal propagates
  info!("Attempting to submit task after shutdown initiated...");
  let late_future = Box::pin(async { work_task_fn(99, 1).await });
  match manager.submit(HashSet::new(), late_future).await {
    Ok(_) => tracing::error!("LATE SUBMISSION SUCCEEDED (UNEXPECTED!)"),
    Err(e) => info!("Late submission correctly failed: {:?}", e),
  }

  info!("Awaiting results for initially submitted tasks...");
  // Expected: Tasks 0, 1 complete. Tasks 2, 3, 4 (queued) should not run.
  // Their handles will likely return ResultChannelError or TaskCancelled.
  for handle in handles {
    let task_id = handle.id();
    info!("Awaiting result for task handle {}...", task_id);
    match handle.await_result().await {
      Ok(result) => info!("Task {} result: {}", task_id, result),
      Err(e) => info!("Task {} error (expected for queued tasks): {:?}", task_id, e),
    }
  }

  info!("Awaiting pool shutdown join handle...");
  shutdown_jh.await.expect("Shutdown join handle failed");

  info!("--- Graceful Shutdown Example End ---");
}
