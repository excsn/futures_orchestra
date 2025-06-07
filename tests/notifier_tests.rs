use futures_orchestra::{
  FuturePoolManager, PoolError, ShutdownMode, TaskCompletionInfo, TaskCompletionStatus, TaskHandle,
  TaskLabel, TaskToExecute,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering}; // Removed AtomicUsize as it's not used in these tests directly
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle as TokioHandle; // Added for FuturePoolManager::new
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing; // For logging in tests

// Helper to create a task future (copied from pool_tests.rs for standalone notifier tests)
fn create_task(
  task_id_for_log: usize,
  duration_ms: u64,
  output_value: String,
  should_panic: bool,
  task_internal_cancel_token: Option<CancellationToken>,
  completion_flag: Option<Arc<AtomicBool>>, // Kept for compatibility, though not always used here
) -> TaskToExecute<String> {
  Box::pin(async move {
    let internal_token = task_internal_cancel_token.unwrap_or_else(CancellationToken::new);
    let check_interval_ms = 10u64;
    let mut intervals_passed = 0u64;

    // Cooperative cancellation check loop
    let target_intervals = duration_ms / check_interval_ms;
    while intervals_passed < target_intervals {
      if internal_token.is_cancelled() {
        tracing::info!(
          "Task {} (notifier test context) cancelled internally by its own token check.",
          task_id_for_log
        );
        // This return path means the task completed with a specific outcome,
        // not necessarily a `PoolError::TaskCancelled` from the pool's perspective,
        // unless the main task token was also cancelled and won the select race.
        return format!("task_{}_cancelled_internally_by_task", task_id_for_log);
      }
      sleep(Duration::from_millis(check_interval_ms)).await;
      intervals_passed += 1;
    }
    // Handle remaining duration if duration_ms is not a multiple of check_interval_ms
    let remaining_ms = duration_ms % check_interval_ms;
    if remaining_ms > 0 && !internal_token.is_cancelled() {
        sleep(Duration::from_millis(remaining_ms)).await;
    }


    if should_panic {
      tracing::info!(
        "Task {} (notifier test context) panicking as requested.",
        task_id_for_log
      );
      panic!("Task {} (notifier test context) intentionally panicked!", task_id_for_log);
    }

    if let Some(flag) = completion_flag {
      flag.store(true, Ordering::SeqCst);
    }
    tracing::info!(
      "Task {} (notifier test context) completed successfully.",
      task_id_for_log
    );
    output_value
  })
}

// Helper to initialize tracing for tests
fn setup_tracing_for_test() {
  use std::sync::Once;
  use tracing_subscriber::{fmt, util::SubscriberInitExt, EnvFilter};
  static TRACING_INIT: Once = Once::new();

  TRACING_INIT.call_once(|| {
    let filter = EnvFilter::try_from_default_env()
      .unwrap_or_else(|_| EnvFilter::new("info,futures_orchestra=trace")); // Default if RUST_LOG not set
    fmt::Subscriber::builder()
      .with_env_filter(filter)
      .with_test_writer() // Suitable for `cargo test`
      .try_init()
      .ok();
  });
}

// Helper for collecting notifications in tests
fn create_collecting_handler() -> (
  Arc<Mutex<Vec<TaskCompletionInfo>>>,
  impl Fn(TaskCompletionInfo) + Send + Sync + 'static,
) {
  let collected_notifications = Arc::new(Mutex::new(Vec::new()));
  let collected_notifications_clone = collected_notifications.clone();
  let handler = move |info: TaskCompletionInfo| {
    tracing::debug!(
      "Test Collecting Handler (Notifier Test): Received notification for task_id: {}, status: {:?}",
      info.task_id,
      info.status
    );
    let mut guard = collected_notifications_clone.lock().unwrap();
    guard.push(info);
  };
  (collected_notifications, handler)
}

