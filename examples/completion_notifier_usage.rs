use futures_orchestra::{
  FuturePoolManager, ShutdownMode, TaskCompletionInfo, TaskCompletionStatus, TaskHandle, TaskLabel,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::info;

// Dummy task function
async fn my_notified_task(id: usize, delay_ms: u64, should_panic: bool) -> String {
  info!(
    "NotifiedTask {}: Starting, will sleep for {}ms. Panic: {}",
    id, delay_ms, should_panic
  );
  tokio::time::sleep(Duration::from_millis(delay_ms)).await;
  if should_panic {
    info!("NotifiedTask {}: Panicking as requested!", id);
    panic!("NotifiedTask {} panicked!", id);
  }
  let result = format!("NotifiedTask {} finished successfully after {}ms", id, delay_ms);
  info!("{}", result);
  result
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_max_level(tracing::Level::INFO) // INFO or DEBUG for example
    .with_target(false)
    .init();

  info!("--- Completion Notifier Example ---");

  let pool_name = "notifier_example_pool";
  let manager = FuturePoolManager::<String>::new(
    2,  // Concurrency limit
    10, // Queue capacity
    Handle::current(),
    pool_name,
  );

  // --- Setup Completion Handlers ---
  let successful_tasks_count = Arc::new(AtomicUsize::new(0));
  let failed_tasks_count = Arc::new(AtomicUsize::new(0)); // Panicked, Cancelled, or Errored

  // Handler 1: Simple logger
  manager.add_completion_handler({
    let pool_name_clone = manager.name().to_string();
    move |info: TaskCompletionInfo| {
      assert_eq!(*info.pool_name, pool_name_clone);
      info!(
        "[Handler 1 - Logger] Task {} (Pool: {}) completed. Status: {:?}, Labels: {:?}, Time: {:?}",
        info.task_id, info.pool_name, info.status, info.labels, info.completion_time
      );
    }
  });

  // Handler 2: Counter
  let s_clone = successful_tasks_count.clone();
  let f_clone = failed_tasks_count.clone();
  manager.add_completion_handler(move |info: TaskCompletionInfo| match info.status {
    TaskCompletionStatus::Success => {
      s_clone.fetch_add(1, Ordering::Relaxed);
      info!("[Handler 2 - Counter] Task {} succeeded.", info.task_id);
    }
    _ => {
      f_clone.fetch_add(1, Ordering::Relaxed);
      info!(
        "[Handler 2 - Counter] Task {} did not succeed (Status: {:?}).",
        info.task_id, info.status
      );
    }
  });

  // --- Submit Tasks ---
  let mut handles: Vec<TaskHandle<String>> = Vec::new();
  let label_critical: TaskLabel = "critical".to_string();
  let label_batch: TaskLabel = "batch_job".to_string();

  // Task 1: Success
  let future1 = Box::pin(async move { my_notified_task(1, 300, false).await });
  let labels1 = HashSet::from_iter([label_critical.clone(), label_batch.clone()]);
  if let Ok(h) = manager.submit(labels1, future1).await {
    handles.push(h);
  }

  // Task 2: Panic
  let future2 = Box::pin(async move { my_notified_task(2, 100, true).await });
  if let Ok(h) = manager.submit(HashSet::new(), future2).await {
    handles.push(h);
  }

  // Task 3: To be cancelled
  let future3 = Box::pin(async move { my_notified_task(3, 2000, false).await });
  let labels3 = HashSet::from_iter([label_batch.clone()]);
  // <<< MODIFIED START [Handle cancellation without cloning TaskHandle] >>>
  let handle_to_cancel = manager.submit(labels3, future3).await.unwrap();
  let id_to_cancel = handle_to_cancel.id(); // Get ID for logging if needed
                                            // We will cancel it before pushing it to the `handles` vector for awaiting,
                                            // or we can push it, then find it to cancel (less direct).
                                            // For simplicity, let's cancel it, then push it.
                                            // OR, more simply, cancel using the `handle_to_cancel` directly and then move it into `handles`.
                                            // The `cancel()` method takes `&self`, so we don't consume it there.
                                            // The `handles.push()` will move it.
                                            // The issue was trying to clone it.

  // The original intent was likely to await it later.
  // So, we submit it, store it for cancellation, then store it again (or its ID) for awaiting.
  // Let's submit, then cancel using the handle we got, then push that same handle.
  // This means `handle_to_cancel` is the one we'll await.
  // The `clone()` was the problem. We just use the same handle.

  // Store the handle for cancellation and later awaiting.
  // No clone needed if we manage the sequence or which variable holds it.
  // The `handle_to_cancel` will be moved into `handles` later.
  // For cancellation, we use it before it's moved.
  // <<< MODIFIED END >>>

  // Task 4: Success (longer)
  let future4 = Box::pin(async move { my_notified_task(4, 600, false).await });
  if let Ok(h) = manager.submit(HashSet::new(), future4).await {
    handles.push(h);
  }

  info!("All tasks submitted. Some will run, one will be cancelled.");
  tokio::time::sleep(Duration::from_millis(150)).await; // Let some tasks start/progress

  // <<< MODIFIED [Cancel the specific handle before it's moved or if it's the one we're tracking] >>>
  info!("Cancelling task {} (handle specific)", id_to_cancel); // Use stored id_to_cancel
  handle_to_cancel.cancel(); // Call cancel on the handle we have

  // Now, push the (now cancellation-requested) handle_to_cancel into the main list for awaiting its outcome.
  handles.push(handle_to_cancel);
  // <<< MODIFIED END >>>

  // --- Await Task Results (Optional, but good practice) ---
  info!("Awaiting all task results from handles...");
  for handle in handles {
    // This loop now correctly consumes each handle
    let task_id = handle.id();
    match handle.await_result().await {
      Ok(result) => info!("Main: Result for task {}: {}", task_id, result),
      Err(e) => info!("Main: Error for task {}: {:?}", task_id, e),
    }
  }

  // --- Shutdown and Summary ---
  info!("All task handles awaited. Shutting down pool gracefully...");
  manager
    .shutdown(ShutdownMode::Graceful) // Graceful to let notifier process
    .await
    .expect("Pool shutdown failed");
  info!("Pool shutdown complete.");

  info!("--- Summary from Completion Notifier ---");
  info!(
    "Successful tasks (counted by handler): {}",
    successful_tasks_count.load(Ordering::Relaxed)
  );
  info!(
    "Non-successful tasks (counted by handler): {}",
    failed_tasks_count.load(Ordering::Relaxed)
  );
  info!("--- Completion Notifier Example End ---");
}
