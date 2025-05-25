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
*   [Specialized Features](#specialized-features)
    *   [Task Cancellation by Label](#task-cancellation-by-label)
    *   [Pool Shutdown](#pool-shutdown)
*   [Error Handling](#error-handling)

## Core Concepts

*   **`FuturePoolManager<R>`**: The central struct for managing the pool. It handles task submission, queuing, worker lifecycle, and concurrency control. `R` is the success type returned by the futures.
*   **Task (`TaskToExecute<R>`)**: A `Pin<Box<dyn Future<Output = R> + Send + 'static>>`. This is the type of future you submit to the pool. The `Sync` bound is not required for the future itself.
*   **`TaskHandle<R>`**: Returned upon successful task submission. It allows interaction with the submitted task, such as awaiting its result or requesting cancellation.
*   **`TaskLabel`**: A `String` used to categorize tasks. Tasks can have multiple labels. These labels are primarily used for bulk cancellation.
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
use tokio::runtime::Handle; // Use tokio::runtime::Handle for Tokio v1+

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
    tracing_subscriber::fmt::init();

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
use tokio::runtime::Handle; // For Tokio v1+

// In an async context where a Tokio runtime exists:
let tokio_handle = Handle::current();
let concurrency_limit = 4; // Max 4 tasks run at once
let queue_capacity = 100;  // Up to 100 tasks can wait in queue
let pool_name = "data_processing_pool";

let manager = FuturePoolManager::<MyResultType>::new(
    concurrency_limit,
    queue_capacity,
    tokio_handle,
    pool_name,
);
```
*   `concurrency_limit: usize`: The maximum number of tasks that can be actively running at any given time. Must be at least 1.
*   `queue_capacity: usize`: The maximum number of tasks that can be held in the pending queue. Must be at least 1.
*   `tokio_handle: tokio::runtime::Handle`: A handle to the Tokio runtime on which the pool's worker tasks will be spawned.
*   `pool_name: &str`: A descriptive name for the pool, used in logging.

The `FuturePoolManager` is typically wrapped in an `Arc` for shared access: `Arc<FuturePoolManager<R>>`. The `new` method returns this `Arc`.

### ShutdownMode

Used when calling `pool.shutdown()`.
```rust
pub enum ShutdownMode {
    Graceful,
    ForcefulCancel,
}
```
*   `Graceful`: Waits for currently active (running) tasks to complete. Queued tasks that haven't started will not be processed and their `TaskHandle::await_result` will likely return an error (e.g., `PoolError::ResultChannelError` or `PoolError::TaskCancelled`).
*   `ForcefulCancel`: Attempts to cancel all active tasks by triggering their cancellation tokens. Queued tasks are also not processed.

## Main API Sections

### `FuturePoolManager<R: Send + Sync + 'static>`

The main entry point for interacting with the thread pool. It's clonable (as it's an `Arc`).

**Key Methods:**

*   `pub fn new(concurrency_limit: usize, queue_capacity: usize, tokio_handle: TokioHandle, pool_name: &str) -> Arc<Self>`
    Creates a new instance of the pool manager, wrapped in an `Arc`.

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

*   `pub async fn shutdown(self: Arc<Self>, mode: ShutdownMode) -> Result<(), PoolError>`
    Initiates the shutdown process for the pool. This consumes the `Arc<Self>`.

**Drop Implementation:**
If the last `Arc<FuturePoolManager<R>>` is dropped, an implicit shutdown sequence is initiated. It cancels the internal shutdown token and closes the task submission queue, effectively behaving like a graceful shutdown signal, but it does not block or await worker termination. For explicit control and to ensure workers join, call `shutdown()` manually.

### `TaskHandle<R: Send + 'static>`

A handle to a task submitted to the pool.

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

## Specialized Features

### Task Cancellation by Label

You can cancel a group of tasks that share common labels. This is useful for managing related tasks collectively.

```rust
use futures_orchestra::FuturePoolManager;
use std::collections::HashSet;

async fn example_cancellation_by_label(pool: std::sync::Arc<FuturePoolManager<String>>) {
    let label_critical = "critical_batch".to_string();
    let labels1 = HashSet::from_iter([label_critical.clone()]);
    // ... submit task1 with labels1 ...

    let label_optional = "optional_cleanup".to_string();
    let labels2 = HashSet::from_iter([label_optional.clone()]);
    // ... submit task2 with labels2 ...

    // Later, if something goes wrong with critical tasks:
    pool.cancel_tasks_by_label(&label_critical);
}
```
The `cancel_tasks_by_label` and `cancel_tasks_by_labels` methods iterate through active tasks and signal their cancellation tokens if they match the provided labels.

### Pool Shutdown

Shutting down the pool ensures that all resources are released and that the worker loop terminates.

```rust
use futures_orchestra::{FuturePoolManager, ShutdownMode};
use std::sync::Arc;

async fn example_shutdown(pool: Arc<FuturePoolManager<String>>) {
    // ... after submitting and processing tasks ...

    // For graceful shutdown:
    match pool.clone().shutdown(ShutdownMode::Graceful).await {
        Ok(()) => println!("Pool shutdown gracefully."),
        Err(e) => eprintln!("Error during graceful shutdown: {:?}", e),
    }

    // Or, for forceful shutdown:
    // match pool.shutdown(ShutdownMode::ForcefulCancel).await {
    //     Ok(()) => println!("Pool shutdown forcefully."),
    //     Err(e) => eprintln!("Error during forceful shutdown: {:?}", e),
    // }
}
```
The `shutdown` method takes `Arc<Self>` by value, consuming one reference to the pool manager. It ensures that the internal worker loop is properly joined.

## Error Handling

The library uses a dedicated error enum `PoolError` for all fallible operations.

```rust
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum PoolError {
    #[error("Failed to submit task to the pool queue: {0}")]
    QueueSendError(String),

    #[error("Task result channel error (task might have panicked, was cancelled, or receiver dropped): {0}")]
    ResultChannelError(String),

    #[error("Task result already taken or channel was not available")]
    ResultUnavailable,

    #[error("Pool's internal semaphore was closed unexpectedly")]
    SemaphoreClosed,

    #[error("Pool's internal task queue (sender side) was closed unexpectedly")]
    QueueSendChannelClosed,

    #[error("Pool's internal task queue (receiver side) was closed unexpectedly")]
    QueueReceiveChannelClosed,

    #[error("Submitted task future panicked")]
    TaskPanicked,
  
    #[error("Task was cancelled")]
    TaskCancelled,

    #[error("Pool is shutting down or already shut down, cannot accept new tasks")]
    PoolShuttingDown,
}
```

**Key Error Variants:**
*   `PoolError::QueueSendError`: Failed to send a task to the internal queue, often because the queue is full or closed.
*   `PoolError::ResultChannelError`: The oneshot channel used to send a task's result back to its `TaskHandle` was broken. This can happen if the task panicked very early, was abruptly cancelled, or the `TaskHandle` was dropped before the result was sent.
*   `PoolError::ResultUnavailable`: `TaskHandle::await_result()` was called more than once.
*   `PoolError::TaskPanicked`: The future submitted as a task panicked during its execution.
*   `PoolError::TaskCancelled`: The task was cancelled, either individually via its `TaskHandle` or as part of a group cancellation (by label or during forceful shutdown).
*   `PoolError::PoolShuttingDown`: An operation was attempted (like submitting a new task) while the pool is in the process of shutting down or has already shut down.

Most public methods that can fail, such as `FuturePoolManager::submit` and `TaskHandle::await_result`, return `Result<T, PoolError>`.