#[tokio::test]
async fn test_completion_notifier_success() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_success";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 1, TokioHandle::current(), pool_name);
  let (notifications, handler) = create_collecting_handler();
  manager.add_completion_handler(handler);

  let labels = HashSet::from_iter(["label1".to_string(), "success_tag".to_string()].iter().cloned());
  let task_future = create_task(10, 50, "success_val".to_string(), false, None, None);
  let handle = manager.submit(labels.clone(), task_future).await.unwrap();
  let task_id = handle.id();

  assert_eq!(handle.await_result().await, Ok("success_val".to_string()));
  // Short sleep to allow notifier to process if it's slightly delayed, then shutdown
  sleep(Duration::from_millis(10)).await;
  manager.shutdown(ShutdownMode::Graceful).await.unwrap();

  let notifs = notifications.lock().unwrap();
  assert_eq!(notifs.len(), 1);
  let info = &notifs[0];
  assert_eq!(info.task_id, task_id);
  assert_eq!(*info.pool_name, pool_name);
  assert_eq!(*info.labels, labels);
  assert_eq!(info.status, TaskCompletionStatus::Success);
  assert!(info.completion_time <= std::time::SystemTime::now());
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_panic() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_panic";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 1, TokioHandle::current(), pool_name);
  let (notifications, handler) = create_collecting_handler();
  manager.add_completion_handler(handler);

  let labels = HashSet::from_iter(["panic_label".to_string()].iter().cloned());
  let task_future = create_task(20, 50, "panic_wont_return".to_string(), true, None, None);
  let handle = manager.submit(labels.clone(), task_future).await.unwrap();
  let task_id = handle.id();

  match handle.await_result().await {
    Err(PoolError::TaskPanicked) => {}
    res => panic!("Expected TaskPanicked, got {:?}", res),
  }
  sleep(Duration::from_millis(10)).await;
  manager.shutdown(ShutdownMode::Graceful).await.unwrap();

  let notifs = notifications.lock().unwrap();
  assert_eq!(notifs.len(), 1);
  let info = &notifs[0];
  assert_eq!(info.task_id, task_id);
  assert_eq!(*info.pool_name, pool_name);
  assert_eq!(*info.labels, labels);
  assert_eq!(info.status, TaskCompletionStatus::Panicked);
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_cancelled_by_handle() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_cancel_handle";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 1, TokioHandle::current(), pool_name);
  let (notifications, handler) = create_collecting_handler();
  manager.add_completion_handler(handler);

  let labels = HashSet::from_iter(["cancel_handle_label".to_string()].iter().cloned());
  let task_future = create_task(30, 1000, "cancel_wont_return".to_string(), false, None, None);
  let handle = manager.submit(labels.clone(), task_future).await.unwrap();
  let task_id = handle.id();

  sleep(Duration::from_millis(20)).await; // Let task start
  handle.cancel();

  match handle.await_result().await {
    Err(PoolError::TaskCancelled) => {}
    res => panic!("Expected TaskCancelled, got {:?}", res),
  }
  sleep(Duration::from_millis(10)).await;
  manager.shutdown(ShutdownMode::Graceful).await.unwrap();

  let notifs = notifications.lock().unwrap();
  assert_eq!(notifs.len(), 1);
  let info = &notifs[0];
  assert_eq!(info.task_id, task_id);
  assert_eq!(*info.pool_name, pool_name);
  assert_eq!(*info.labels, labels);
  assert_eq!(info.status, TaskCompletionStatus::Cancelled);
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_cancelled_by_label() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_cancel_label";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 1, TokioHandle::current(), pool_name);
  let (notifications, handler) = create_collecting_handler();
  manager.add_completion_handler(handler);

  let label_to_cancel: TaskLabel = "cancel_this_label_notifier".to_string();
  let labels = HashSet::from_iter([label_to_cancel.clone()].iter().cloned());
  let task_future = create_task(40, 1000, "cancel_wont_return_l".to_string(), false, None, None);
  let handle = manager.submit(labels.clone(), task_future).await.unwrap();
  let task_id = handle.id();

  sleep(Duration::from_millis(20)).await; // Let task start
  manager.cancel_tasks_by_label(&label_to_cancel);

  match handle.await_result().await {
    Err(PoolError::TaskCancelled) => {}
    res => panic!("Expected TaskCancelled, got {:?}", res),
  }
  sleep(Duration::from_millis(10)).await;
  manager.shutdown(ShutdownMode::Graceful).await.unwrap();

  let notifs = notifications.lock().unwrap();
  assert_eq!(notifs.len(), 1);
  let info = &notifs[0];
  assert_eq!(info.task_id, task_id);
  assert_eq!(*info.pool_name, pool_name);
  assert_eq!(*info.labels, labels);
  assert_eq!(info.status, TaskCompletionStatus::Cancelled);
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_task_dequeued_but_pre_cancelled() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_pre_cancelled_dequeued";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 5, TokioHandle::current(), pool_name); // Concurrency 1
  let (notifications, handler) = create_collecting_handler();
  manager.add_completion_handler(handler);

  // Task A to occupy the permit
  let task_a_labels = HashSet::from_iter(["task_a_label".to_string()]);
  let task_a_future = create_task(51, 200, "task_a_done".to_string(), false, None, None);
  let handle_a = manager.submit(task_a_labels.clone(), task_a_future).await.unwrap();
  let task_a_id = handle_a.id();

  // Task B, submitted then immediately cancelled, will be queued.
  let labels_b = HashSet::from_iter(["pre_cancel_label".to_string()].iter().cloned());
  let task_b_future = create_task(50, 1000, "task_b_wont_run".to_string(), false, None, None);
  let handle_b = manager.submit(labels_b.clone(), task_b_future).await.unwrap();
  let task_b_id = handle_b.id();

  handle_b.cancel(); // Cancel task B while it's likely still in the queue

  // Await A to ensure B gets a chance to be dequeued (and found to be cancelled)
  assert_eq!(handle_a.await_result().await, Ok("task_a_done".to_string()));

  // Await B
  match handle_b.await_result().await {
    Err(PoolError::TaskCancelled) => {} // Expected since we cancelled it
    res => panic!("Expected TaskCancelled for task B, got {:?}", res),
  }
  sleep(Duration::from_millis(50)).await; // Allow time for notifications to be processed
  manager.shutdown(ShutdownMode::Graceful).await.unwrap();

  let notifs = notifications.lock().unwrap();
  assert_eq!(notifs.len(), 2, "Expected notifications for task A and B");

  let info_a = notifs
    .iter()
    .find(|n| n.task_id == task_a_id)
    .expect("Notification for task A not found");
  assert_eq!(info_a.status, TaskCompletionStatus::Success);
  assert_eq!(*info_a.labels, task_a_labels);


  let info_b = notifs
    .iter()
    .find(|n| n.task_id == task_b_id)
    .expect("Notification for task B not found");
  assert_eq!(info_b.status, TaskCompletionStatus::Cancelled);
  assert_eq!(*info_b.labels, labels_b);
  
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_multiple_handlers() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_multi_handler";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 1, TokioHandle::current(), pool_name);

  let (notifications1, handler1) = create_collecting_handler();
  let (notifications2, handler2) = create_collecting_handler();
  manager.add_completion_handler(handler1);
  manager.add_completion_handler(handler2);

  let task_future = create_task(60, 50, "multi_handler_val".to_string(), false, None, None);
  let handle = manager.submit(HashSet::new(), task_future).await.unwrap();
  let task_id = handle.id();

  assert_eq!(handle.await_result().await, Ok("multi_handler_val".to_string()));
  sleep(Duration::from_millis(10)).await;
  manager.shutdown(ShutdownMode::Graceful).await.unwrap();

  let notifs1 = notifications1.lock().unwrap();
  assert_eq!(notifs1.len(), 1);
  assert_eq!(notifs1[0].task_id, task_id);
  assert_eq!(notifs1[0].status, TaskCompletionStatus::Success);

  let notifs2 = notifications2.lock().unwrap();
  assert_eq!(notifs2.len(), 1);
  assert_eq!(notifs2[0].task_id, task_id);
  assert_eq!(notifs2[0].status, TaskCompletionStatus::Success);
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_handler_panics() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_handler_panic";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 1, TokioHandle::current(), pool_name);

  let (notifications_collect, collecting_handler) = create_collecting_handler();
  let panicking_handler = |_info: TaskCompletionInfo| {
    panic!("Intentional panic in completion handler for test_notifier_handler_panic!");
  };

  manager.add_completion_handler(panicking_handler); // Add panicking handler first
  manager.add_completion_handler(collecting_handler); // Add normal handler second

  let task_future = create_task(70, 50, "handler_panic_test_val".to_string(), false, None, None);
  let handle = manager.submit(HashSet::new(), task_future).await.unwrap();
  let task_id = handle.id();

  assert_eq!(handle.await_result().await, Ok("handler_panic_test_val".to_string()));
  sleep(Duration::from_millis(50)).await; // Give time for handlers to run (and one to panic)
  manager.shutdown(ShutdownMode::Graceful).await.unwrap();

  let collected = notifications_collect.lock().unwrap();
  assert_eq!(collected.len(), 1, "Collecting handler should still have received the notification.");
  assert_eq!(collected[0].task_id, task_id);
  assert_eq!(collected[0].status, TaskCompletionStatus::Success);
  // Also, check logs for the panic from the other handler (manual step or advanced logging capture)
  tracing::info!("Finished test: {}. Check logs for handler panic.", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_during_graceful_shutdown() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_graceful_shutdown";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 5, TokioHandle::current(), pool_name); // Concurrency 1
  let (notifications, handler) = create_collecting_handler();
  manager.add_completion_handler(handler);

  // Task A (active, will complete)
  let task_a_future = create_task(80, 300, "task_a_graceful".to_string(), false, None, None);
  let handle_a = manager.submit(HashSet::new(), task_a_future).await.unwrap();
  let task_a_id = handle_a.id();

  // Task B (queued, will not run)
  let task_b_future = create_task(81, 50, "task_b_queued_graceful".to_string(), false, None, None);
  let handle_b = manager.submit(HashSet::new(), task_b_future).await.unwrap();
  // let task_b_id = handle_b.id(); // Not needed for notification check as it won't be notified

  sleep(Duration::from_millis(50)).await; // Ensure Task A starts
  assert_eq!(manager.active_task_count(), 1);
  assert_eq!(manager.queued_task_count(), 1);

  manager.shutdown(ShutdownMode::Graceful).await.unwrap(); // manager is consumed

  // Check handle results
  assert_eq!(handle_a.await_result().await, Ok("task_a_graceful".to_string()));
  match handle_b.await_result().await {
    Err(PoolError::ResultChannelError(_)) | Err(PoolError::TaskCancelled) => {} // Expected for unstarted queued task
    res => panic!("Task B (queued): Expected ResultChannelError or TaskCancelled, got {:?}", res),
  }

  // Check notifications
  let notifs = notifications.lock().unwrap();
  assert_eq!(notifs.len(), 1, "Only task A should have a completion notification.");
  assert_eq!(notifs[0].task_id, task_a_id);
  assert_eq!(notifs[0].status, TaskCompletionStatus::Success);
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_during_forceful_shutdown() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_forceful_shutdown";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 5, TokioHandle::current(), pool_name); // Concurrency 1
  let (notifications, handler) = create_collecting_handler();
  manager.add_completion_handler(handler);

  // Task A (active, will be cancelled)
  let task_a_future = create_task(90, 2000, "task_a_forceful".to_string(), false, None, None);
  let handle_a = manager.submit(HashSet::new(), task_a_future).await.unwrap();
  let task_a_id = handle_a.id();

  // Task B (queued, will not run)
  let task_b_future = create_task(91, 50, "task_b_queued_forceful".to_string(), false, None, None);
  let handle_b = manager.submit(HashSet::new(), task_b_future).await.unwrap();

  sleep(Duration::from_millis(50)).await; // Ensure Task A starts
  assert_eq!(manager.active_task_count(), 1);
  assert_eq!(manager.queued_task_count(), 1);

  manager.shutdown(ShutdownMode::ForcefulCancel).await.unwrap();

  // Check handle results
  match handle_a.await_result().await {
    Err(PoolError::TaskCancelled) => {} // Expected for forcefully cancelled active task
    res => panic!("Task A (active): Expected TaskCancelled, got {:?}", res),
  }
  match handle_b.await_result().await {
    Err(PoolError::ResultChannelError(_)) | Err(PoolError::TaskCancelled) => {} // Expected for unstarted queued task
    res => panic!("Task B (queued): Expected ResultChannelError or TaskCancelled, got {:?}", res),
  }

  // Check notifications
  let notifs = notifications.lock().unwrap();
  assert_eq!(notifs.len(), 1, "Only task A should have a completion notification.");
  assert_eq!(notifs[0].task_id, task_a_id);
  assert_eq!(notifs[0].status, TaskCompletionStatus::Cancelled);
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_no_handlers_added() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_no_handlers";
  tracing::info!("Starting test: {}", pool_name);
  let manager =
    FuturePoolManager::<String>::new(1, 1, TokioHandle::current(), pool_name);
  // No handlers added

  let task_future = create_task(100, 50, "no_handler_val".to_string(), false, None, None);
  let handle = manager.submit(HashSet::new(), task_future).await.unwrap();

  assert_eq!(handle.await_result().await, Ok("no_handler_val".to_string()));
  // Pool should operate normally and not panic due to lack of handlers
  manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  tracing::info!("Finished test: {}. Pool operated normally without handlers.", pool_name);
}

