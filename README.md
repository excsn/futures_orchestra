# futures_orchestra

[![License: MPL-2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0)
[![Crates.io](https://img.shields.io/crates/v/futures_orchestra.svg)](https://crates.io/crates/futures_orchestra)
[![Docs.rs](https://docs.rs/futures_orchestra/badge.svg)](https://docs.rs/futures_orchestra)

`futures_orchestra` is a Tokio-based pool for managing concurrent execution of futures. It provides a robust solution for controlling the concurrency of asynchronous tasks, offering queuing, labeling for bulk operations, cooperative cancellation, and detailed completion notifications. This library helps in scenarios where you need to limit the number of simultaneously running futures, manage task lifecycles, observe task outcomes, and organize tasks for targeted actions like cancellation.

## Key Features

### Concurrency Limiting
The pool allows you to specify a maximum number of futures that can run concurrently. Tasks submitted beyond this limit are queued until a slot becomes available, preventing resource exhaustion.

### Task Queuing
A bounded queue holds tasks awaiting execution. You can define the capacity of this queue, and the library provides insights into the current queue length.

### Task Labeling
Assign one or more string labels to tasks upon submission. This feature enables group operations, most notably, canceling all active tasks that share a specific label or set of labels.

### Cooperative Cancellation
Tasks can be cancelled individually via their `TaskHandle` or in bulk using labels. Cancellation is cooperative; the pool's internal `tokio::select!` races task execution against a cancellation signal.

### Graceful and Forceful Shutdown
The pool can be shut down in two modes:
*   **Graceful:** Waits for currently active tasks to complete but does not start new tasks from the queue.
*   **ForcefulCancel:** Attempts to cancel all active tasks and does not start new tasks from thequeue.

### Task Handles
Submitting a task returns a `TaskHandle<R>`, which provides the task's unique ID, allows requesting its cancellation, and enables awaiting its `Result<R, PoolError>`.

### Completion Notifications
Register custom handlers to be invoked when tasks complete, providing detailed information about their outcome (success, panic, cancellation, or other errors). This is useful for logging, metrics, or triggering follow-up actions.

### Detailed Error Reporting
The library defines a comprehensive `PoolError` enum, clearly indicating the source of issues such as queue send errors, result channel problems, task panics, or cancellations.

## Installation

Add `futures_orchestra` to your `Cargo.toml`:

```toml
[dependencies]
futures_orchestra = "1.0.0" # Replace with the latest version
```

This library relies on `tokio` for its asynchronous runtime. Ensure your project is set up to use Tokio.

## Getting Started

For a detailed guide on how to use `futures_orchestra`, including API overview and examples, please see the [Usage Guide](README.USAGE.md).

The `examples/` directory in the repository contains runnable code demonstrating various features of the library, such as basic submission, concurrency control, completion notifications, cancellation, and shutdown modes.

Full API reference documentation is available on [docs.rs/futures_orchestra](https://docs.rs/futures_orchestra/latest/futures_orchestra/).