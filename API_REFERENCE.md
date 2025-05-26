# API Reference: **futures_orchestra**

This document provides a detailed API reference for the `futures_orchestra` library.

## 1. Introduction / Core Concepts

`futures_orchestra` is a library designed for managing and executing asynchronous tasks (futures) in a controlled, concurrent environment within the Tokio runtime. It provides mechanisms for limiting concurrency, queuing tasks, labeling tasks for group operations, cooperative cancellation, and completion notifications.

**Core Concepts & Primary Structs/Classes:**

*   **`FuturePoolManager<R>`**:
    *   **Description**: The central component and main entry point for the library. It manages a pool of workers, a task queue, and a semaphore to control concurrency. It's responsible for accepting task submissions, scheduling their execution, and handling the pool's lifecycle, including shutdown. `FuturePoolManager` implements `Clone`, allowing instances to be easily shared to interact with the same underlying pool.
    *   **Interaction**: Users create an instance of `FuturePoolManager`. `R` is the success type returned by the futures executed by the pool.

*   **`TaskHandle<R>`**:
    *   **Description**: A handle returned when a task is successfully submitted to the `FuturePoolManager`. It allows users to interact with an individual task, such as awaiting its result or requesting its cancellation. `R` matches the result type of the submitted future.

*   **Task (`TaskToExecute<R>`)**:
    *   **Description**: Represents the actual asynchronous work to be done. It's a type alias for `Pin<Box<dyn Future<Output = R> + Send + 'static>>`. This means any future that is `Send` (can be sent between threads) and `'static` (does not hold non-static references) and produces a value of type `R` can be submitted.

*   **`TaskLabel`**:
    *   **Description**: A type alias for `String`. Labels are used to categorize tasks. A task can have multiple labels. These are primarily used for performing bulk operations, such as canceling all tasks that share a particular label.

*   **Concurrency Control**:
    *   **Description**: Achieved via an internal `tokio::sync::Semaphore`. The `FuturePoolManager` is configured with a concurrency limit, and the semaphore ensures that no more than this number of tasks are actively running simultaneously.

*   **Task Queuing**:
    *   **Description**: Tasks submitted when the concurrency limit is reached are placed into an internal, bounded, asynchronous MPMC (Multi-Producer Multi-Consumer) queue (implemented using `kanal`). Tasks are dequeued and processed as semaphore permits become available.

*   **Cooperative Cancellation**:
    *   **Description**: Cancellation is signaled through `tokio_util::sync::CancellationToken`. The `FuturePoolManager`'s worker loop uses `tokio::select!` to race the execution of a task's future against its associated cancellation token. If the token is cancelled, the future's execution is aborted by the pool.

*   **Completion Notification System**:
    *   **Description**: The pool can notify registered handlers about the completion status (success, panic, cancellation, error) of each task. This is managed by an internal `CompletionNotifier` which processes notifications asynchronously. See `FuturePoolManager::add_completion_handler`, `TaskCompletionInfo`, and `TaskCompletionStatus`.

*   **Shutdown Mechanism**:
    *   **Description**: The pool can be shut down gracefully (allowing active tasks to complete) or forcefully (attempting to cancel active tasks). The `FuturePoolManager` also implements `Drop` for implicit signaling of shutdown.

**Pervasive Types/Patterns:**

*   **`PoolError`**: The primary error enum used throughout the library for all fallible operations.
*   **`Result<T, PoolError>`**: Most public methods that can fail return this standard Rust `Result` type, specialized with `PoolError`.
*   **Cloning `FuturePoolManager<R>`**: `FuturePoolManager` implements `Clone`. Cloning an instance creates a new handle that interacts with the same underlying pool, as its internal components responsible for shared state (like the task queue sender, semaphore, shutdown token) are themselves cloneable (often `Arc`-wrapped or specialized senders/tokens).

## 2. Configuration

Configuration primarily happens during the instantiation of the `FuturePoolManager`.

*   **`FuturePoolManager::new(...)` parameters**:
    *   `concurrency_limit: usize`: The maximum number of tasks that can be actively running. Must be at least 1.
    *   `queue_capacity: usize`: The maximum capacity of the internal task queue. Must be at least 1.
    *   `tokio_handle: tokio::runtime::Handle`: A handle to an existing Tokio runtime. Tasks managed by the pool will be spawned onto this runtime.
    *   `pool_name: &str`: A descriptive name for the pool, primarily used for logging and tracing.

