use crate::error::PoolError;
use crate::handle::TaskHandle;
use crate::notifier::{CompletionNotifier, InternalCompletionMessage, TaskCompletionInfo, TaskCompletionStatus};
use crate::task::{ManagedTaskInternal, TaskLabel, TaskToExecute};

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashmap::DashMap;
use fibre::mpsc::{self, AsyncSender};
use futures::FutureExt;
use tokio::runtime::Handle as TokioHandle;
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{self, debug, error, info, info_span, trace, warn, Instrument};

/// Defines how the pool should behave upon shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
  /// Waits for currently active tasks to complete.
  /// Queued tasks that haven't started will not be processed.
  Graceful,
  /// Attempts to cancel all active tasks.
  /// Queued tasks that haven't started will not be processed.
  ForcefulCancel,
}

#[derive(Clone)]
pub struct FuturePoolManager<R: Send + 'static> {
  shutdown_guard: Arc<()>,
  pool_name: Arc<String>,
  semaphore: Arc<Semaphore>,
  task_queue_tx: AsyncSender<ManagedTaskInternal<R>>,
  active_task_info: Arc<DashMap<u64, (CancellationToken, Arc<HashSet<TaskLabel>>)>>,
  shutdown_token: CancellationToken,
  worker_join_handle_internal: Arc<Mutex<Option<JoinHandle<()>>>>,
  next_task_id: Arc<AtomicU64>,
  completion_notifier: Arc<CompletionNotifier>,
  internal_notification_tx: AsyncSender<InternalCompletionMessage>,
}

