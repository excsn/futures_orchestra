use crate::error::PoolError;
use crate::task::TaskLabel;
use std::collections::HashSet;
use std::sync::Arc;
use fibre::oneshot;
use tokio_util::sync::CancellationToken;
use tracing;

/// A handle to a task submitted to the `FuturePoolManager`.
///
/// Allows for requesting cancellation of the task and awaiting its result.
#[derive(Debug)]
pub struct TaskHandle<R: Send + 'static> {
  pub(crate) task_id: u64,
  pub(crate) cancellation_token: CancellationToken,
  pub(crate) result_receiver: Option<oneshot::Receiver<Result<R, PoolError>>>,
  pub(crate) labels: Arc<HashSet<TaskLabel>>,
}

impl<R: Send + 'static> TaskHandle<R> {
  /// Returns the unique ID of this task.
  pub fn id(&self) -> u64 {
    self.task_id
  }

  /// Returns a clone of the labels associated with this task.
  pub fn labels(&self) -> HashSet<TaskLabel> {
    (*self.labels).clone() // Clone the HashSet from the Arc<HashSet>
  }

  /// Checks if cancellation has been requested for this task via its token.
  pub fn is_cancellation_requested(&self) -> bool {
    self.cancellation_token.is_cancelled()
  }

  /// Requests cancellation of this specific task by triggering its `CancellationToken`.
  /// The task must be designed to cooperatively check this token.
  pub fn cancel(&self) {
    tracing::debug!(task_id = %self.task_id, "TaskHandle: Cancellation requested.");
    self.cancellation_token.cancel();
  }

  /// Awaits the completion of the task and returns its result of type `R`.
  ///
  /// # Errors
  /// Returns `PoolError::ResultChannelError` if the result channel itself was broken (e.g., sender dropped prematurely).
  /// Returns `PoolError::TaskPanicked` if the task panicked during execution.
  /// Returns `PoolError::TaskCancelled` if the task was cancelled.
  /// Returns `PoolError::ResultUnavailable` if `await_result` has already been called.
  pub async fn await_result(mut self) -> Result<R, PoolError> {
    match self.result_receiver.take() {
      Some(rx) => {
        match rx.recv().await {
          Ok(task_outcome_result) => task_outcome_result, // This is already Result<R, PoolError>
          Err(oneshot_recv_error) => {
            // This means the sender side was dropped without sending a value,
            // which can happen if the pool worker panics or has a bug before sending.
            // Or if the task was "cancelled" so abruptly the worker didn't send a specific PoolError::TaskCancelled.
            tracing::warn!(task_id = %self.task_id, "Result channel receive error: {}", oneshot_recv_error);
            Err(PoolError::ResultChannelError(format!(
              "Task (id: {}) result channel unexpectedly closed: {}",
              self.task_id, oneshot_recv_error
            )))
          }
        }
      }
      None => Err(PoolError::ResultUnavailable),
    }
  }
}
