# futures_orchestra

[![License: MPL-2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0)
[![Crates.io](https://img.shields.io/crates/v/futures_orchestra.svg)](https://crates.io/crates/futures_orchestra)
[![Docs.rs](https://docs.rs/futures_orchestra/badge.svg)](https://docs.rs/futures_orchestra)

`futures_orchestra` is a Tokio-based pool for managing concurrent execution of futures. It provides a robust solution for controlling the concurrency of asynchronous tasks, offering queuing, labeling for bulk operations, cooperative cancellation, and detailed completion notifications. This library helps in scenarios where you need to limit the number of simultaneously running futures, manage task lifecycles, observe task outcomes, and organize tasks for targeted actions like cancellation.

## Notable Users

[Hi Stakes Markets Game](https://www.histakesgame.com) -  The worlds most advanced financial simulator, available on iPhone and Android.

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

### Relaxed Variant
`FuturePoolManagerRlxd` offers the same API and features with relaxed start-order FIFO, dispatching directly past the queue when the pool is idle for roughly 40% more throughput.

## Performance

*Pay the queuing tax that tokio doesn't.*

Cost per task on an Apple M4 Pro (14 cores, rustc 1.94.1): default multi-threaded Tokio runtime, 1000 trivial tasks per iteration, concurrency limit 64, queue capacity 2048, criterion medians.

| | per task | throughput |
| --- | --- | --- |
| `tokio::spawn` + `JoinSet` | 0.38 µs | 2.62 M/s |
| `tokio::sync::Semaphore` + `tokio::spawn` | 0.46 µs | 2.18 M/s |
| the above + result channel + panic isolation | 0.41 µs | 2.45 M/s |
| the above + cancellation + task registry (DIY feature parity) | 0.83 µs | 1.20 M/s |
| `FuturePoolManager` | 0.72 µs | 1.39 M/s |
| `FuturePoolManager`, one completion handler | 1.32 µs | 755 K/s |
| `FuturePoolManagerRlxd`, 1 lane | 0.53 µs | 1.89 M/s |
| `FuturePoolManagerRlxd`, 2 lanes | 0.55 µs | 1.83 M/s |
| `FuturePoolManagerRlxd`, 4 lanes | 0.52 µs | 1.94 M/s |
| `FuturePoolManagerRlxd`, 1 lane, one completion handler | 1.02 µs | 978 K/s |
| `FuturePoolManagerRlxd`, 2 lanes, one completion handler | 1.06 µs | 939 K/s |
| `FuturePoolManagerRlxd`, 4 lanes, one completion handler | 1.09 µs | 919 K/s |

Reproduce with `cargo bench`. Benches are in `benches/`, split into `tokio_baseline` and `orchestra`.

## Installation

Add `futures_orchestra` to your `Cargo.toml`:

```toml
[dependencies]
futures_orchestra = "1"
```

This library relies on `tokio` for its asynchronous runtime. Ensure your project is set up to use Tokio.

## Getting Started

For a detailed guide on how to use `futures_orchestra`, including API overview and examples, please see the [Usage Guide](README.USAGE.md).

The `examples/` directory in the repository contains runnable code demonstrating various features of the library, such as basic submission, concurrency control, completion notifications, cancellation, and shutdown modes.

Full API reference documentation is available on [docs.rs/futures_orchestra](https://docs.rs/futures_orchestra/latest/futures_orchestra/).