impl<R: Send + 'static> FuturePoolManager<R> {
  pub fn new(concurrency_limit: usize, queue_capacity: usize, tokio_handle: TokioHandle, pool_name: &str) -> Self {
    let pool_name_arc_for_components = Arc::new(pool_name.to_string()); // Used for notifier and manager's own name
    let (tx, rx) = mpsc::unbounded_async();
    let shutdown_token = CancellationToken::new();
    let worker_join_handle_internal_arc = Arc::new(Mutex::new(None));

    let (internal_noti_tx_for_fpm, internal_notification_rx_for_notifier_worker) =
      mpsc::unbounded_async::<InternalCompletionMessage>();

    let notifier_arc = CompletionNotifier::new(
      internal_notification_rx_for_notifier_worker,
      tokio_handle.clone(),
      shutdown_token.clone(),               // Share the main shutdown token
      pool_name_arc_for_components.clone(), // For logging within the notifier
    );

    let manager = Self {
      shutdown_guard: Arc::new(()),
      pool_name: pool_name_arc_for_components, // Use the same Arc
      semaphore: Arc::new(Semaphore::new(concurrency_limit.max(1))),
      task_queue_tx: tx,
      active_task_info: Arc::new(DashMap::new()),
      shutdown_token: shutdown_token.clone(),
      worker_join_handle_internal: worker_join_handle_internal_arc.clone(),
      next_task_id: Arc::new(AtomicU64::new(0)),
      completion_notifier: notifier_arc,
      internal_notification_tx: internal_noti_tx_for_fpm,
    };

    let worker_pool_name = manager.pool_name.clone();
    let worker_semaphore = manager.semaphore.clone();
    let worker_active_task_info = manager.active_task_info.clone();
    let worker_tokio_handle = tokio_handle.clone();
    let worker_shutdown_token = shutdown_token.clone();
    let worker_notification_tx = manager.internal_notification_tx.clone();

    let worker_loop_join_handle = worker_tokio_handle.clone().spawn(
      async move {
        Self::run_worker_loop(
          worker_pool_name,
          worker_semaphore,
          rx,
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

  pub fn name(&self) -> &str {
    &self.pool_name
  }

  pub fn active_task_count(&self) -> usize {
    self.active_task_info.len()
  }

  pub fn queued_task_count(&self) -> usize {
    self.task_queue_tx.len()
  }

  pub async fn submit(
    &self,
    labels: HashSet<TaskLabel>,
    task_future: TaskToExecute<R>,
  ) -> Result<TaskHandle<R>, PoolError> {
    if self.shutdown_token.is_cancelled() || self.task_queue_tx.is_closed() {
      warn!(pool_name = %self.pool_name, "Submit: Attempted to submit task to a pool that is shutting down or closed.");
      return Err(PoolError::PoolShuttingDown);
    }

    let task_id = self.next_task_id.fetch_add(1, AtomicOrdering::Relaxed);
    let token = CancellationToken::new();
    let (result_tx, result_rx) = oneshot::channel::<Result<R, PoolError>>();
    let arc_labels = Arc::new(labels);

    let managed_task_internal = ManagedTaskInternal {
      task_id,
      labels: (*arc_labels).clone(), // Clone HashSet for internal task, TaskHandle gets the Arc
      future: task_future,
      token: token.clone(),
      result_sender: Some(result_tx),
    };

    debug!(pool_name = %self.pool_name, %task_id, labels = ?managed_task_internal.labels, "Submitting task to queue.");

    match self.task_queue_tx.send(managed_task_internal).await {
      Ok(()) => Ok(TaskHandle {
        task_id,
        cancellation_token: token,
        result_receiver: Some(result_rx),
        labels: arc_labels,
      }),
      Err(send_error) => {
        error!(
          pool_name = %self.pool_name,
          %task_id,
          "Submit: Failed to send task to queue. Kanal SendError: {:?}",
          send_error
        );
        // This case is tricky. If send fails, the task was never really "managed" by the pool's worker.
        // A notification here might be unexpected if the user expects notifications only for tasks that at least reached the worker.
        // For now, let's not send a notification for submission failure, as the `submit` itself returns an error.
        // The PoolError::PoolShuttingDown or PoolError::QueueSendChannelClosed is the primary feedback.
        if self.shutdown_token.is_cancelled() || self.task_queue_tx.is_closed() {
          Err(PoolError::PoolShuttingDown)
        } else {
          Err(PoolError::QueueSendChannelClosed)
        }
      }
    }
  }

  pub fn cancel_tasks_by_label(&self, label_to_cancel: &TaskLabel) {
    self.cancel_tasks_by_labels_internal(&HashSet::from_iter([label_to_cancel.clone()]));
  }

  pub fn cancel_tasks_by_labels(&self, labels_to_cancel: &HashSet<TaskLabel>) {
    self.cancel_tasks_by_labels_internal(labels_to_cancel);
  }

  /// Registers a handler function to be called upon task completion, cancellation, or panic.
  ///
  /// Multiple handlers can be registered. Each handler will be invoked
  /// with `TaskCompletionInfo` detailing the outcome of a task.
  ///
  /// Handlers are executed asynchronously by the notifier's dedicated worker
  /// and should aim to be non-blocking. Any panics within a user-provided
  /// handler are caught and logged by the notifier, preventing them from
  /// crashing the notifier worker or the pool.
  ///
  /// Note: Handlers are called after the task's `TaskHandle::await_result()` would unblock.
  pub fn add_completion_handler(&self, handler: impl Fn(TaskCompletionInfo) + Send + Sync + 'static) {
    self.completion_notifier.add_handler(handler);
  }

  pub async fn shutdown(self, mode: ShutdownMode) -> Result<(), PoolError> {
    let already_initiating_shutdown = self.shutdown_token.is_cancelled();

    if !already_initiating_shutdown {
      info!(pool_name = %self.pool_name, "Initiating explicit pool shutdown (mode: {:?}).", mode);
      self.shutdown_token.cancel();
      let _ = self.task_queue_tx.close();
      info!(pool_name = %self.pool_name, "Shutdown token cancelled and task queue sender closed.");

      if mode == ShutdownMode::ForcefulCancel {
        info!(pool_name = %self.pool_name, "Forceful shutdown: Cancelling all active tasks.");
        // This part is mostly correct: iterate active_task_info and cancel their tokens.
        // The tasks themselves will then react, send notifications, and remove themselves from active_task_info.
        let tasks_to_cancel: Vec<(u64, CancellationToken)> = self
          .active_task_info
          .iter()
          .map(|entry| (*entry.key(), entry.value().0.clone()))
          .collect();
        if tasks_to_cancel.is_empty() {
          info!(pool_name = %self.pool_name, "No active tasks to cancel forcefully.");
        } else {
          for (task_id, token) in tasks_to_cancel {
            debug!(pool_name = %self.pool_name, %task_id, "Forcefully cancelling active task during shutdown.");
            token.cancel();
          }
          // After triggering cancellation, we still need to wait for these tasks to actually terminate
          // and remove themselves from active_task_info.
        }
      }
      // For Graceful mode, we don't cancel active tasks here. They run to completion.
    } else {
      info!(pool_name = %self.pool_name, "Shutdown already in progress.");
      // If already shutting down, we might just proceed to await completion without re-signaling.
      // Or, if this is a concurrent call, it could also participate in awaiting.
      // For simplicity, let the first call drive the main shutdown signals.
    }

    // --- Wait for all active tasks to complete ---
    // This applies to both Graceful and ForcefulCancel modes.
    // In Graceful, tasks complete naturally.
    // In ForcefulCancel, tasks complete by reacting to cancellation.
    // They all remove themselves from `active_task_info` upon termination.
    if !self.active_task_info.is_empty() {
      info!(pool_name = %self.pool_name, "Waiting for {} active task(s) to complete...", self.active_task_info.len());
      let mut check_interval = tokio::time::interval(Duration::from_millis(50)); // Check periodically
                                                                                 // Add a timeout to prevent indefinite hang if a task misbehaves
      let shutdown_wait_timeout = tokio::time::sleep(Duration::from_secs(30)); // e.g., 30 seconds max wait
      tokio::pin!(shutdown_wait_timeout);

      loop {
        tokio::select! {
            _ = &mut shutdown_wait_timeout => {
                warn!(pool_name = %self.pool_name, "Timeout waiting for active tasks to complete during shutdown. {} tasks still active.", self.active_task_info.len());
                // Optionally, at this point, one could try to forcefully cancel remaining tasks even if mode was Graceful.
                // For now, we'll just break and proceed with shutting down other components.
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
      info!(pool_name = %self.pool_name, "No active tasks to wait for at initiation of active task wait phase.");
    }

    // --- Shutdown internal worker loop ---
    // The worker loop should have already been signaled by shutdown_token and task_queue_tx closure.
    // Now, we just join its handle.
    let worker_handle_to_await: Option<JoinHandle<()>> = {
      let mut guard = self.worker_join_handle_internal.lock().unwrap();
      guard.take()
    };

    if let Some(handle) = worker_handle_to_await {
      info!(pool_name = %self.pool_name, "Waiting for main worker loop to join.");
      if let Err(join_error) = tokio::time::timeout(Duration::from_secs(5), handle).await {
        error!(pool_name = %self.pool_name, "Timeout or error joining main worker loop: {:?}.", join_error);
      } else {
        info!(pool_name = %self.pool_name, "Main worker loop successfully joined.");
      }
    } else {
      trace!(pool_name = %self.pool_name, "Main worker join handle already taken or was not set.");
    }

    // --- Shutdown notifier ---
    // Now that active tasks have finished (or timed out) and the main worker loop is down,
    // close the FPM's own notification sender.
    // This signals to the notifier that this FPM instance won't directly send more.
    // Clones held by tasks that just finished would have also been dropped.
    if !already_initiating_shutdown {
      // Avoid closing multiple times if shutdown is called concurrently
      debug!(pool_name = %self.pool_name, "Explicitly closing manager's internal notification sender before awaiting notifier.");
      let _ = self.internal_notification_tx.close();
    }

    debug!(pool_name = %self.pool_name, "Waiting for completion notifier to shutdown.");
    self.completion_notifier.await_shutdown().await;
    info!(pool_name = %self.pool_name, "Completion notifier shutdown complete.");

    if !already_initiating_shutdown {
      info!(pool_name = %self.pool_name, "Pool shutdown process completed by this call.");
    }
    Ok(())
  }

  fn cancel_tasks_by_labels_internal(&self, labels_to_cancel: &HashSet<TaskLabel>) {
    if labels_to_cancel.is_empty() {
      return;
    }
    if self.shutdown_token.is_cancelled() {
      trace!(pool_name = %self.pool_name, "Cancel by label: Pool is shutting down, cancellation might be redundant or superseded by shutdown mode.");
    }
    info!(pool_name = %self.pool_name, "Requesting cancellation for active tasks with labels: {:?}", labels_to_cancel);
    for entry in self.active_task_info.iter() {
      let (task_id, (token, task_labels)) = entry.pair();
      if !task_labels.is_disjoint(labels_to_cancel) {
        debug!(pool_name = %self.pool_name, %task_id, "Signaling cancellation for active task due to label match.");
        token.cancel();
        // The task's select! will pick this up, leading to PoolError::TaskCancelled,
        // which then triggers a notification.
      }
    }
  }

  async fn run_worker_loop(
    pool_name: Arc<String>,
    semaphore: Arc<Semaphore>,
    task_queue_rx: mpsc::AsyncReceiver<ManagedTaskInternal<R>>,
    tasks_tokio_handle: TokioHandle,
    active_task_info_map: Arc<DashMap<u64, (CancellationToken, Arc<HashSet<TaskLabel>>)>>,
    shutdown_token: CancellationToken,
    notification_tx: AsyncSender<InternalCompletionMessage>,
  ) {
    info!(name = %*pool_name, "Worker loop started.");

    loop {
      tokio::select! {
        biased;

        _ = shutdown_token.cancelled() => {
          info!(name = %*pool_name, "Shutdown signal (token) received. Worker loop terminating.");
          break;
        }

        permit_acquisition_result = semaphore.clone().acquire_owned() => {
          let permit_initial = match permit_acquisition_result {
            Ok(p) => p,
            Err(_) => {
              error!(name = %*pool_name, "Semaphore closed. Worker loop exiting.");
              // This is a catastrophic failure for the pool. No tasks can run.
              // It might be too late/complex to notify individual tasks here.
              // Global pool health is the issue. For now, no specific task notifications from here.
              break;
            }
          };
          trace!(name = %*pool_name, "Acquired semaphore permit. Available: {}", semaphore.available_permits());

          let task_and_permit_option: Option<(ManagedTaskInternal<R>, OwnedSemaphorePermit)> = tokio::select! {
            biased;
            _ = shutdown_token.cancelled() => {
              info!(name = %*pool_name, "Shutdown signal (token) received while holding permit and waiting for task. Releasing permit.");
              drop(permit_initial);
              None
            }
            recv_result = task_queue_rx.recv() => {
              match recv_result {
                Ok(task) => Some((task, permit_initial)),
                Err(_) => { // Kanal's recv Err means queue is closed and empty
                  info!(name = %*pool_name, "Task queue closed and empty. Releasing permit. Worker loop will check shutdown token and likely exit.");
                  drop(permit_initial);
                  None
                }
              }
            }
          };

          if let Some((managed_task, permit_for_task)) = task_and_permit_option {
            if managed_task.token.is_cancelled() {
              debug!(
                name = %*pool_name,
                task_id = managed_task.task_id,
                labels = ?managed_task.labels,
                "Dequeued task already cancelled (task's own token)."
              );
              if let Some(tx) = managed_task.result_sender {
                let _ = tx.send(Err(PoolError::TaskCancelled));
              }

              let completion_msg = InternalCompletionMessage {
                task_id: managed_task.task_id,
                pool_name: pool_name.clone(),
                labels: Arc::new(managed_task.labels.clone()), // managed_task.labels is HashSet
                status: TaskCompletionStatus::Cancelled,
              };
              if let Err(e) = notification_tx.send(completion_msg).await {
                error!(
                  pool_name = %*pool_name,
                  task_id = managed_task.task_id,
                  "Failed to send completion notification for pre-cancelled task: {:?}", e
                );
              }
              drop(permit_for_task); // Release permit
              continue; // Process next item from queue or shutdown
            }

            let task_id = managed_task.task_id;
            // `managed_task.labels` is HashSet, `active_task_info` needs Arc<HashSet>
            let task_labels_for_active_map = Arc::new(managed_task.labels.clone());
            let task_specific_token = managed_task.token.clone();
            let task_future = managed_task.future;
            let result_sender = managed_task.result_sender; // Option<Sender>

            active_task_info_map.insert(
              task_id,
              (task_specific_token.clone(), task_labels_for_active_map.clone()) // Clone Arc for map
            );
            debug!(
              name = %*pool_name,
              %task_id,
              labels = ?managed_task.labels, // Log original labels
              "Dequeued task. Spawning with permit."
            );

            let active_task_info_map_cleanup = active_task_info_map.clone();

            let notification_tx_for_spawned_task = notification_tx.clone();
            let pool_name_for_notification = pool_name.clone(); // Arc<String>
            // task_labels_for_active_map is already Arc<HashSet<TaskLabel>>, clone the Arc for notification
            let task_labels_for_notification = task_labels_for_active_map.clone();

            let pool_name_for_task_execution = pool_name.clone();
            let pool_name_for_instrument_span = pool_name.clone();
            let pool_name_for_then_block = pool_name.clone();


            tasks_tokio_handle.spawn({
              let permit_guard = permit_for_task; // Moves permit into the spawned task
              async move {
                let _local_permit_guard = permit_guard; // Ensures permit is held for duration of task

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
                  task_result = AssertUnwindSafe(task_future).catch_unwind() => {
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

                // Determine status for notification using a borrow *before* `execution_outcome` is potentially consumed.
                let completion_status = TaskCompletionStatus::from(&execution_outcome);

                // Send result to TaskHandle's oneshot channel. This consumes `execution_outcome`.
                if let Some(tx_result) = result_sender {
                  if tx_result.send(execution_outcome).is_err() {
                    warn!(
                      pool_name = %*pool_name_for_task_execution,
                      %task_id,
                      "Result receiver for task handle was dropped. Task outcome for handle may have been lost."
                    );
                  }
                } else {
                  // This case should ideally not happen if submit always provides a sender.
                  // If it does, `execution_outcome` was not consumed.
                  trace!(pool_name = %*pool_name_for_task_execution, %task_id, "No result_sender for task, outcome not sent to handle.");
                }

                // Send completion notification
                let completion_msg = InternalCompletionMessage {
                  task_id,
                  pool_name: pool_name_for_notification,
                  labels: task_labels_for_notification,
                  status: completion_status,
                };

                if let Err(e) = notification_tx_for_spawned_task.send(completion_msg).await {
                  error!(
                    pool_name = %*pool_name_for_task_execution,
                    %task_id,
                    "Failed to send completion notification for task: {:?}", e
                  );
                }
              }
              .instrument(info_span!(
                "managed_task",
                pool_name = %*pool_name_for_instrument_span,
                %task_id
              ))
              .then(move |_| { // This `then` block runs after the above async block completes or panics.
                // It's primarily for cleaning up active_task_info_map.
                // The permit is dropped when _local_permit_guard goes out of scope at the end of the async block.
                active_task_info_map_cleanup.remove(&task_id);
                debug!(
                  name = %*pool_name_for_then_block,
                  %task_id,
                  "Managed task finished processing, removed active info."
                );
                async {} // `then` expects a future
              })
            });
          } else { // No task obtained from queue (either due to shutdown or queue actually closed)
            trace!(name = %*pool_name, "No task processed in this iteration (task_and_permit_option was None). Continuing worker loop to re-check shutdown.");
            // The permit (`permit_initial`) would have been dropped if task_and_permit_option was None.
          }
        } // end of permit_acquisition_result match
      } // end of outer tokio::select!
    } // end of loop

    info!(
      name = %*pool_name,
      "Worker loop stopped. Active tasks remaining (e.g., after forceful shutdown or pool panic): {}",
      active_task_info_map.len()
    );
    // Any remaining tasks in active_task_info_map will not have their notifications sent by this loop.
    // If they were forcefully cancelled by shutdown, their cancellation tokens were triggered,
    // and their spawned tasks should run to completion (detecting cancellation) and send their own notifications.
    // If the pool worker itself panicked, these tasks might be orphaned.
  }
}

impl<R: Send + 'static> Drop for FuturePoolManager<R> {
  fn drop(&mut self) {
    if Arc::strong_count(&self.shutdown_guard) > 1 {
      return;
    }

    if !self.shutdown_token.is_cancelled() {
      info!(
        pool_name = %*self.pool_name,
        "FuturePoolManager instance dropped. Initiating implicit shutdown (signaling worker to stop, closing queue)."
      );
      self.shutdown_token.cancel();
      let _ = self.task_queue_tx.close(); // Close sender to help worker loop terminate

      // This signals the notifier worker that no more notifications will come from this pool instance.
      // If this is the last FPM instance sharing a notifier (not typical, notifier is per-FPM),
      // it helps the notifier drain and shut down.
      // It is important that this is done *after* shutdown_token.cancel() so that tasks
      // processing in-flight due to shutdown have a chance to send their notifications.
      // However, drop should be quick. Worker loop termination and task completion processing
      // might happen after drop returns.
      // The `CompletionNotifier::await_shutdown()` in `FuturePoolManager::shutdown()` is the main
      // place to ensure notifications are drained. In Drop, we just signal.
      // Closing `internal_notification_tx` is a strong signal to the notifier.
      // If the worker loop is still running tasks that need to send notifications,
      // they might fail if this tx is closed too early *relative to their execution*.
      // Given that the worker loop also checks `shutdown_token`, it should stop processing new tasks.
      // In-flight tasks will complete their execution in their own spawned tokio tasks.
      // So, closing this here is reasonable as a signal to the notifier that this source is done.
      let _ = self.internal_notification_tx.close();

      debug!(
        pool_name = %*self.pool_name,
        "Drop: Shutdown token cancelled, task queue sender closed, and internal notification sender closed. Worker loop will terminate."
      );
    } else {
      trace!(
        pool_name = %*self.pool_name,
        "Drop: Shutdown already in progress or completed. No new signals sent."
      );
    }
  }
}
