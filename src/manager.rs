use crate::capacity_gate::CapacityGate;
use crate::error::PoolError;
use crate::handle::TaskHandle;
use crate::notifier::{
  CompletionNotifier, CompletionSink, InternalCompletionMessage, SharedCompletionSink, TaskCompletionStatus,
};
use crate::task::{ManagedTaskInternal, TaskLabel, TaskToExecute};

use crate::task_queue::{QueueConsumer, QueueProducer, TaskQueue};
use crate::TaskCompletionInfo;

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use fibre::oneshot::exclusive;
use futures::FutureExt;
use tokio::runtime::Handle as TokioHandle;
use tokio::time::timeout;
use tokio::task::JoinHandle;
use crate::token::CancellationToken;
use tracing::{self, debug, error, info, info_span, trace, warn, Instrument};

/// Cancellation tokens and labels for the tasks currently executing, keyed by task id.
type ActiveTaskInfo = Arc<RwLock<HashMap<u64, (CancellationToken, Arc<HashSet<TaskLabel>>)>>>;

/// Defines how the `FuturePoolManager` should behave upon shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
  /// Waits for currently active tasks to complete. Queued tasks that have not
  /// yet started will be dropped and will not be processed.
  Graceful,
  /// Attempts to cooperatively cancel all currently active tasks by triggering
  /// their cancellation tokens. Queued tasks are dropped.
  ForcefulCancel,
}

/// A highly configurable, Tokio-based pool for managing the concurrent
/// execution of futures.
///
/// `FuturePoolManager` provides a robust environment for running a large number
/// of asynchronous tasks with fine-grained control over concurrency, queuing,
/// labeling, and cancellation. It is designed for resilience, ensuring that
/// panics within individual tasks do not affect the pool's operation.
///
/// # Features
/// - **Concurrency Limiting**: Restricts the number of tasks running simultaneously.
/// - **Bounded Task Queuing**: Limits the number of pending tasks to prevent
///   unbounded memory growth, providing backpressure to submitters.
/// - **Cooperative Cancellation**: Tasks can be cancelled individually via their
///   `TaskHandle` or in batches using `TaskLabel`s.
/// - **Completion Notifications**: Subscribe to task completion events (success,
///   panic, cancellation) for monitoring and cleanup.
/// - **Resilient Workers**: Catches panics within tasks to keep the pool alive.
///
/// This manager is cloneable (`Clone`), allowing multiple parts of an application
/// to submit tasks to the same pool.
#[derive(Clone)]
pub struct FuturePoolManager<R: Send + 'static> {
  /// An internal guard to manage drop behavior for cloned instances.
  shutdown_guard: Arc<()>,
  /// The user-provided name of the pool, used for logging and identification.
  pool_name: Arc<String>,
  /// A semaphore to limit the number of concurrently *executing* tasks.
  concurrency_gate: Arc<CapacityGate>,
  /// The producer handle for the lock-free, bounded task queue.
  task_queue: QueueProducer<R>,
  /// A map holding cancellation tokens and labels for all *active* tasks.
  active_task_info: ActiveTaskInfo,
  /// A token to signal a global shutdown to all pool components.
  shutdown_token: CancellationToken,
  /// A join handle for the main worker loop, used for graceful shutdown.
  worker_join_handle_internal: Arc<Mutex<Option<JoinHandle<()>>>>,
  /// An atomic counter to generate unique IDs for each submitted task.
  next_task_id: Arc<AtomicU64>,
  /// The decoupled notifier system for handling completion events.
  completion_notifier: Arc<CompletionNotifier>,
  /// The completion queue's pool-wide sender, shared by all clones of the manager.
  /// Stays uninitialized until the first completion handler is registered, so a pool
  /// nobody listens to never builds a queue. The first shutdown (or last-clone drop)
  /// closes it so the notifier's queue can disconnect.
  notification_sink: SharedCompletionSink,
}