#[tokio::test]
async fn test_completion_notifier_pool_name_and_labels_in_info() {
  setup_tracing_for_test();
  let pool_name = "test_notifier_info_details";
  let manager = FuturePoolManager::<String>::new(1, 1, TokioHandle::current(), pool_name);
  let (notifications, handler) = create_collecting_handler();
  manager.add_completion_handler(handler);

  let expected_labels = HashSet::from_iter([
    "detail_label_1".to_string(),
    "detail_label_2".to_string(),
  ]);

  let task_future = create_task(110, 50, "details_val".to_string(), false, None, None);
  let handle = manager.submit(expected_labels.clone(), task_future).await.unwrap();
  let task_id = handle.id();

  assert_eq!(handle.await_result().await, Ok("details_val".to_string()));
  sleep(Duration::from_millis(10)).await;
  manager.shutdown(ShutdownMode::Graceful).await.unwrap();

  let notifs = notifications.lock().unwrap();
  assert_eq!(notifs.len(), 1);
  let info = &notifs[0];

  assert_eq!(info.task_id, task_id);
  assert_eq!(*info.pool_name, pool_name.to_string(), "Pool name in notification should match.");
  assert_eq!(*info.labels, expected_labels, "Labels in notification should match.");
  assert_eq!(info.status, TaskCompletionStatus::Success);
}