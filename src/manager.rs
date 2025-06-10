use crate::capacity_gate::CapacityGate;
use crate::error::PoolError;
use crate::handle::TaskHandle;
use crate::notifier::{CompletionNotifier, InternalCompletionMessage, TaskCompletionStatus};
use crate::task::{ManagedTaskInternal, TaskLabel, TaskToExecute};

use crate::task_queue::{QueueConsumer, QueueProducer, TaskQueue};
use crate::TaskCompletionInfo;

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashmap::DashMap;
use fibre::mpsc;
use fibre::oneshot::oneshot;
use futures::FutureExt;
use tokio::runtime::Handle as TokioHandle;
use tokio::time::timeout;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{self, debug, error, info, info_span, trace, warn, Instrument};

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
  active_task_info: Arc<DashMap<u64, (CancellationToken, Arc<HashSet<TaskLabel>>)>>,
  /// A token to signal a global shutdown to all pool components.
  shutdown_token: CancellationToken,
  /// A join handle for the main worker loop, used for graceful shutdown.
  worker_join_handle_internal: Arc<Mutex<Option<JoinHandle<()>>>>,
  /// An atomic counter to generate unique IDs for each submitted task.
  next_task_id: Arc<AtomicU64>,
  /// The decoupled notifier system for handling completion events.
  completion_notifier: Arc<CompletionNotifier>,
  /// A channel sender for the pool to send completion messages to the notifier.
  internal_notification_tx: mpsc::UnboundedAsyncSender<InternalCompletionMessage>,
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

    let (internal_noti_tx_for_fpm, internal_notification_rx_for_notifier_worker) =
      fibre::mpsc::unbounded_async::<InternalCompletionMessage>();

    let notifier_arc = CompletionNotifier::new(
      internal_notification_rx_for_notifier_worker,
      tokio_handle.clone(),
      shutdown_token.clone(),
      pool_name_arc_for_components.clone(),
    );

    let manager = Self {
      shutdown_guard: Arc::new(()),
      pool_name: pool_name_arc_for_components,
      concurrency_gate: Arc::new(CapacityGate::new(concurrency_limit.max(1))),
      task_queue: producer_queue,
      active_task_info: Arc::new(DashMap::new()),
      shutdown_token: shutdown_token.clone(),
      worker_join_handle_internal: worker_join_handle_internal_arc.clone(),
      next_task_id: Arc::new(AtomicU64::new(0)),
      completion_notifier: notifier_arc,
      internal_notification_tx: internal_noti_tx_for_fpm,
    };

    let worker_pool_name = manager.pool_name.clone();
    let worker_semaphore = manager.concurrency_gate.clone();
    let worker_active_task_info = manager.active_task_info.clone();
    let worker_tokio_handle = tokio_handle.clone();
    let worker_shutdown_token = shutdown_token.clone();
    let worker_notification_tx = manager.internal_notification_tx.clone();

    let worker_loop_join_handle = worker_tokio_handle.clone().spawn(
      async move {
        Self::run_worker_loop(
          worker_pool_name,
          worker_semaphore,
          consumer_queue,
          worker_tokio_handle,
          worker_active_task_info,
          worker_shutdown_token,
          worker_notification_tx,
        )
        .await;
      }
      .instrument(info_span!("future_pool_worker_loop", name = %pool_name)),
    );

    *worker_join_handle_internal_arc.lock().unwrap() = Some(worker_loop_join_handle);

    manager
  }

  /// Returns the configured name of the pool.
  pub fn name(&self) -> &str {
    &self.pool_name
  }

  /// Returns the current number of tasks actively running.
  pub fn active_task_count(&self) -> usize {
    self.active_task_info.len()
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
    let (result_tx, result_rx) = oneshot::<Result<R, PoolError>>();
    let arc_labels = Arc::new(labels);

    let managed_task = ManagedTaskInternal {
      task_id,
      labels: (*arc_labels).clone(),
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
  pub async fn shutdown(self, mode: ShutdownMode) -> Result<(), PoolError> {
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
          .iter()
          .map(|entry| (*entry.key(), entry.value().0.clone()))
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

    if !self.active_task_info.is_empty() {
      info!(
        pool_name = %self.pool_name,
        "Waiting for {} active task(s) to complete...",
        self.active_task_info.len()
      );
      let mut check_interval = tokio::time::interval(Duration::from_millis(50));
      let shutdown_wait_timeout = tokio::time::sleep(Duration::from_secs(30));
      tokio::pin!(shutdown_wait_timeout);

      loop {
        tokio::select! {
            _ = &mut shutdown_wait_timeout => {
                warn!(pool_name = %self.pool_name, "Timeout waiting for active tasks to complete during shutdown. {} tasks still active.", self.active_task_info.len());
                break;
            }
            _ = check_interval.tick() => {
                if self.active_task_info.is_empty() {
                    info!(pool_name = %self.pool_name, "All active tasks have completed.");
                    break;
                } else {
                    trace!(pool_name = %self.pool_name, "Still waiting for {} active task(s)...", self.active_task_info.len());
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
      let mut guard = self.worker_join_handle_internal.lock().unwrap();
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
        "Explicitly closing manager's internal notification sender before awaiting notifier."
      );
      let _ = self.internal_notification_tx.close();
    }

    debug!(
      pool_name = %self.pool_name,
      "Waiting for completion notifier to shutdown."
    );
    self.completion_notifier.await_shutdown().await;
    info!(
      pool_name = %self.pool_name,
      "Completion notifier shutdown complete."
    );

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
    for entry in self.active_task_info.iter() {
      let (task_id, (token, task_labels)) = entry.pair();
      if !task_labels.is_disjoint(labels_to_cancel) {
        debug!(
          pool_name = %self.pool_name, %task_id,
          "Signaling cancellation for active task due to label match."
        );
        token.cancel();
      }
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
    task_queue: QueueConsumer<R>,
    tasks_tokio_handle: TokioHandle,
    active_task_info_map: Arc<DashMap<u64, (CancellationToken, Arc<HashSet<TaskLabel>>)>>,
    shutdown_token: CancellationToken,
    notification_tx: mpsc::UnboundedAsyncSender<InternalCompletionMessage>,
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

          let completion_msg = InternalCompletionMessage {
            task_id: managed_task.task_id,
            pool_name: pool_name.clone(),
            labels: Arc::new(managed_task.labels),
            status: TaskCompletionStatus::Cancelled,
          };
          if notification_tx.send(completion_msg).await.is_err() {
            error!(
              pool_name = %*pool_name,
              "Failed to send completion for pre-cancelled task."
            );
          }
          continue;
        }

        let task_id = managed_task.task_id;
        let task_labels_for_active_map = Arc::new(managed_task.labels.clone());
        let task_specific_token = managed_task.token.clone();

        active_task_info_map.insert(
          task_id,
          (task_specific_token.clone(), task_labels_for_active_map.clone()),
        );

        let notification_tx_for_spawned_task = notification_tx.clone();
        let pool_name_for_notification = pool_name.clone();
        let pool_name_for_task_execution = pool_name.clone();

        tasks_tokio_handle.spawn({
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
            if let Some(tx_result) = managed_task.result_sender {
              if tx_result.send(execution_outcome).is_err() {
                trace!(
                  pool_name = %*pool_name_for_task_execution,
                  %task_id,
                  "Result receiver for task handle was dropped."
                );
              }
            }

            let completion_msg = InternalCompletionMessage {
              task_id,
              pool_name: pool_name_for_notification,
              labels: task_labels_for_active_map,
              status: completion_status,
            };

            if notification_tx_for_spawned_task.send(completion_msg).await.is_err() {
              error!(
                pool_name = %*pool_name_for_task_execution,
                %task_id,
                "Failed to send completion notification for task."
              );
            }
          }
          .instrument(info_span!("managed_task", pool_name = %*pool_name, %task_id))
          .then({
            let pool_name = pool_name.clone();
            let active_task_info_map = active_task_info_map.clone();
            move |_| {
              active_task_info_map.remove(&task_id);
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
      let _ = self.internal_notification_tx.close();

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