impl<R: Send + 'static> FuturePoolManager<R> {
  /// Creates a new `FuturePoolManager`.
  ///
  /// # Arguments
  ///
  /// * `concurrency_limit`: The maximum number of tasks that can run at the same time.
  /// * `queue_capacity`: The maximum number of tasks that can be waiting in the
  ///   queue. Once this limit is reached, `submit` calls will wait asynchronously
  ///   until a slot becomes available.
  /// * `tokio_handle`: A handle to the Tokio runtime on which the pool's worker
  ///   and the submitted tasks will be spawned.
  /// * `pool_name`: A descriptive name for the pool, used in logs and metrics.
  pub fn new(concurrency_limit: usize, queue_capacity: usize, tokio_handle: TokioHandle, pool_name: &str) -> Self {
    let pool_name_arc_for_components = Arc::new(pool_name.to_string());
    let shutdown_token = CancellationToken::new();
    let worker_join_handle_internal_arc = Arc::new(Mutex::new(None));

    let task_queue = TaskQueue::new(queue_capacity);
    let (producer_queue, consumer_queue) = task_queue.split();

    let notification_sink: SharedCompletionSink = Arc::new(OnceLock::new());
    let worker_notification_sink = notification_sink.clone();

    let notifier_arc = CompletionNotifier::new(
      notification_sink.clone(),
      tokio_handle.clone(),
      shutdown_token.clone(),
      pool_name_arc_for_components.clone(),
    );

    let manager = Self {
      shutdown_guard: Arc::new(()),
      pool_name: pool_name_arc_for_components,
      concurrency_gate: Arc::new(CapacityGate::new(concurrency_limit.max(1))),
      task_queue: producer_queue,
      active_task_info: Arc::new(RwLock::new(HashMap::with_capacity(concurrency_limit.max(1)))),
      shutdown_token: shutdown_token.clone(),
      worker_join_handle_internal: worker_join_handle_internal_arc.clone(),
      next_task_id: Arc::new(AtomicU64::new(0)),
      completion_notifier: notifier_arc,
      notification_sink,
    };

    let worker_pool_name = manager.pool_name.clone();
    let worker_semaphore = manager.concurrency_gate.clone();
    let worker_active_task_info = manager.active_task_info.clone();
    let worker_tokio_handle = tokio_handle.clone();
    let worker_shutdown_token = shutdown_token.clone();

    let worker_loop_join_handle = worker_tokio_handle.clone().spawn(
      async move {
        Self::run_worker_loop(
          worker_pool_name,
          worker_semaphore,
          consumer_queue,
          worker_tokio_handle,
          worker_active_task_info,
          worker_shutdown_token,
          worker_notification_sink,
        )
        .await;
      }
      .instrument(info_span!("future_pool_worker_loop", name = %pool_name)),
    );

    *worker_join_handle_internal_arc.lock() = Some(worker_loop_join_handle);

    manager
  }

  /// Returns the configured name of the pool.
  pub fn name(&self) -> &str {
    &self.pool_name
  }

  /// Returns the current number of tasks actively running.
  pub fn active_task_count(&self) -> usize {
    self.active_task_info.read().len()
  }

  /// Returns the approximate number of tasks waiting in the queue.
  pub fn queued_task_count(&self) -> usize {
    self.task_queue.len()
  }

  /// Submits a future to the pool for execution.
  ///
  /// This method will wait asynchronously if the task queue is full.
  ///
  /// # Arguments
  ///
  /// * `labels`: A set of `TaskLabel`s to associate with the task, which can be
  ///   used for batch cancellation.
  /// * `task_future`: The future to be executed by the pool.
  ///
  /// # Returns
  ///
  /// A `Result` containing a `TaskHandle<R>` on success, which can be used to
  /// await the task's result or cancel it. Returns `PoolError` if the pool
  /// is shutting down or the internal queue is broken.
  pub async fn submit(
    &self,
    labels: HashSet<TaskLabel>,
    task_future: TaskToExecute<R>,
  ) -> Result<TaskHandle<R>, PoolError> {
    if self.shutdown_token.is_cancelled() || self.task_queue.is_closed() {
      warn!(pool_name = %self.pool_name, "Submit: Attempted to submit task to a pool that is shutting down or closed.");
      return Err(PoolError::PoolShuttingDown);
    }

    let task_id = self.next_task_id.fetch_add(1, AtomicOrdering::Relaxed);
    let token = CancellationToken::new();
    let (result_tx, result_rx) = exclusive::<Result<R, PoolError>>();
    let arc_labels = Arc::new(labels);

    let managed_task = ManagedTaskInternal {
      task_id,
      labels: arc_labels.clone(),
      future: task_future,
      token: token.clone(),
      result_sender: Some(result_tx),
    };

    debug!(pool_name = %self.pool_name, %task_id, labels = ?managed_task.labels, "Submitting task to queue.");

    match self.task_queue.send(managed_task, &self.shutdown_token).await {
      Ok(()) => Ok(TaskHandle {
        task_id,
        cancellation_token: token,
        result_receiver: Some(result_rx),
        labels: arc_labels,
        is_detached: false,
      }),
      Err(e) => {
        error!(
          pool_name = %self.pool_name,
          %task_id,
          "Submit: Failed to send task to queue. Error: {:?}",
          e
        );
        Err(e)
      }
    }
  }

  /// Requests cancellation for all active tasks that have the specified label.
  pub fn cancel_tasks_by_label(&self, label_to_cancel: &TaskLabel) {
    self.cancel_tasks_by_labels_internal(&HashSet::from_iter([label_to_cancel.clone()]));
  }

  /// Requests cancellation for all active tasks that have one or more of the specified labels.
  pub fn cancel_tasks_by_labels(&self, labels_to_cancel: &HashSet<TaskLabel>) {
    self.cancel_tasks_by_labels_internal(labels_to_cancel);
  }

  /// Registers a handler function to be called upon task completion, cancellation, or panic.
  ///
  /// Multiple handlers can be registered. Each handler will be invoked with
  /// `TaskCompletionInfo` detailing the outcome of a task. Handlers are executed
  /// asynchronously by a dedicated notifier worker and should be non-blocking.
  ///
  /// Handlers only observe tasks that complete after registration: the pool does not
  /// record completions while no handler is registered. Register before submitting work
  /// if every task's outcome matters.
  pub fn add_completion_handler(&self, handler: impl Fn(TaskCompletionInfo) + Send + Sync + 'static) {
    self.completion_notifier.add_handler(handler);
  }

  /// Shuts down the pool.
  ///
  /// This method signals all internal workers to stop, waits for tasks to
  /// finish according to the specified `ShutdownMode`, and cleans up all resources.
  ///
  /// This consumes the `FuturePoolManager` instance.
  ///
  /// # Arguments
  ///
  /// * `mode`: The `ShutdownMode` (`Graceful` or `ForcefulCancel`) to use.
  pub async fn shutdown(mut self, mode: ShutdownMode) -> Result<(), PoolError> {
    let already_initiating_shutdown = self.shutdown_token.is_cancelled();

    if !already_initiating_shutdown {
      info!(
        pool_name = %self.pool_name,
        "Initiating explicit pool shutdown (mode: {:?}).",
        mode
      );
      self.shutdown_token.cancel();
      self.task_queue.close();
      info!(
        pool_name = %self.pool_name,
        "Shutdown token cancelled and task queue sender closed."
      );

      if mode == ShutdownMode::ForcefulCancel {
        info!(
          pool_name = %self.pool_name,
          "Forceful shutdown: Cancelling all active tasks."
        );
        let tasks_to_cancel: Vec<(u64, CancellationToken)> = self
          .active_task_info
          .read()
          .iter()
          .map(|(task_id, (token, _))| (*task_id, token.clone()))
          .collect();
        if tasks_to_cancel.is_empty() {
          info!(
            pool_name = %self.pool_name,
            "No active tasks to cancel forcefully."
          );
        } else {
          for (task_id, token) in tasks_to_cancel {
            debug!(
              pool_name = %self.pool_name, %task_id,
              "Forcefully cancelling active task during shutdown."
            );
            token.cancel();
          }
        }
      }
    } else {
      info!(pool_name = %self.pool_name, "Shutdown already in progress.");
    }

    let initially_active = self.active_task_info.read().len();
    if initially_active > 0 {
      info!(
        pool_name = %self.pool_name,
        "Waiting for {} active task(s) to complete...",
        initially_active
      );
      let mut check_interval = tokio::time::interval(Duration::from_millis(50));
      let shutdown_wait_timeout = tokio::time::sleep(Duration::from_secs(30));
      tokio::pin!(shutdown_wait_timeout);

      loop {
        tokio::select! {
            _ = &mut shutdown_wait_timeout => {
                let remaining = self.active_task_info.read().len();
                warn!(pool_name = %self.pool_name, "Timeout waiting for active tasks to complete during shutdown. {} tasks still active.", remaining);
                break;
            }
            _ = check_interval.tick() => {
                let remaining = self.active_task_info.read().len();
                if remaining == 0 {
                    info!(pool_name = %self.pool_name, "All active tasks have completed.");
                    break;
                } else {
                    trace!(pool_name = %self.pool_name, "Still waiting for {} active task(s)...", remaining);
                }
            }
        }
      }
    } else {
      info!(
        pool_name = %self.pool_name,
        "No active tasks to wait for at initiation of active task wait phase."
      );
    }

    let worker_handle_to_await: Option<JoinHandle<()>> = {
      let mut guard = self.worker_join_handle_internal.lock();
      guard.take()
    };

    if let Some(handle) = worker_handle_to_await {
      info!(
        pool_name = %self.pool_name,
        "Waiting for main worker loop to join."
      );
      if let Err(join_error) = timeout(Duration::from_secs(5), handle).await {
        error!(
          pool_name = %self.pool_name,
          "Timeout or error joining main worker loop: {:?}.",
          join_error
        );
      } else {
        info!(
          pool_name = %self.pool_name,
          "Main worker loop successfully joined."
        );
      }
    } else {
      trace!(
        pool_name = %self.pool_name,
        "Main worker join handle already taken or was not set."
      );
    }

    if !already_initiating_shutdown {
      debug!(
        pool_name = %self.pool_name,
        "Dropping pool-wide notification sender before awaiting notifier."
      );
      if let Some(sink) = self.notification_sink.get() {
        sink.close();
      }
    }

    debug!(
      pool_name = %self.pool_name,
      "Waiting for completion notifier to shutdown."
    );
    if timeout(Duration::from_secs(5), self.completion_notifier.await_shutdown())
      .await
      .is_err()
    {
      error!(
        pool_name = %self.pool_name,
        "Timeout waiting for completion notifier to shutdown."
      );
    } else {
      info!(
        pool_name = %self.pool_name,
        "Completion notifier shutdown complete."
      );
    }

    if !already_initiating_shutdown {
      info!(
        pool_name = %self.pool_name,
        "Pool shutdown process completed by this call."
      );
    }
    Ok(())
  }

  /// Internal implementation for cancelling tasks by a set of labels.
  fn cancel_tasks_by_labels_internal(&self, labels_to_cancel: &HashSet<TaskLabel>) {
    if labels_to_cancel.is_empty() {
      return;
    }
    if self.shutdown_token.is_cancelled() {
      trace!(pool_name = %self.pool_name, "Cancel by label: Pool is shutting down, cancellation might be redundant or superseded by shutdown mode.");
    }
    info!(
      pool_name = %self.pool_name,
      "Requesting cancellation for active tasks with labels: {:?}",
      labels_to_cancel
    );
    let matched: Vec<(u64, CancellationToken)> = self
      .active_task_info
      .read()
      .iter()
      .filter(|(_, (_, task_labels))| !task_labels.is_disjoint(labels_to_cancel))
      .map(|(task_id, (token, _))| (*task_id, token.clone()))
      .collect();

    for (task_id, token) in matched {
      debug!(
        pool_name = %self.pool_name, %task_id,
        "Signaling cancellation for active task due to label match."
      );
      token.cancel();
    }
  }

  /// The main worker loop for the pool. (Private method)
  ///
  /// This loop is responsible for:
  /// 1. Acquiring a concurrency permit.
  /// 2. Receiving a task from the bounded queue.
  /// 3. Spawning the task onto the Tokio runtime.
  /// 4. Handling shutdown signals.
  async fn run_worker_loop(
    pool_name: Arc<String>,
    concurrency_gate: Arc<CapacityGate>,
    mut task_queue: QueueConsumer<R>,
    tasks_tokio_handle: TokioHandle,
    active_task_info_map: ActiveTaskInfo,
    shutdown_token: CancellationToken,
    notification_sink: SharedCompletionSink,
  ) {
    info!(name = %*pool_name, "Worker loop started.");

    loop {
      let concurrency_permit = tokio::select! {
          biased;
          _ = shutdown_token.cancelled() => {
              info!(name = %pool_name, "Shutdown signal (token) received. Worker loop terminating.");
              break;
          }
          permit = concurrency_gate.clone().acquire_owned() => {
            // The `acquire` future resolves to the permit guard.
            permit
          }
      };

      trace!(
        name = %*pool_name,
        "Acquired concurrency permit. Available: {}",
        concurrency_gate.get_permits()
      );

      let managed_task_option = tokio::select! {
          biased;
          _ = shutdown_token.cancelled() => {
              info!(name = %*pool_name, "Shutdown signal (token) received while holding concurrency permit and waiting for task. Releasing permit.");
              None
          }
          recv_result = task_queue.recv() => {
              match recv_result {
                  Ok(task) => Some(task),
                  Err(_) => {
                      info!(name = %*pool_name, "Task queue closed and empty. Worker loop will exit.");
                      None
                  }
              }
          }
      };

      if let Some(managed_task) = managed_task_option {
        if managed_task.token.is_cancelled() {
          debug!(
            name = %*pool_name,
            task_id = managed_task.task_id,
            "Dequeued task already cancelled."
          );
          if let Some(tx) = managed_task.result_sender {
            let _ = tx.send(Err(PoolError::TaskCancelled));
          }

          if let Some(mut noti_tx) = notification_sink.get().and_then(CompletionSink::sender) {
            let completion_msg = InternalCompletionMessage {
              task_id: managed_task.task_id,
              pool_name: pool_name.clone(),
              labels: managed_task.labels,
              status: TaskCompletionStatus::Cancelled,
            };
            if noti_tx.send(completion_msg).await.is_err() {
              error!(
                pool_name = %*pool_name,
                "Failed to send completion for pre-cancelled task."
              );
            }
          }
          continue;
        }

        let task_id = managed_task.task_id;
        let task_labels_for_active_map = managed_task.labels.clone();
        let task_specific_token = managed_task.token.clone();

        active_task_info_map.write().insert(
          task_id,
          (task_specific_token.clone(), task_labels_for_active_map.clone()),
        );

        let pool_name_for_notification = pool_name.clone();
        let pool_name_for_task_execution = pool_name.clone();

        tasks_tokio_handle.spawn({

          let notification_tx_for_spawned_task = notification_sink.get().and_then(CompletionSink::sender);

          async move {
            let _permit_guard = concurrency_permit; // Permit held for task duration
            let execution_outcome: Result<R, PoolError> = tokio::select! {
                biased;
                _ = task_specific_token.cancelled() => {
                  debug!(
                    pool_name = %*pool_name_for_task_execution,
                    %task_id,
                    "Task execution cancelled by its specific token."
                  );
                  Err(PoolError::TaskCancelled)
                },
                task_result = AssertUnwindSafe(managed_task.future).catch_unwind() => {
                  match task_result {
                    Ok(actual_result) => {
                      trace!(
                        pool_name = %*pool_name_for_task_execution,
                        %task_id,
                        "Task executed successfully."
                      );
                      Ok(actual_result)
                    },
                    Err(_panic_payload) => {
                      error!(
                        pool_name = %*pool_name_for_task_execution,
                        %task_id,
                        "Task panicked during execution."
                      );
                      Err(PoolError::TaskPanicked)
                    }
                  }
                }
            };

            let completion_status = TaskCompletionStatus::from(&execution_outcome);
            if let Some(tx_result) = managed_task.result_sender
              && tx_result.send(execution_outcome).is_err()
            {
              trace!(
                pool_name = %*pool_name_for_task_execution,
                %task_id,
                "Result receiver for task handle was dropped."
              );
            }

            if let Some(mut noti_tx) = notification_tx_for_spawned_task {
              let completion_msg = InternalCompletionMessage {
                task_id,
                pool_name: pool_name_for_notification,
                labels: task_labels_for_active_map,
                status: completion_status,
              };

              if noti_tx.send(completion_msg).await.is_err() {
                error!(
                  pool_name = %*pool_name_for_task_execution,
                  %task_id,
                  "Failed to send completion notification for task."
                );
              }
            }
          }
          .instrument(info_span!("managed_task", pool_name = %*pool_name, %task_id))
          .then({
            let pool_name = pool_name.clone();
            let active_task_info_map = active_task_info_map.clone();
            move |_| {
              active_task_info_map.write().remove(&task_id);
              debug!(
                name = %*pool_name,
                %task_id,
                "Managed task finished, removed active info."
              );
              async {}
            }
          })
        });
      } else {
        info!(
          name = %*pool_name,
          "Worker loop terminating due to closed queue or shutdown signal."
        );
        break; // Exit the main loop
      }
    }

    info!(name = %*pool_name, "Worker loop stopped.");
  }
}