*   **`ShutdownMode` enum**:
    *   **Description**: Specifies the behavior of the pool during an explicit shutdown requested via `FuturePoolManager::shutdown()`.
    *   **Variants**:
        *   `Graceful`: The pool stops accepting new tasks. It waits for currently executing tasks to complete. Queued tasks that have not yet started will not be executed.
        *   `ForcefulCancel`: The pool stops accepting new tasks. It attempts to cancel all currently executing tasks. Queued tasks that have not yet started will not be executed.

## 3. Main Types and Their Public Methods

### `struct FuturePoolManager<R: Send + 'static>`

The main struct for managing and interacting with the task pool. It implements `Clone`.

**Constructors:**

*   `pub fn new(concurrency_limit: usize, queue_capacity: usize, tokio_handle: tokio::runtime::Handle, pool_name: &str) -> Self`
    *   Creates a new `FuturePoolManager` instance.
    *   `R` is the success type of the futures this pool will manage.

**Methods:**

*   `pub fn name(&self) -> &str`
    *   Returns the configured name of the pool.

*   `pub fn active_task_count(&self) -> usize`
    *   Returns the current number of tasks that are actively running (i.e., have acquired a semaphore permit and have been spawned).

*   `pub fn queued_task_count(&self) -> usize`
    *   Returns the current number of tasks waiting in the internal queue for a semaphore permit.

*   `pub async fn submit(&self, labels: std::collections::HashSet<TaskLabel>, task_future: TaskToExecute<R>) -> Result<TaskHandle<R>, PoolError>`
    *   Submits a new task to the pool.
    *   `labels`: A set of `TaskLabel` (strings) to associate with this task.
    *   `task_future`: The future to be executed, of type `TaskToExecute<R>`.
    *   Returns a `TaskHandle<R>` on successful submission, or a `PoolError` if submission fails (e.g., pool is shutting down, queue send error).

*   `pub fn cancel_tasks_by_label(&self, label_to_cancel: &TaskLabel)`
    *   Requests cancellation for all currently active tasks that have the specified `label_to_cancel`.

*   `pub fn cancel_tasks_by_labels(&self, labels_to_cancel: &std::collections::HashSet<TaskLabel>)`
    *   Requests cancellation for all currently active tasks that have one or more of the labels in `labels_to_cancel`.

*   `pub fn add_completion_handler(&self, handler: impl Fn(TaskCompletionInfo) + Send + Sync + 'static)`
    *   Registers a handler function to be called upon task completion, cancellation, or panic.
    *   Multiple handlers can be registered. Each handler will be invoked with `TaskCompletionInfo` detailing the outcome of a task.
    *   Handlers are executed asynchronously by the notifier's dedicated worker and should aim to be non-blocking. Panics within a handler are caught and logged.

*   `pub async fn shutdown(self, mode: ShutdownMode) -> Result<(), PoolError>`
    *   Initiates an explicit shutdown of the pool. This method consumes the `FuturePoolManager` instance.
    *   `mode`: The `ShutdownMode` (Graceful or ForcefulCancel) to use.
    *   Awaits the termination of the internal worker loop and the completion notification system. Returns `Ok(())` on successful shutdown, or a `PoolError` if an issue occurs during shutdown.

**`Drop` Implementation:**
`FuturePoolManager<R>` implements `std::ops::Drop`. When a `FuturePoolManager<R>` instance is dropped, if an explicit shutdown has not already been initiated for the underlying shared pool state:
1.  The internal global shutdown token is cancelled.
2.  The task submission queue (`task_queue_tx`) is closed.
3.  The internal notification sender to the completion notifier is closed.
This effectively signals the worker loop and notifier to terminate but does *not* block or await their completion. For guaranteed cleanup and joining, an explicit `shutdown()` call is recommended.

### `struct TaskHandle<R: Send + 'static>`

A handle to a task that has been submitted to the pool. `R` is the success type of the task's future.

**Methods:**

*   `pub fn id(&self) -> u64`
    *   Returns the unique ID assigned to this task by the pool.

*   `pub fn labels(&self) -> std::collections::HashSet<TaskLabel>`
    *   Returns a clone of the set of `TaskLabel`s associated with this task at the time of submission.

*   `pub fn is_cancellation_requested(&self) -> bool`
    *   Checks if cancellation has been requested for this specific task (either via `TaskHandle::cancel()` or by a label-based cancellation affecting this task).

*   `pub fn cancel(&self)`
    *   Requests cancellation of this specific task by triggering its internal `CancellationToken`.

