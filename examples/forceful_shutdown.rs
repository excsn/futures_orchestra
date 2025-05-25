use futures_orchestra::{FuturePoolManager, PoolError, ShutdownMode, TaskHandle};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::info;

async fn long_running_interruptible_task(id: usize, duration_s: u64) -> String {
  info!(
    "Task {} starting (potentially long: {}s), checking for cancellation.",
    id, duration_s
  );
  // This task simulates work that can be interrupted if the driving future is cancelled.
  // The pool's `select!` on the task future and its cancellation token handles this.
  tokio::time::sleep(Duration::from_secs(duration_s)).await;
  let result = format!(
    "Task {} completed NORMALLY after {}s (should be rare in forceful shutdown)",
    id, duration_s
  );
  info!("{}", result);
  result
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .with_target(false)
    .init();
  info!("--- Forceful Shutdown Example ---");

  let manager = FuturePoolManager::<String>::new(
    2, // Concurrency limit of 2
    10,
    Handle::current(),
    "forceful_shutdown_pool",
  );

  let mut handles: Vec<TaskHandle<String>> = Vec::new();

  // Submit 5 tasks, each "takes" 5 seconds.
  for i in 0..5 {
    let future = Box::pin(async move { long_running_interruptible_task(i, 5).await });
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
  info!("Initiating FORCEFUL shutdown shortly...");
  tokio::time::sleep(Duration::from_millis(200)).await; // Let some tasks start (e.g., task 0 and 1)

  let manager_for_shutdown = manager.clone();
  let shutdown_jh = tokio::spawn(async move {
    info!("Calling pool.shutdown(ForcefulCancel)...");
    manager_for_shutdown
      .shutdown(ShutdownMode::ForcefulCancel)
      .await
      .expect("Forceful shutdown failed");
    info!("Pool shutdown call completed.");
  });

  // Try submitting another task after shutdown initiated (should fail)
  tokio::time::sleep(Duration::from_millis(50)).await; // Ensure shutdown signal propagates
  info!("Attempting to submit task after forceful shutdown initiated...");
  let late_future = Box::pin(async { long_running_interruptible_task(99, 1).await });
  match manager.submit(HashSet::new(), late_future).await {
    Ok(_) => tracing::error!("LATE SUBMISSION SUCCEEDED (UNEXPECTED!)"),
    Err(e) => info!("Late submission correctly failed: {:?}", e),
  }

  info!("Awaiting results for initially submitted tasks...");
  // Expected:
  // - Active tasks (0, 1) should be cancelled (PoolError::TaskCancelled).
  // - Queued tasks (2, 3, 4) should also likely result in PoolError::TaskCancelled or ResultChannelError.
  for handle in handles {
    let task_id = handle.id();
    info!("Awaiting result for task handle {}...", task_id);
    match handle.await_result().await {
      Ok(result) => info!("Task {} result (UNEXPECTED for active tasks): {}", task_id, result),
      Err(PoolError::TaskCancelled) => info!(
        "Task {} correctly cancelled (expected for active/queued tasks).",
        task_id
      ),
      Err(e) => info!(
        "Task {} error: {:?} (possibly expected for queued tasks if not TaskCancelled)",
        task_id, e
      ),
    }
  }

  info!("Awaiting pool shutdown join handle...");
  shutdown_jh.await.expect("Shutdown join handle failed");

  info!("--- Forceful Shutdown Example End ---");
}
