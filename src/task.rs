use crate::error::PoolError;
use crate::token::CancelState;

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use fibre::oneshot;

/// A descriptive label for a task, typically a `String`.
pub type TaskLabel = String;

/// The type of future that the pool executes.
/// It must be `Send` and `'static`, and produce a result of type `R`.
pub type TaskToExecute<R> = Pin<Box<dyn Future<Output = R> + Send + 'static>>;

/// One task's shared control block: id, cancellation and completion flags, and
/// labels, in a single allocation shared by the handle, the running task, the
/// registry, and completion notifications.
pub(crate) struct TaskCore {
  task_id: u64,
  state: CancelState,
  labels: HashSet<TaskLabel>,
}

impl TaskCore {
  pub(crate) fn new(task_id: u64, labels: HashSet<TaskLabel>) -> Self {
    TaskCore {
      task_id,
      state: CancelState::new(),
      labels,
    }
  }

  pub(crate) fn task_id(&self) -> u64 {
    self.task_id
  }

  pub(crate) fn labels(&self) -> &HashSet<TaskLabel> {
    &self.labels
  }

  pub(crate) fn is_cancelled(&self) -> bool {
    self.state.is_cancelled()
  }

  pub(crate) fn cancel(&self) {
    self.state.cancel();
  }

  pub(crate) async fn cancelled(&self) {
    self.state.cancelled().await;
  }

  pub(crate) fn mark_finished(&self) {
    self.state.mark_finished();
  }

  pub(crate) fn is_finished(&self) -> bool {
    self.state.is_finished()
  }
}

impl fmt::Debug for TaskCore {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("TaskCore")
      .field("task_id", &self.task_id)
      .field("cancelled", &self.is_cancelled())
      .field("labels", &self.labels)
      .finish()
  }
}

/// Internal representation of a task managed by the pool.
pub(crate) struct ManagedTaskInternal<R: Send + 'static> {
  pub(crate) core: Arc<TaskCore>,
  pub(crate) future: TaskToExecute<R>,
  pub(crate) result_sender: Option<oneshot::ExclusiveSender<Result<R, PoolError>>>,
}
