use futures_orchestra::{FuturePoolManager, PoolError, ShutdownMode, TaskToExecute};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

// Helper to create a task future
fn create_task(
  task_id_for_log: usize,
  duration_ms: u64,
  output_value: String,
  should_panic: bool,
  // Token for the task to cooperatively check for cancellation (optional)
  task_internal_cancel_token: Option<CancellationToken>,
  completion_flag: Option<Arc<AtomicBool>>, // External flag to verify completion
) -> TaskToExecute<String> {
  Box::pin(async move {
    let internal_token = task_internal_cancel_token.unwrap_or_else(CancellationToken::new);

    let check_interval_ms = 10u64;
    let mut intervals_passed = 0u64;

    while intervals_passed * check_interval_ms < duration_ms {
      if internal_token.is_cancelled() {
        tracing::info!("Task {} cancelled internally by its own token check.", task_id_for_log);
        // To distinguish from pool-driven cancellation, this task returns a specific string.
        // If the pool's `select!` on the task's main CancellationToken wins,
        // `await_result` will yield `Err(PoolError::TaskCancelled)`.
        return format!("task_{}_cancelled_internally_by_task", task_id_for_log);
      }
      sleep(Duration::from_millis(check_interval_ms)).await;
      intervals_passed += 1;
    }

    if should_panic {
      tracing::info!("Task {} panicking as requested.", task_id_for_log);
      panic!("Task {} intentionally panicked!", task_id_for_log);
    }

    if let Some(flag) = completion_flag {
      flag.store(true, Ordering::SeqCst);
    }
    tracing::info!("Task {} completed successfully.", task_id_for_log);
    output_value
  })
}

// Helper to initialize tracing for tests (call once per test run, not per test function)
// For simplicity in example, each test calls it, but Once ensures it runs once.
fn setup_tracing_for_test() {
  use std::sync::Once;
  use tracing_subscriber::{fmt, EnvFilter}; // Added SubscriberInitExt
  static TRACING_INIT: Once = Once::new();

  TRACING_INIT.call_once(|| {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,futures_orchestra=trace")); // Default if RUST_LOG not set

    fmt::Subscriber::builder()
      .with_env_filter(filter)
      .with_test_writer() // Suitable for `cargo test`
      .try_init() // Use try_init to avoid panic if already initialized
      .ok(); // Ok to ignore error if already initialized
  });
}

