//! A Tokio-based pool for managing concurrent execution of futures with
//! queuing, labeling, and cooperative cancellation.

mod error;
mod handle;
mod capacity_gate;
mod manager;
mod notifier;
mod task;
mod task_queue;
mod token;


pub use error::PoolError;
pub use handle::TaskHandle;
pub use manager::{FuturePoolManager, ShutdownMode};
pub use notifier::{TaskCompletionInfo, TaskCompletionStatus};
pub use task::{TaskLabel, TaskToExecute};