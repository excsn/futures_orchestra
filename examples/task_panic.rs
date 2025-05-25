use futures_orchestra::{FuturePoolManager, PoolError, ShutdownMode};
use std::collections::HashSet;
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::info;

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .with_target(false)
    .init();
  info!("--- Task Panic Example ---");

  let manager = FuturePoolManager::<String>::new(
    1, // Concurrency limit
    5, // Queue capacity
    Handle::current(),
    "panic_pool",
  );

  let panicking_future = Box::pin(async {
    info!("Panicking Task: Starting...");
    tokio::time::sleep(Duration::from_millis(100)).await;
    info!("Panicking Task: About to panic!");
    panic!("This task is designed to panic!");
    #[allow(unreachable_code)]
    "This will not be returned".to_string()
  });

  let handle = manager
    .submit(HashSet::new(), panicking_future)
    .await
    .expect("Failed to submit panicking task");

  let task_id = handle.id(); // Get the ID before handle is consumed
  info!("Panicking task {} submitted. Awaiting result...", task_id);

  match handle.await_result().await {
    // handle is consumed here
    Ok(result) => info!("Task {} completed with UNEXPECTED result: {}", task_id, result),
    Err(PoolError::TaskPanicked) => {
      info!("Task {} correctly resulted in PoolError::TaskPanicked.", task_id);
    }
    Err(e) => info!("Task {} resulted in unexpected error: {:?}", task_id, e),
  }

  info!("Shutting down pool.");
  manager
    .shutdown(ShutdownMode::Graceful)
    .await
    .expect("Pool shutdown failed");
  info!("Pool shutdown complete.");
  info!("--- Task Panic Example End ---");
}