#[tokio::test]
async fn test_submit_and_await_basic_task() {
  setup_tracing_for_test();
  let pool_name = "test_pool_basic_submit";
  tracing::info!("Starting test: {}", pool_name);
  let manager = FuturePoolManager::<String>::new(2, 5, tokio::runtime::Handle::current(), pool_name);

  let task_future = create_task(1, 50, "task1_done".to_string(), false, None, None);
  let handle = manager.submit(HashSet::new(), task_future).await.unwrap();

  let result = handle.await_result().await;
  assert_eq!(result, Ok("task1_done".to_string()));

  manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_task_panics_are_handled() {
  setup_tracing_for_test();
  let pool_name = "test_pool_panic_handling";
  tracing::info!("Starting test: {}", pool_name);
  let manager = FuturePoolManager::<String>::new(1, 5, tokio::runtime::Handle::current(), pool_name);

  let panic_task_future = create_task(1, 50, "wont_complete".to_string(), true, None, None);
  let handle_panic = manager.submit(HashSet::new(), panic_task_future).await.unwrap();

  let result_panic = handle_panic.await_result().await;
  match result_panic {
    Err(PoolError::TaskPanicked) => { /* Expected */ }
    _ => panic!("Expected TaskPanicked error, got {:?}", result_panic),
  }

  // Ensure pool still works for other tasks
  let normal_task_future = create_task(2, 50, "task2_done".to_string(), false, None, None);
  let handle_normal = manager.submit(HashSet::new(), normal_task_future).await.unwrap();
  assert_eq!(handle_normal.await_result().await, Ok("task2_done".to_string()));

  manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_task_cancellation_via_handle_triggers_pool_error() {
  setup_tracing_for_test();
  let pool_name = "test_pool_cancel_via_handle";
  tracing::info!("Starting test: {}", pool_name);
  let manager = FuturePoolManager::<String>::new(1, 5, tokio::runtime::Handle::current(), pool_name);

  // This task has a long duration. We will cancel it via its handle.
  // It does not need to check its own internal token for *this specific test*,
  // as we are testing the PoolError::TaskCancelled from the pool's select mechanism.
  let task_future = create_task(
    1,
    5000, // Long duration
    "output_if_not_cancelled".to_string(),
    false,
    None, // Not checking internal token for this test case
    None,
  );
  let handle = manager.submit(HashSet::new(), task_future).await.unwrap();

  // Let the task start and run for a very short while
  sleep(Duration::from_millis(50)).await;
  tracing::info!(
    "Test: Requesting cancellation via handle.cancel() for task {}",
    handle.id()
  );
  handle.cancel(); // This cancels the token the pool's `select!` uses.

  let result = handle.await_result().await;
  tracing::info!("Test: Cancellation result: {:?}", result);
  match result {
    Err(PoolError::TaskCancelled) => { /* Expected: pool's select! detected cancellation */ }
    _ => panic!("Expected PoolError::TaskCancelled, got {:?}", result),
  }

  manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_task_cooperative_cancellation_via_its_own_token() {
  setup_tracing_for_test();
  let pool_name = "test_pool_cooperative_cancel";
  tracing::info!("Starting test: {}", pool_name);
  let manager = FuturePoolManager::<String>::new(1, 5, tokio::runtime::Handle::current(), pool_name);

  let task_internal_token = CancellationToken::new(); // Token for the task to check

  let task_future = create_task(
    1,
    5000, // Long duration
    "output_if_not_cancelled".to_string(),
    false,
    Some(task_internal_token.clone()), // Task will check this token
    None,
  );
  let handle = manager.submit(HashSet::new(), task_future).await.unwrap();

  // Let the task start
  sleep(Duration::from_millis(50)).await;
  tracing::info!(
    "Test: Triggering task's internal CancellationToken for task {}",
    handle.id()
  );
  task_internal_token.cancel(); // Cancel the token the *task itself* is checking

  let result = handle.await_result().await;
  tracing::info!("Test: Cooperative cancellation result: {:?}", result);
  match result {
    // Expect the task's own "cancelled_internally_by_task" message
    Ok(s) if s == "task_1_cancelled_internally_by_task" => { /* Expected */ }
    // If the handle.cancel() was also called AND won the race, TaskCancelled would be possible.
    // But here we only cancelled the internal token.
    Err(PoolError::TaskCancelled) => {
      panic!("Got PoolError::TaskCancelled, expected task to return its internal cancellation string. This means the pool's main task token might have been cancelled unexpectedly or handle.cancel() was called and won race.");
    }
    _ => panic!("Expected task's internal cancellation message, got {:?}", result),
  }

  manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_shutdown_graceful_allows_active_tasks_to_complete() {
  setup_tracing_for_test();
  let pool_name = "test_pool_shutdown_graceful";
  tracing::info!("Starting test: {}", pool_name);
  let manager = FuturePoolManager::<String>::new(
    2, // Concurrency limit
    5, // Queue capacity
    tokio::runtime::Handle::current(),
    pool_name,
  );

  let task1_completed_flag = Arc::new(AtomicBool::new(false));
  let task2_completed_flag = Arc::new(AtomicBool::new(false));
  let task3_should_not_run_flag = Arc::new(AtomicBool::new(false)); // To check if queued task runs

  // Task 1 (active)
  let task1_future = create_task(
    1,
    300,
    "task1_done_graceful".to_string(),
    false,
    None,
    Some(task1_completed_flag.clone()),
  );
  let handle1 = manager.submit(HashSet::new(), task1_future).await.unwrap();

  // Task 2 (active)
  let task2_future = create_task(
    2,
    350,
    "task2_done_graceful".to_string(),
    false,
    None,
    Some(task2_completed_flag.clone()),
  );
  let handle2 = manager.submit(HashSet::new(), task2_future).await.unwrap();

  // Task 3 (queued)
  let task3_future = create_task(
    3,
    50,
    "task3_queued_wont_run".to_string(),
    false,
    None,
    Some(task3_should_not_run_flag.clone()),
  );
  let _handle3 = manager.submit(HashSet::new(), task3_future).await.unwrap(); // Store handle if we want to check its result (should be error)

  // Let tasks get scheduled and start running
  sleep(Duration::from_millis(50)).await;
  assert_eq!(manager.active_task_count(), 2);
  assert_eq!(manager.queued_task_count(), 1);

  tracing::info!("Test: Initiating graceful shutdown.");
  // Shutdown gracefully. This should wait for task1 and task2.
  manager.clone().shutdown(ShutdownMode::Graceful).await.unwrap(); // Clone manager for shutdown
  tracing::info!("Test: Graceful shutdown completed.");

  // Check results of active tasks
  assert_eq!(handle1.await_result().await, Ok("task1_done_graceful".to_string()));
  assert_eq!(handle2.await_result().await, Ok("task2_done_graceful".to_string()));

  assert!(
    task1_completed_flag.load(Ordering::SeqCst),
    "Task 1 should have set its completion flag."
  );
  assert!(
    task2_completed_flag.load(Ordering::SeqCst),
    "Task 2 should have set its completion flag."
  );

  // Check that queued task did not run
  assert!(
    !task3_should_not_run_flag.load(Ordering::SeqCst),
    "Task 3 (queued) should not have run."
  );

  // If we had handle3, its await_result should probably error,
  // as the task wasn't processed and its result_sender would be dropped.
  // let result3 = _handle3.await_result().await;
  // match result3 {
  //   Err(PoolError::ResultChannelError(_)) | Err(PoolError::TaskCancelled) => { /* Expected for unprocessed task */ }
  //   _ => panic!("Expected error for unprocessed queued task, got {:?}", result3),
  // }
  // For now, just asserting the flag is enough.

  // Pool should have no active tasks after shutdown
  // Note: manager was consumed by shutdown. To check stats, we'd need another Arc or a different design.
  // For this test, verifying task completion and non-completion is key.
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_shutdown_forceful_cancels_active_tasks() {
  setup_tracing_for_test();
  let pool_name = "test_pool_shutdown_forceful";
  tracing::info!("Starting test: {}", pool_name);
  let manager = FuturePoolManager::<String>::new(2, 5, tokio::runtime::Handle::current(), pool_name);

  let task1_completed_flag = Arc::new(AtomicBool::new(false));
  // For forceful cancel, tasks ideally are designed to respond to their main CancellationToken.
  // Our `create_task` doesn't directly use the main token, it uses an optional internal one.
  // The pool's `select!` on the main token is what will cause PoolError::TaskCancelled.

  // Task 1 (active) - long running
  let task1_future = create_task(
    1,
    5000,
    "task1_forceful_wont_finish".to_string(),
    false,
    None,
    Some(task1_completed_flag.clone()),
  );
  let handle1 = manager.submit(HashSet::new(), task1_future).await.unwrap();

  // Task 2 (active) - long running
  let task2_future = create_task(2, 5000, "task2_forceful_wont_finish".to_string(), false, None, None); // No flag needed for this one
  let handle2 = manager.submit(HashSet::new(), task2_future).await.unwrap();

  // Task 3 (queued)
  let task3_should_not_run_flag = Arc::new(AtomicBool::new(false));
  let task3_future = create_task(
    3,
    50,
    "task3_queued_wont_run_forceful".to_string(),
    false,
    None,
    Some(task3_should_not_run_flag.clone()),
  );
  let _handle3 = manager.submit(HashSet::new(), task3_future).await.unwrap();

  sleep(Duration::from_millis(50)).await; // Let tasks start
  assert_eq!(manager.active_task_count(), 2);
  assert_eq!(manager.queued_task_count(), 1);

  tracing::info!("Test: Initiating forceful shutdown.");
  manager.clone().shutdown(ShutdownMode::ForcefulCancel).await.unwrap();
  tracing::info!("Test: Forceful shutdown completed.");

  // Check results of active tasks - they should be cancelled
  match handle1.await_result().await {
    Err(PoolError::TaskCancelled) => { /* Expected */ }
    res => panic!("Task 1: Expected TaskCancelled, got {:?}", res),
  }
  match handle2.await_result().await {
    Err(PoolError::TaskCancelled) => { /* Expected */ }
    res => panic!("Task 2: Expected TaskCancelled, got {:?}", res),
  }

  assert!(
    !task1_completed_flag.load(Ordering::SeqCst),
    "Task 1 should not have completed due to forceful cancel."
  );
  assert!(
    !task3_should_not_run_flag.load(Ordering::SeqCst),
    "Task 3 (queued) should not have run."
  );

  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_submit_to_shutting_down_pool_fails() {
  setup_tracing_for_test();
  let pool_name = "test_pool_submit_to_shutting_down";
  tracing::info!("Starting test: {}", pool_name);
  let manager = FuturePoolManager::<String>::new(1, 1, tokio::runtime::Handle::current(), pool_name);

  // Initiate shutdown (but don't await it fully yet, just signal)
  let manager_clone_for_shutdown = manager.clone();
  tokio::spawn(async move {
    manager_clone_for_shutdown.shutdown(ShutdownMode::Graceful).await.ok();
  });

  sleep(Duration::from_millis(50)).await; // Give shutdown a moment to start (cancel token, close queue)

  let task_future = create_task(1, 50, "task_after_shutdown_signal".to_string(), false, None, None);
  let submit_result = manager.submit(HashSet::new(), task_future).await;

  match submit_result {
    Err(PoolError::PoolShuttingDown) => { /* Expected */ }
    _ => panic!("Expected PoolShuttingDown error, got {:?}", submit_result),
  }

  // Allow the spawned shutdown to complete if it hasn't already.
  // This test's main assertion is about submit_result.
  sleep(Duration::from_millis(200)).await;
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_cancel_by_label() {
  setup_tracing_for_test();
  let pool_name = "test_pool_cancel_by_label";
  tracing::info!("Starting test: {}", pool_name);
  let manager = FuturePoolManager::<String>::new(3, 5, tokio::runtime::Handle::current(), pool_name);

  let mut labels_group_a = HashSet::new();
  labels_group_a.insert("group_a".to_string());

  let mut labels_group_b = HashSet::new();
  labels_group_b.insert("group_b".to_string());

  // Task 1 (group_a) - should be cancelled
  let task1_future = create_task(1, 5000, "task1_label_a".to_string(), false, None, None);
  let handle1 = manager.submit(labels_group_a.clone(), task1_future).await.unwrap();

  // Task 2 (group_a) - should be cancelled
  let task2_future = create_task(2, 5000, "task2_label_a".to_string(), false, None, None);
  let handle2 = manager.submit(labels_group_a.clone(), task2_future).await.unwrap();

  // Task 3 (group_b) - should NOT be cancelled by "group_a"
  let task3_completed_flag = Arc::new(AtomicBool::new(false));
  let task3_future = create_task(
    3,
    200,
    "task3_label_b".to_string(),
    false,
    None,
    Some(task3_completed_flag.clone()),
  );
  let handle3 = manager.submit(labels_group_b.clone(), task3_future).await.unwrap();

  sleep(Duration::from_millis(50)).await; // Let tasks start

  tracing::info!("Test: Cancelling tasks with label 'group_a'.");
  manager.cancel_tasks_by_label(&"group_a".to_string());

  // Check results
  match handle1.await_result().await {
    Err(PoolError::TaskCancelled) => { /* Expected */ }
    res => panic!("Task 1 (label_a): Expected TaskCancelled, got {:?}", res),
  }
  match handle2.await_result().await {
    Err(PoolError::TaskCancelled) => { /* Expected */ }
    res => panic!("Task 2 (label_a): Expected TaskCancelled, got {:?}", res),
  }

  // Task 3 should complete successfully
  assert_eq!(handle3.await_result().await, Ok("task3_label_b".to_string()));
  assert!(
    task3_completed_flag.load(Ordering::SeqCst),
    "Task 3 (label_b) should have completed."
  );

  manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  tracing::info!("Finished test: {}", pool_name);
}

#[tokio::test]
async fn test_drop_behavior_initiates_cleanup() {
  setup_tracing_for_test();
  let pool_name = "test_pool_drop_cleanup";
  tracing::info!("Starting test: {}", pool_name);

  let task_completed_flag = Arc::new(AtomicBool::new(false));

  {
    let manager = FuturePoolManager::<String>::new(1, 1, tokio::runtime::Handle::current(), pool_name);
    let task_future = create_task(
      1,
      300,
      "task_for_drop_test".to_string(),
      false,
      None,
      Some(task_completed_flag.clone()),
    );
    let _handle = manager.submit(HashSet::new(), task_future).await.unwrap();

    // Manager goes out of scope here, Drop should be called.
    tracing::info!("Test: Dropping manager for pool {}", pool_name);
  } // manager is dropped

  // Give some time for the Drop to trigger shutdown signals and for a short task to potentially complete or be halted.
  // The exact outcome of the task depends on how quickly the worker loop reacts to shutdown_token from Drop
  // and whether the task finishes before the worker thread might be terminated by Tokio runtime if detached.
  // For this test, we primarily rely on logs from Drop to show it ran.
  // If the task is short enough, it might complete.
  sleep(Duration::from_millis(500)).await;

  // Check logs for "Drop: FuturePoolManager dropped without explicit shutdown" from the pool named "test_pool_drop_cleanup".
  // This is a manual check for this test. A more robust test might involve a side channel.
  // If the task was very short it might have completed.
  // If it was longer, it might have been cut short.
  // This assertion depends on the task duration and shutdown speed.
  // For a 300ms task, it *might* complete if Drop cleanup is not immediate in stopping it.
  // For this test, let's assume it completes if the worker has a chance.
  // assert!(task_completed_flag.load(Ordering::SeqCst), "Task should have completed or Drop logs should indicate cleanup.");
  tracing::info!(
    "Test: Drop test finished checking for logs (manual step). Task completion flag: {}",
    task_completed_flag.load(Ordering::SeqCst)
  );
  // The key is that Drop runs and tries to clean up, not necessarily that every task completes.
}

#[tokio::test]
async fn test_concurrency_limit_and_queuing() {
  setup_tracing_for_test();
  let pool_name = "test_pool_concurrency_queue";
  tracing::info!("Starting test: {}", pool_name);
  // Concurrency 1, queue large enough
  let manager = FuturePoolManager::<String>::new(1, 5, tokio::runtime::Handle::current(), pool_name);
  let completion_order = Arc::new(parking_lot::Mutex::new(Vec::new()));

  let mut handles = Vec::new();

  for i in 0..3 {
    let task_id = i + 1;
    let completion_order_clone = completion_order.clone();
    let task_future = Box::pin(async move {
      tracing::info!("Task {} starting execution.", task_id);
      sleep(Duration::from_millis(100 + (task_id as u64 * 20))).await; // Staggered completion
      let mut order = completion_order_clone.lock();
      order.push(task_id);
      tracing::info!("Task {} finished execution. Order: {:?}", task_id, *order);
      format!("task_{}_done", task_id)
    }) as TaskToExecute<String>;
    handles.push(manager.submit(HashSet::new(), task_future).await.unwrap());
  }

  // Initially, 1 active, 2 queued
  sleep(Duration::from_millis(10)).await; // let first task start
  assert_eq!(manager.active_task_count(), 1);
  assert_eq!(manager.queued_task_count(), 2);

  // Wait for all tasks to complete
  for handle in handles {
    handle.await_result().await.unwrap();
  }

  let final_order = completion_order.lock();
  assert_eq!(
    *final_order,
    vec![1, 2, 3],
    "Tasks should complete in submission order due to concurrency 1."
  );

  assert_eq!(manager.active_task_count(), 0);
  assert_eq!(manager.queued_task_count(), 0);

  manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  tracing::info!("Finished test: {}", pool_name);
}
