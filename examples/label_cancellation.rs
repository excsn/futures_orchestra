use futures_orchestra::{FuturePoolManager, PoolError, ShutdownMode, TaskHandle, TaskLabel};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tracing::info;

async fn labelled_task_fn(id: usize, task_labels: Vec<String>) -> String {
  // This task just sleeps. Cancellation is handled by the pool's select!.
  info!(
    "Labelled Task {} ({:?}) starting, will sleep for 5s unless cancelled.",
    id, task_labels
  );
  tokio::time::sleep(Duration::from_secs(5)).await;
  let result = format!(
    "Labelled Task {} ({:?}) finished normally (should only happen for non-cancelled).",
    id, task_labels
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
  info!("--- Label Cancellation Example ---");

  let manager = FuturePoolManager::<String>::new(
    4, // Allow multiple tasks to run to see cancellation effect
    10,
    Handle::current(),
    "label_pool",
  );

  let mut handles: Vec<TaskHandle<String>> = Vec::new();
  let label_a: TaskLabel = "group_a".to_string();
  let label_b: TaskLabel = "group_b".to_string();

  // Tasks for group_a (to be cancelled)
  for i in 0..2 {
    let labels = HashSet::from_iter([label_a.clone()]);
    let task_labels_for_fn = labels.iter().cloned().collect();
    let future = Box::pin(async move { labelled_task_fn(i, task_labels_for_fn).await });
    handles.push(manager.submit(labels, future).await.unwrap());
    info!("Submitted task {} with label 'group_a'", i);
  }

  // Tasks for group_b (should complete)
  for i in 2..4 {
    let labels = HashSet::from_iter([label_b.clone()]);
    let task_labels_for_fn = labels.iter().cloned().collect();
    let future = Box::pin(async move { labelled_task_fn(i, task_labels_for_fn).await });
    handles.push(manager.submit(labels, future).await.unwrap());
    info!("Submitted task {} with label 'group_b'", i);
  }

  info!("All tasks submitted. Waiting 500ms before cancelling 'group_a'.");
  tokio::time::sleep(Duration::from_millis(500)).await;

  info!("Cancelling tasks with label '{}'", label_a);
  manager.cancel_tasks_by_label(&label_a);

  info!("Awaiting all task results...");
  for handle in handles {
    let task_id = handle.id();
    let task_labels = handle.labels();
    match handle.await_result().await {
      Ok(result) => info!("Task {} ({:?}) result: {}", task_id, task_labels, result),
      Err(PoolError::TaskCancelled) => {
        info!("Task {} ({:?}) was correctly cancelled.", task_id, task_labels);
      }
      Err(e) => info!("Task {} ({:?}) error: {:?}", task_id, task_labels, e),
    }
  }

  info!("Shutting down pool.");
  manager
    .shutdown(ShutdownMode::Graceful)
    .await
    .expect("Pool shutdown failed");
  info!("Pool shutdown complete.");
  info!("--- Label Cancellation Example End ---");
}
