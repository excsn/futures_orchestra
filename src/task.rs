use crate::error::PoolError;

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// A descriptive label for a task, typically a `String`.
pub type TaskLabel = String;

/// The type of future that the pool executes.
/// It must be `Send` and `'static`, and produce a result of type `R`.
pub type TaskToExecute<R> = Pin<Box<dyn Future<Output = R> + Send + 'static>>;

/// Internal representation of a task managed by the pool.
pub(crate) struct ManagedTaskInternal<R: Send + 'static> {
  pub(crate) task_id: u64,
  pub(crate) labels: HashSet<TaskLabel>,
  pub(crate) future: TaskToExecute<R>,
  pub(crate) token: CancellationToken,
  pub(crate) result_sender: Option<oneshot::Sender<Result<R, PoolError>>>, // Option to allow taking
}
