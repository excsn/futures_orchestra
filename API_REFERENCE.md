# API Reference: **futures_orchestra**

This document provides a detailed API reference for the `futures_orchestra` library.

### 1. Introduction & Core Concepts

`futures-orchestra` is a Tokio-based asynchronous task execution pool designed for scenarios requiring precise control over concurrency, task queuing, and lifecycle management. It offers a resilient and observable environment for running futures, ensuring that failures in one task do not impact the pool itself.

#### Core Concepts

*   **`FuturePoolManager`**: The central orchestrator and the main entry point to the library. It manages worker tasks, a task queue, and concurrency limits. It is cloneable (`Clone`), allowing multiple parts of an application to submit tasks to the same shared pool.
*   **`TaskHandle`**: A handle returned upon task submission. It allows a user to await the task's result or request its cancellation individually.
*   **Concurrency & Queuing**: The pool strictly limits the number of concurrently running tasks. Additional tasks are placed in a bounded queue, providing backpressure to the submitter if the queue is full.
*   **Cancellation**: The library provides multiple cooperative cancellation mechanisms:
    *   **Individual Cancellation**: via `TaskHandle::cancel()`.
    *   **Label-based Cancellation**: via `FuturePoolManager::cancel_tasks_by_label()`.
    *   **Pool-wide Cancellation**: via `FuturePoolManager::shutdown()` with `ShutdownMode::ForcefulCancel`.
*   **Completion Notification**: A decoupled system for receiving real-time notifications about the outcome (success, panic, cancellation) of every task. This is useful for logging, metrics, or other "fire-and-forget" monitoring patterns.
*   **Error Handling**: The library uses a comprehensive `PoolError` enum to report failures, such as task panics or cancellations, allowing for robust error handling.

---

### 2. Primary Types

These are the main structs you will interact with when using the library.

#### `FuturePoolManager<R>`

The central struct for creating, managing, and shutting down a task pool.

**Generic Parameters:**
*   `R: Send + 'static`: The return type of the futures that will be executed by this pool.

##### Methods

*   **Constructor**
    ```rust
    pub fn new(
        concurrency_limit: usize,
        queue_capacity: usize,
        tokio_handle: tokio::runtime::Handle,
        pool_name: &str
    ) -> Self
    ```
    Creates a new `FuturePoolManager`.

*   **Task Submission**
    ```rust
    pub async fn submit(
        &self,
        labels: std::collections::HashSet<TaskLabel>,
        task_future: TaskToExecute<R>
    ) -> Result<TaskHandle<R>, PoolError>
    ```
    Submits a future to the pool for execution. This method will wait asynchronously if the task queue is full.

*   **Cancellation**
    ```rust
    pub fn cancel_tasks_by_label(&self, label_to_cancel: &TaskLabel)
    ```
    Requests cancellation for all *active* tasks that have the specified label.

    ```rust
    pub fn cancel_tasks_by_labels(&self, labels_to_cancel: &std::collections::HashSet<TaskLabel>)
    ```
    Requests cancellation for all *active* tasks that have one or more of the specified labels.

*   **State & Monitoring**
    ```rust
    pub fn name(&self) -> &str
    ```
    Returns the configured name of the pool.

    ```rust
    pub fn active_task_count(&self) -> usize
    ```
    Returns the current number of tasks actively running.

    ```rust
    pub fn queued_task_count(&self) -> usize
    ```
    Returns the approximate number of tasks waiting in the queue.

    ```rust
    pub fn add_completion_handler(
        &self,
        handler: impl Fn(TaskCompletionInfo) + Send + Sync + 'static
    )
    ```
    Registers a handler function to be called upon any task's completion.

*   **Shutdown**
    ```rust
    pub async fn shutdown(self, mode: ShutdownMode) -> Result<(), PoolError>
    ```
    Initiates a clean shutdown of the pool, consuming the manager instance. The behavior is determined by the provided `ShutdownMode`.

#### `TaskHandle<R>`

A handle to a task submitted to the pool, allowing for interaction and result retrieval.

**Generic Parameters:**
*   `R: Send + 'static`: The return type of the future this handle corresponds to.

##### Methods

*   **Information**
    ```rust
    pub fn id(&self) -> u64
    ```
    Returns the unique ID of this task.

    ```rust
    pub fn labels(&self) -> std::collections::HashSet<TaskLabel>
    ```
    Returns a clone of the labels associated with this task.

*   **Cancellation**
    ```rust
    pub fn is_cancellation_requested(&self) -> bool
    ```
    Checks if cancellation has been requested for this task.

    ```rust
    pub fn cancel(&self)
    ```
    Requests cancellation of this specific task.

*   **Result Retrieval**
    ```rust
    pub async fn await_result(mut self) -> Result<R, PoolError>
    ```
    Awaits the completion of the task and returns its result. This method consumes the handle and can only be called once.

---

### 3. Configuration & Shutdown

These enums are used to configure the pool's shutdown behavior.

#### `ShutdownMode`

Defines how the `FuturePoolManager` should behave upon shutdown.

**Variants:**

*   `Graceful`
    Waits for currently active tasks to complete. Queued tasks that have not yet started are dropped.
*   `ForcefulCancel`
    Attempts to cooperatively cancel all currently active tasks by triggering their cancellation tokens. Queued tasks are dropped.

---

### 4. Completion Notification API

These types are used by the completion notification system.

#### `TaskCompletionInfo`

A struct containing detailed information about a completed task, passed to handlers registered with `add_completion_handler`.

**Public Fields:**

*   `pub task_id: u64`
*   `pub pool_name: std::sync::Arc<String>`
*   `pub labels: std::sync::Arc<std::collections::HashSet<TaskLabel>>`
*   `pub status: TaskCompletionStatus`
*   `pub completion_time: std::time::SystemTime`

#### `TaskCompletionStatus`

An enum describing the outcome of a completed task.

**Variants:**

*   `Success`
*   `Cancelled`
*   `Panicked`
*   `PoolErrorOccurred`

---

### 5. Public Type Aliases

These type aliases provide convenient names for commonly used types.

*   `pub type TaskLabel = String;`
    A descriptive label for a task, used for identification and batch cancellation.

*   `pub type TaskToExecute<R> = std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'static>>;`
    The type of future that the pool executes. It must be a pinned, boxed, `Send`, `'static` future that produces a result of type `R`.

---

### 6. Error Handling

#### `PoolError`

The primary error enum for the library, returned by `submit` and `await_result`.

**Variants:**

*   `ResultChannelError(String)`
    The task's result channel encountered an error. This often occurs when a task is dropped from the queue during shutdown before it could run.
*   `ResultUnavailable`
    An attempt was made to call `await_result` more than once on the same `TaskHandle`.
*   `SemaphoreClosed`
    The pool's internal concurrency gate (the mechanism limiting the number of active tasks) was closed unexpectedly.
*   `QueueSendChannelClosed`
    The pool's internal task queue (sender side) was closed unexpectedly.
*   `QueueReceiveChannelClosed`
    The pool's internal task queue (receiver side) was closed unexpectedly.
*   `TaskPanicked`
    The submitted future panicked during its execution.
*   `TaskCancelled`
    The task was cancelled, either individually, by label, or during a forceful shutdown.
*   `PoolShuttingDown`
    An attempt was made to submit a task to a pool that is shutting down or has already been shut down.