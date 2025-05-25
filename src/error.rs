use thiserror::Error;

/// Errors that can occur within the `futures_orchestra` pool.
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