impl<R: Send + 'static> Drop for FuturePoolManager<R> {
  /// Implements drop to ensure the pool is gracefully shut down when the last
  /// `FuturePoolManager` instance goes out of scope.
  ///
  /// This initiates a non-blocking shutdown by signaling all workers to terminate.
  /// It does not wait for tasks to complete. For a blocking, guaranteed shutdown,
  /// call the explicit `shutdown()` method.
  fn drop(&mut self) {
    if Arc::strong_count(&self.shutdown_guard) > 1 {
      return;
    }

    if !self.shutdown_token.is_cancelled() {
      info!(
        pool_name = %*self.pool_name,
        "FuturePoolManager instance dropped. Initiating implicit shutdown."
      );
      self.shutdown_token.cancel();
      self.task_queue.close();
      if let Some(sink) = self.notification_sink.get() {
        sink.close();
      }

      debug!(
        pool_name = %*self.pool_name,
        "Drop: Shutdown signals sent. Worker and notifier will terminate."
      );
    } else {
      trace!(
        pool_name = %*self.pool_name,
        "Drop: Shutdown already in progress or completed."
      );
    }
  }
}

#[cfg(test)]
impl<R: Send + 'static> FuturePoolManager<R> {
  /// Completion messages sitting in the queue with nothing consuming them.
  fn pending_completions(&self) -> usize {
    self
      .notification_sink
      .get()
      .and_then(CompletionSink::sender)
      .map_or(0, |tx| tx.len())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn no_completion_queue_without_a_handler() {
    let manager = FuturePoolManager::<u32>::new(2, 8, TokioHandle::current(), "lazy_notifier_pool");

    for i in 0..8u32 {
      let handle = manager.submit(HashSet::new(), Box::pin(async move { i })).await.unwrap();
      assert_eq!(handle.await_result().await, Ok(i));
    }

    assert_eq!(
      manager.pending_completions(),
      0,
      "completions were queued with nothing to consume them"
    );
    assert!(manager.notification_sink.get().is_none());

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  }

  #[tokio::test]
  async fn cancelled_tasks_do_not_queue_completions_without_a_handler() {
    let manager = FuturePoolManager::<u32>::new(1, 8, TokioHandle::current(), "lazy_notifier_cancel_pool");

    let blocker = manager
      .submit(
        HashSet::new(),
        Box::pin(async {
          tokio::time::sleep(Duration::from_millis(100)).await;
          0u32
        }),
      )
      .await
      .unwrap();

    let mut queued = Vec::new();
    for i in 1..5u32 {
      queued.push(manager.submit(HashSet::new(), Box::pin(async move { i })).await.unwrap());
    }
    for handle in &queued {
      handle.cancel();
    }

    assert_eq!(blocker.await_result().await, Ok(0));
    for handle in queued {
      assert_eq!(handle.await_result().await, Err(PoolError::TaskCancelled));
    }

    assert_eq!(manager.pending_completions(), 0);
    assert!(manager.notification_sink.get().is_none());

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  }

  #[tokio::test]
  async fn completion_queue_is_built_on_first_handler() {
    let manager = FuturePoolManager::<u32>::new(2, 8, TokioHandle::current(), "eager_notifier_pool");
    assert!(manager.notification_sink.get().is_none());

    let seen = Arc::new(AtomicU64::new(0));
    let seen_in_handler = seen.clone();
    manager.add_completion_handler(move |_| {
      seen_in_handler.fetch_add(1, AtomicOrdering::Relaxed);
    });

    assert!(manager.notification_sink.get().is_some());

    let handle = manager.submit(HashSet::new(), Box::pin(async { 7u32 })).await.unwrap();
    assert_eq!(handle.await_result().await, Ok(7));

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();
    assert_eq!(seen.load(AtomicOrdering::Relaxed), 1);
  }

  #[tokio::test]
  async fn shutdown_closes_the_sink_so_the_notifier_can_join() {
    let manager = FuturePoolManager::<u32>::new(2, 8, TokioHandle::current(), "sink_close_pool");
    manager.add_completion_handler(|_| {});

    let sink = manager.notification_sink.clone();
    assert!(sink.get().unwrap().sender().is_some());

    let handle = manager.submit(HashSet::new(), Box::pin(async { 1u32 })).await.unwrap();
    assert_eq!(handle.await_result().await, Ok(1));

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();

    assert!(sink.get().unwrap().sender().is_none());
  }
}
