use futures_orchestra::{FuturePoolManager, PoolError, ShutdownMode};
use std::collections::HashSet;
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::{info, warn};

// The task itself is simple, cancellation is managed by the pool's select!
// The type R for the pool is Result<String, String> as an example of a fallible task.
async fn example_task_future(id_str: &str, work_duration_s: u64, succeed: bool) -> Result<String, String> {
  info!(
    "Task ({}) starting, work for {}s. Will it succeed? {}",
    id_str, work_duration_s, succeed
  );
  tokio::time::sleep(Duration::from_secs(work_duration_s)).await;
  if succeed {
    let result = format!("Task ({}) finished normally.", id_str);
    info!("{}", result);
    Ok(result)
  } else {
    let error_msg = format!("Task ({}) failed intentionally.", id_str);
    warn!("{}", error_msg);
    Err(error_msg)
  }
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .with_target(false)
    .init();
  info!("--- Individual Cancellation Example ---");

  // R is Result<String, String>
  let manager = FuturePoolManager::<Result<String, String>>::new(
    2,  // Concurrency
    10, // Queue capacity
    Handle::current(),
    "cancellation_pool",
  );

  // Task that will be cancelled
  let future_to_cancel = Box::pin(
    example_task_future("to_be_cancelled", 5, true), // 5s work, set to succeed if not cancelled
  );

  let handle_to_cancel = manager
    .submit(HashSet::new(), future_to_cancel)
    .await
    .expect("Failed to submit task to be cancelled");
  info!("Submitted task {} (intended for cancellation).", handle_to_cancel.id());

  // Task that will complete
  let future_to_complete = Box::pin(
    example_task_future("to_complete", 1, true), // 1s work, set to succeed
  );
  let handle_to_complete = manager
    .submit(HashSet::new(), future_to_complete)
    .await
    .expect("Failed to submit task to complete");
  info!("Submitted task {} (intended for completion).", handle_to_complete.id());

  info!("Waiting 500ms before cancelling task {}.", handle_to_cancel.id());
  tokio::time::sleep(Duration::from_millis(500)).await;

  info!("Requesting cancellation for task {}.", handle_to_cancel.id());
  handle_to_cancel.cancel(); // Request cancellation for this specific handle

  // Await results

  let task_id_cancelled = handle_to_cancel.id(); // Get ID before consuming handle
  info!(
    "Awaiting result for task {} (should be PoolError::TaskCancelled).",
    task_id_cancelled
  );
  match handle_to_cancel.await_result().await {
    // handle_to_cancel consumed here
    Ok(task_outcome_result) => {
      // task_outcome_result is Result<String, String>
      // This path means the task was not cancelled by the pool's mechanism and it completed.
      match task_outcome_result {
        Ok(s) => warn!(
          "Task {} completed with SUCCESS (UNEXPECTED for cancelled task): {:?}",
          task_id_cancelled, s
        ),
        Err(e_str) => warn!(
          "Task {} completed with an INNER ERROR (UNEXPECTED for cancelled task): {:?}",
          task_id_cancelled, e_str
        ),
      }
    }
    Err(PoolError::TaskCancelled) => {
      info!(
        "Task {} correctly resulted in PoolError::TaskCancelled.",
        task_id_cancelled
      );
    }
    Err(e) => {
      // Other PoolError
      warn!(
        "Task {} (cancelled) resulted in unexpected PoolError: {:?}",
        task_id_cancelled, e
      );
    }
  }

  let task_id_complete = handle_to_complete.id(); // Get ID before consuming handle
  info!(
    "Awaiting result for task {} (should complete normally).",
    task_id_complete
  );
  match handle_to_complete.await_result().await {
    // handle_to_complete consumed here
    Ok(task_outcome_result) => {
      // task_outcome_result is Result<String, String>
      match task_outcome_result {
        Ok(s) => info!("Task {} completed with SUCCESS: {:?}", task_id_complete, s),
        Err(e_str) => warn!(
          // This task was set to succeed, so inner error is also notable
          "Task {} completed with an INNER ERROR: {:?}",
          task_id_complete, e_str
        ),
      }
    }
    Err(e) => {
      // PoolError
      warn!(
        "Task {} (meant to complete) resulted in PoolError: {:?}",
        task_id_complete, e
      );
    }
  }

  info!("Shutting down pool.");
  manager
    .shutdown(ShutdownMode::Graceful)
    .await
    .expect("Pool shutdown failed");
  info!("Pool shutdown complete.");
  info!("--- Individual Cancellation Example End ---");
}