*   `pub async fn await_result(mut self) -> Result<R, PoolError>`
    *   Awaits the completion of the task and returns its outcome.
    *   This method consumes the `TaskHandle` (takes `self` by value).
    *   Returns `Ok(R)` if the task completes successfully with a value of type `R`.
    *   Returns `Err(PoolError)` if the task panics (`PoolError::TaskPanicked`), is cancelled (`PoolError::TaskCancelled`), the result channel is broken (`PoolError::ResultChannelError`), or if this method has already been called (`PoolError::ResultUnavailable`).

### `struct TaskCompletionInfo`

Provides detailed information about a task's completion, used with `FuturePoolManager::add_completion_handler`.

**Public Fields:**

*   `pub task_id: u64`
    *   The unique ID of the completed task.
*   `pub pool_name: std::sync::Arc<String>`
    *   The name of the pool that managed the task.
*   `pub labels: std::sync::Arc<std::collections::HashSet<TaskLabel>>`
    *   An `Arc` clone of the labels associated with the task.
*   `pub status: TaskCompletionStatus`
    *   The final status of the task (e.g., Success, Cancelled, Panicked).
*   `pub completion_time: std::time::SystemTime`
    *   The timestamp when the task's completion was recorded by the notifier.

## 4. Public Traits and Their Methods

There are no public traits defined in this library for external implementation by users.

## 5. Public Enums (Non-Config)

*   **`PoolError`** (See [9. Error Handling](#9-error-handling) for variants)
    *   **Description**: Represents all possible errors that can occur within the `futures_orchestra` pool operations.

*   **`TaskCompletionStatus`**
    *   **Description**: Indicates the outcome of a task, used in `TaskCompletionInfo`.
    *   **Variants**:
        *   `Success`: The task completed successfully.
        *   `Cancelled`: The task was cancelled.
        *   `Panicked`: The task panicked during execution.
        *   `PoolErrorOccurred`: The task terminated due to another `PoolError` (e.g., result channel issues not covered by other specific statuses).

## 6. Public Functions (Free-standing)

There are no public free-standing functions exported at the crate root. All primary interactions are through methods on `FuturePoolManager` and `TaskHandle`.

## 7. Public Type Aliases

*   `pub type TaskLabel = String;`
    *   Alias for `String`, used for labeling tasks.

*   `pub type TaskToExecute<R> = std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'static>>;`
    *   Alias for the type of future that can be submitted to the pool.
    *   `R`: The output type of the future.
    *   The future must be `Send` (sendable between threads) and `'static` (has no non-static lifetimes).

## 8. Public Constants

There are no public constants exported at the crate root.

## 9. Error Handling

### `enum PoolError`

The primary error type used throughout the library. It implements `std::error::Error` (via `thiserror::Error`) and `std::fmt::Debug`, `PartialEq`.

**Variants:**

*   `ResultChannelError(String)`: An error occurred with the `tokio::sync::oneshot` channel used to communicate the task's result back to its `TaskHandle`. This can happen if the task panicked before sending, was cancelled abruptly, or the `TaskHandle` (receiver) was dropped. The `String` provides more context.
*   `ResultUnavailable`: `TaskHandle::await_result()` was called when the result had already been taken or the channel was otherwise not available (e.g., called multiple times).
*   `SemaphoreClosed`: The pool's internal `tokio::sync::Semaphore` was unexpectedly closed, preventing new tasks from acquiring permits.
*   `QueueSendChannelClosed`: The sender side of the pool's internal task queue (`kanal::AsyncSender`) was unexpectedly closed when trying to submit a task. This typically indicates the pool is shutting down or has encountered a critical error.
*   `QueueReceiveChannelClosed`: The receiver side of the pool's internal task queue (`kanal::AsyncReceiver`) was unexpectedly closed. This is an internal error condition for the worker loop.
*   `TaskPanicked`: The submitted task future panicked during its execution.
*   `TaskCancelled`: The task was cancelled before it could complete.
*   `PoolShuttingDown`: An operation was attempted (e.g., submitting a new task) while the pool is in the process of shutting down or has already shut down.

**Standard Result Type:**
The library does not define a custom `Result` type alias (like `LibResult<T>`). It uses the standard `std::result::Result<T, PoolError>` for operations that can fail with a `PoolError`.

## 10. Modules

The public API is primarily exposed from the crate root. Key types are:
*   `futures_orchestra::FuturePoolManager`
*   `futures_orchestra::TaskHandle`
*   `futures_orchestra::PoolError`
*   `futures_orchestra::ShutdownMode`
*   `futures_orchestra::TaskLabel` (type alias)
*   `futures_orchestra::TaskToExecute<R>` (type alias)
*   `futures_orchestra::TaskCompletionInfo`
*   `futures_orchestra::TaskCompletionStatus`