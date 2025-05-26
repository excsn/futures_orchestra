# Usage Guide: futures_orchestra

This guide provides detailed information on how to use the `futures_orchestra` library, covering core concepts, configuration, API usage, and error handling.

## Table of Contents

*   [Core Concepts](#core-concepts)
*   [Quick Start](#quick-start)
*   [Configuration and Initialization](#configuration-and-initialization)
    *   [FuturePoolManager Creation](#futurepoolmanager-creation)
    *   [ShutdownMode](#shutdownmode)
*   [Main API Sections](#main-api-sections)
    *   [FuturePoolManager](#futurepoolmanager)
    *   [TaskHandle](#taskhandle)
    *   [Task Types](#task-types)
    *   [Completion Notifications (`TaskCompletionInfo`, `TaskCompletionStatus`)](#completion-notifications)
*   [Specialized Features](#specialized-features)
    *   [Task Cancellation by Label](#task-cancellation-by-label)
    *   [Pool Shutdown](#pool-shutdown)
    *   [Completion Handlers](#completion-handlers)
*   [Error Handling](#error-handling)

## Core Concepts

*   **`FuturePoolManager<R>`**: The central struct for managing the pool. It handles task submission, queuing, worker lifecycle, and concurrency control. `FuturePoolManager` itself implements `Clone`, allowing multiple handles to the same underlying pool state. `R` is the success type returned by the futures.
*   **Task (`TaskToExecute<R>`)**: A `Pin<Box<dyn Future<Output = R> + Send + 'static>>`. This is the type of future you submit to the pool.
*   **`TaskHandle<R>`**: Returned upon successful task submission. It allows interaction with the submitted task, such as awaiting its result or requesting cancellation.
*   **`TaskLabel`**: A `String` used to categorize tasks. Tasks can have multiple labels. These labels are primarily used for bulk cancellation.
*   **`CompletionNotifier` System**: An internal system that allows users to register handlers to be notified about task completion events (success, panic, cancellation, etc.). See `FuturePoolManager::add_completion_handler`, `TaskCompletionInfo`, and `TaskCompletionStatus`.
*   **Semaphore**: Internally, a `tokio::sync::Semaphore` controls how many tasks can run concurrently.
*   **Queue**: An internal `kanal` asynchronous MPMC queue holds tasks that are waiting for a semaphore permit to run.
*   **Cooperative Cancellation**: Cancellation is signaled via `tokio_util::sync::CancellationToken`. The pool uses `tokio::select!` to race task execution against its cancellation token. Futures themselves don't directly receive the token but are interrupted if the token is cancelled.
*   **Worker Loop**: An internal asynchronous loop that dequeues tasks, acquires semaphore permits, and spawns tasks onto the provided Tokio runtime.

## Quick Start

Here's a basic example of creating a pool, submitting a task, and awaiting its result:

```rust
use futures_orchestra::{FuturePoolManager, ShutdownMode};
use std::collections::HashSet;
use std::time::Duration;
use tokio::runtime::Handle;

async fn my_simple_task(id: u32) -> String {
    println!("Task {} starting.", id);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let result = format!("Task {} finished.", id);
    println!("{}", result);
    result
}

#[tokio::main]
async fn main() {
    // Initialize tracing (optional, but good for seeing pool logs)
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO) // Or DEBUG for more verbosity
        .with_target(false) // Cleaner output for examples
        .init();

    let pool_manager = FuturePoolManager::<String>::new(
        2, // Max concurrent tasks
        5, // Queue capacity
        Handle::current(), // Handle to the Tokio runtime
        "my_app_pool",     // A descriptive name for the pool
    );

    let task_future = Box::pin(my_simple_task(1));
    let labels = HashSet::new(); // No specific labels for this task

    match pool_manager.submit(labels, task_future).await {
        Ok(handle) => {
            println!("Task submitted with ID: {}", handle.id());
            match handle.await_result().await {
                Ok(result) => println!("Task result: {}", result),
                Err(e) => eprintln!("Task failed: {:?}", e),
            }
        }
        Err(e) => {
            eprintln!("Failed to submit task: {:?}", e);
        }
    }

    // Shutdown the pool gracefully
    pool_manager
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("Pool shutdown failed");
    println!("Pool shutdown complete.");
}
```
This example demonstrates:
1.  Creating a `FuturePoolManager` with a concurrency limit of 2 and a queue capacity of 5.
2.  Defining an async task function.
3.  Pinning the future and submitting it to the pool.
4.  Awaiting the task's result using its `TaskHandle`.
5.  Gracefully shutting down the pool.

## Configuration and Initialization

### FuturePoolManager Creation

The primary way to configure the pool is through the `FuturePoolManager::new` constructor.

```rust
use futures_orchestra::FuturePoolManager;
use tokio::runtime::Handle;

// In an async context where a Tokio runtime exists:
let tokio_handle = Handle::current();
let concurrency_limit = 4; // Max 4 tasks run at once
let queue_capacity = 100;  // Up to 100 tasks can wait in queue
let pool_name = "data_processing_pool";
type MyResultType = String; // Example result type

let manager_instance = FuturePoolManager::<MyResultType>::new(
    concurrency_limit,
    queue_capacity,
    tokio_handle,
    pool_name,
);

// To share the manager, clone it:
let another_handle_to_same_pool = manager_instance.clone();
```
*   `concurrency_limit: usize`: The maximum number of tasks that can be actively running at any given time. Must be at least 1.
*   `queue_capacity: usize`: The maximum number of tasks that can be held in the pending queue. Must be at least 1.
*   `tokio_handle: tokio::runtime::Handle`: A handle to the Tokio runtime on which the pool's worker tasks will be spawned.
*   `pool_name: &str`: A descriptive name for the pool, used in logging.

`FuturePoolManager` implements `Clone`. Cloning creates another handle to the same underlying pool infrastructure (semaphore, task queue, etc.).

### ShutdownMode

Used when calling `pool_manager.shutdown()`.
```rust
pub enum ShutdownMode {
    Graceful,
    ForcefulCancel,
}
```
*   `Graceful`: Waits for currently active (running) tasks to complete. Queued tasks that haven't started will not be processed and their `TaskHandle::await_result` will likely return an error (e.g., `PoolError::ResultChannelError` or `PoolError::TaskCancelled`).
*   `ForcefulCancel`: Attempts to cancel all active tasks by triggering their cancellation tokens. Queued tasks are also not processed.

## Main API Sections

### `FuturePoolManager<R: Send + 'static>`

The main entry point for interacting with the thread pool. It implements `Clone`. `R` is the type of the successful result of tasks.

**Key Methods:**

*   `pub fn new(concurrency_limit: usize, queue_capacity: usize, tokio_handle: Handle, pool_name: &str) -> Self`
    Creates a new instance of the pool manager.

*   `pub fn name(&self) -> &str`
    Returns the name of the pool.

*   `pub fn active_task_count(&self) -> usize`
    Returns the number of tasks currently running.

*   `pub fn queued_task_count(&self) -> usize`
    Returns the number of tasks currently in the pending queue.

*   `pub async fn submit(&self, labels: HashSet<TaskLabel>, task_future: TaskToExecute<R>) -> Result<TaskHandle<R>, PoolError>`
    Submits a new task (a pinned, boxed future) to the pool with a set of labels. Returns a `TaskHandle` on success or a `PoolError` if submission fails (e.g., pool is shutting down).

*   `pub fn cancel_tasks_by_label(&self, label_to_cancel: &TaskLabel)`
    Requests cancellation for all active tasks that have the specified label.

*   `pub fn cancel_tasks_by_labels(&self, labels_to_cancel: &HashSet<TaskLabel>)`
    Requests cancellation for all active tasks that have any of the specified labels.

*   `pub fn add_completion_handler(&self, handler: impl Fn(TaskCompletionInfo) + Send + Sync + 'static)`
    Registers a handler function to be called upon task completion, cancellation, or panic. Multiple handlers can be added. See [Completion Handlers](#completion-handlers) and [Completion Notifications](#completion-notifications).

*   `pub async fn shutdown(self, mode: ShutdownMode) -> Result<(), PoolError>`
    Initiates the shutdown process for the pool. This consumes the `FuturePoolManager` instance.

**Drop Implementation:**
If a `FuturePoolManager<R>` instance is dropped, and it's effectively the last handle managing the underlying pool state (or if explicit shutdown hasn't been fully processed), an implicit shutdown sequence is initiated. It signals the internal shutdown token, closes the task submission queue, and the completion notification channel. This behaves like a graceful shutdown signal but does *not* block or await worker termination. For explicit control and to ensure workers join, call `shutdown()` manually.

### `TaskHandle<R: Send + 'static>`

A handle to a task submitted to the pool. `R` is the type of the successful result of the task.

**Key Methods:**

*   `pub fn id(&self) -> u64`
    Returns the unique ID of this task.

*   `pub fn labels(&self) -> HashSet<TaskLabel>`
    Returns a clone of the labels associated with this task.

*   `pub fn is_cancellation_requested(&self) -> bool`
    Checks if cancellation has been requested for this task via its token.

*   `pub fn cancel(&self)`
    Requests cancellation of this specific task. The task's future will be interrupted by the pool if it's actively being polled in a `tokio::select!`.

*   `pub async fn await_result(mut self) -> Result<R, PoolError>`
    Awaits the completion of the task and returns its result. Consumes the `TaskHandle`.
    *   Returns `Ok(R)` if the task completes successfully.
    *   Returns `Err(PoolError::TaskPanicked)` if the task panicked.
    *   Returns `Err(PoolError::TaskCancelled)` if the task was cancelled.
    *   Returns `Err(PoolError::ResultChannelError)` if the communication channel for the result was broken.
    *   Returns `Err(PoolError::ResultUnavailable)` if `await_result` has already been called.

### Task Types

*   `pub type TaskLabel = String;`
    A type alias for task labels.

*   `pub type TaskToExecute<R> = Pin<Box<dyn Future<Output = R> + Send + 'static>>;`
    The type of future that the pool executes. It must be `Send`, `'static`, and produce a result of type `R`.

### Completion Notifications (`TaskCompletionInfo`, `TaskCompletionStatus`)

These types are used by the completion notification system.

*   **`TaskCompletionInfo`**: A struct passed to completion handlers.
    *   `task_id: u64`: The unique ID of the completed task.
    *   `pool_name: Arc<String>`: The name of the pool.
    *   `labels: Arc<HashSet<TaskLabel>>`: Labels associated with the task.
    *   `status: TaskCompletionStatus`: The final outcome of the task.
    *   `completion_time: SystemTime`: When the completion was recorded.

*   **`TaskCompletionStatus`**: An enum indicating the outcome.
    *   `Success`: Task completed successfully.
    *   `Cancelled`: Task was cancelled.
    *   `Panicked`: Task panicked.
    *   `PoolErrorOccurred`: Task terminated due to another `PoolError`.

## Specialized Features

### Task Cancellation by Label

You can cancel a group of tasks that share common labels. This is useful for managing related tasks collectively.

```rust
use futures_orchestra::{FuturePoolManager, TaskLabel};
use std::collections::HashSet;
use tokio::runtime::Handle;

async fn my_long_task(id: usize) -> String {
    println!("Task {} (cancellable by label) starting...", id);
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    println!("Task {} finished (should have been cancelled).", id);
    format!("Task {} done", id)
}

#[tokio::main]
async fn main() {
    let pool = FuturePoolManager::<String>::new(2, 5, Handle::current(), "label_cancel_pool");

    let label_critical: TaskLabel = "critical_batch".to_string();
    let labels1 = HashSet::from_iter([label_critical.clone()]);
    let task1 = pool.submit(labels1, Box::pin(my_long_task(1))).await.unwrap();

    let label_optional: TaskLabel = "optional_cleanup".to_string();
    let labels2 = HashSet::from_iter([label_optional.clone()]);
    let task2 = pool.submit(labels2, Box::pin(my_long_task(2))).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await; // Let tasks start

    println!("Cancelling tasks with label: {}", label_critical);
    pool.cancel_tasks_by_label(&label_critical);

    // Await results (task1 should be cancelled, task2 should complete if its duration is short)
    // For this example, my_long_task(2) will also likely be cancelled if it started
    // or may complete if cancellation is very fast and it hasn't been picked up yet.
    // In a real scenario, its duration would differ or it wouldn't share the cancellation signal.

    match task1.await_result().await {
        Err(futures_orchestra::PoolError::TaskCancelled) => println!("Task 1 correctly cancelled."),
        res => println!("Task 1 result: {:?}", res),
    }
    match task2.await_result().await {
        Ok(res_str) => println!("Task 2 result: {}", res_str), // If it runs to completion
        Err(e) => println!("Task 2 error (possibly cancelled if active): {:?}", e),
    }

    pool.shutdown(futures_orchestra::ShutdownMode::Graceful).await.unwrap();
}
```
The `cancel_tasks_by_label` and `cancel_tasks_by_labels` methods iterate through active tasks and signal their cancellation tokens if they match the provided labels.

### Pool Shutdown

Shutting down the pool ensures that all resources are released and that the worker loop terminates.

```rust
use futures_orchestra::{FuturePoolManager, ShutdownMode};
use tokio::runtime::Handle;

async fn example_shutdown_usage() {
    let pool = FuturePoolManager::<String>::new(1,1, Handle::current(), "shutdown_example_pool");
    // ... submit and process tasks ...

    // For graceful shutdown (consumes 'pool'):
    match pool.shutdown(ShutdownMode::Graceful).await {
        Ok(()) => println!("Pool shutdown gracefully."),
        Err(e) => eprintln!("Error during graceful shutdown: {:?}", e),
    }

    // If you need to initiate shutdown but keep the original manager handle (e.g., for stats later,
    // though stats might not be meaningful after shutdown starts):
    // let another_pool_handle = FuturePoolManager::<String>::new(1,1, Handle::current(), "another_pool");
    // let pool_for_shutdown = another_pool_handle.clone(); // Clone the manager
    // tokio::spawn(async move {
    //     match pool_for_shutdown.shutdown(ShutdownMode::ForcefulCancel).await {
    //         Ok(()) => println!("Pool shutdown forcefully (via clone)."),
    //         Err(e) => eprintln!("Error during forceful shutdown: {:?}", e),
    //     }
    // });
    // println!("Original pool handle still exists.");
    // // Note: After shutdown is initiated, further operations on `another_pool_handle`
    // // might fail if the pool is already shutting down.
}
```
The `shutdown` method takes `self` by value, consuming the specific `FuturePoolManager` instance it's called on. It ensures that the internal worker loop and notifier are properly joined.

### Completion Handlers

You can register functions to be called whenever a task finishes, is cancelled, or panics. This is useful for logging, metrics, or triggering other actions.

```rust
use futures_orchestra::{FuturePoolManager, TaskCompletionInfo, TaskCompletionStatus, ShutdownMode};
use std::collections::HashSet;
use tokio::runtime::Handle;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

async fn sample_task_for_notifier(id: usize) -> String {
    println!("Notifier Task {} running", id);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    if id == 2 {
        panic!("Task 2 is panicking for notifier test!");
    }
    format!("Task {} result", id)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();
    let manager = FuturePoolManager::<String>::new(2, 5, Handle::current(), "notifier_pool");

    let successful_count = Arc::new(AtomicUsize::new(0));
    let failed_count = Arc::new(AtomicUsize::new(0));

    // Handler 1: Simple logger
    manager.add_completion_handler(|info: TaskCompletionInfo| {
        println!(
            "[Handler 1] Task {} (Pool: {}) completed. Status: {:?}, Labels: {:?}, Time: {:?}",
            info.task_id, info.pool_name, info.status, info.labels, info.completion_time
        );
    });

    // Handler 2: Counter
    let s_clone = successful_count.clone();
    let f_clone = failed_count.clone();
    manager.add_completion_handler(move |info: TaskCompletionInfo| {
        match info.status {
            TaskCompletionStatus::Success => {
                s_clone.fetch_add(1, Ordering::Relaxed);
            }
            _ => { // Panicked, Cancelled, or PoolErrorOccurred
                f_clone.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    let _h1 = manager.submit(HashSet::new(), Box::pin(sample_task_for_notifier(1))).await;
    let _h2 = manager.submit(HashSet::new(), Box::pin(sample_task_for_notifier(2))).await; // Will panic
    let h3 = manager.submit(HashSet::new(), Box::pin(sample_task_for_notifier(3))).await.unwrap();
    h3.cancel(); // Will be cancelled

    // Wait for tasks to likely complete or be processed by notifier
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // In a real app, you'd await handles or use other synchronization.

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();

    println!("--- Notifier Summary ---");
    println!("Successful tasks: {}", successful_count.load(Ordering::Relaxed));
    println!("Failed/Cancelled tasks: {}", failed_count.load(Ordering::Relaxed));
}
```
Handlers are executed asynchronously in dedicated tasks and should be non-blocking. Panics within a handler are caught and logged by the notifier system, preventing them from crashing the pool.

## Error Handling

The library uses a dedicated error enum `PoolError` for all fallible operations.

```rust
// From src/error.rs:
// #[derive(Error, Debug, PartialEq)]
// pub enum PoolError {
//   #[error("Task result channel error (task might have panicked, was cancelled, or receiver dropped): {0}")]
//   ResultChannelError(String),

//   #[error("Task result already taken or channel was not available")]
//   ResultUnavailable,

//   #[error("Pool's internal semaphore was closed unexpectedly")]
//   SemaphoreClosed,

//   #[error("Pool's internal task queue (sender side) was closed unexpectedly")]
//   QueueSendChannelClosed,

//   #[error("Pool's internal task queue (receiver side) was closed unexpectedly")]
//   QueueReceiveChannelClosed,

//   #[error("Submitted task future panicked")]
//   TaskPanicked,
  
//   #[error("Task was cancelled")]
//   TaskCancelled,

//   #[error("Pool is shutting down or already shut down, cannot accept new tasks")]
//   PoolShuttingDown,
// }
```

**Key Error Variants:**
*   `PoolError::ResultChannelError(String)`: The oneshot channel for a task's result was broken. This can occur if the task logic itself has an issue before sending a result, if the task was cancelled in a specific way that prevents sending a clear "cancelled" status through the channel, or if the `TaskHandle` was dropped.
*   `PoolError::ResultUnavailable`: `TaskHandle::await_result()` was called more than once.
*   `PoolError::SemaphoreClosed`: The pool's concurrency-limiting semaphore closed unexpectedly. This is a critical internal error.
*   `PoolError::QueueSendChannelClosed`: Failed to send a task to the internal queue because the queue's sender channel is closed. This usually means the pool is shutting down or has an internal fault.
*   `PoolError::QueueReceiveChannelClosed`: The pool's worker could not receive tasks from the queue because the receiver channel is closed. This is an internal error condition.
*   `PoolError::TaskPanicked`: The future submitted as a task panicked during its execution.
*   `PoolError::TaskCancelled`: The task was cancelled, either individually via its `TaskHandle`, by label, or during forceful shutdown.
*   `PoolError::PoolShuttingDown`: An operation was attempted (like submitting a new task) while the pool is in the process of shutting down or has already shut down.

Most public methods that can fail, such as `FuturePoolManager::submit` and `TaskHandle::await_result`, return `Result<T, PoolError>`.