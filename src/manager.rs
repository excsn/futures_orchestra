use crate::error::PoolError;
use crate::handle::TaskHandle;
use crate::task::{ManagedTaskInternal, TaskLabel, TaskToExecute};

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use futures::FutureExt;
use kanal;
use tokio::runtime::Handle as TokioHandle;
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{self, debug, error, info, info_span, trace, warn, Instrument};

lazy_static::lazy_static! {
  static ref NEXT_POOL_TASK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
}

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
pub struct FuturePoolManager<R: Send  + 'static> {
  pool_name: Arc<String>,
  semaphore: Arc<Semaphore>,
  task_queue_tx: kanal::AsyncSender<ManagedTaskInternal<R>>,
  active_task_info: Arc<DashMap<u64, (CancellationToken, Arc<HashSet<TaskLabel>>)>>,
  shutdown_token: CancellationToken,
  worker_join_handle_internal: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl<R: Send  + 'static> FuturePoolManager<R> {
  pub fn new(concurrency_limit: usize, queue_capacity: usize, tokio_handle: TokioHandle, pool_name: &str) -> Arc<Self> {
    let (tx, rx) = kanal::bounded_async(queue_capacity.max(1));
    let shutdown_token = CancellationToken::new();
    let worker_join_handle_internal_arc = Arc::new(Mutex::new(None));

    let manager_arc = Arc::new(Self {
      pool_name: Arc::new(pool_name.to_string()),
      semaphore: Arc::new(Semaphore::new(concurrency_limit.max(1))),
      task_queue_tx: tx,
      active_task_info: Arc::new(DashMap::new()),
      shutdown_token: shutdown_token.clone(),
      worker_join_handle_internal: worker_join_handle_internal_arc.clone(),
    });

    let worker_pool_name = manager_arc.pool_name.clone();
    let worker_semaphore = manager_arc.semaphore.clone();
    let worker_active_task_info = manager_arc.active_task_info.clone();
    let worker_tokio_handle = tokio_handle.clone();
    let worker_shutdown_token = shutdown_token.clone(); // Use the already cloned one

    let worker_loop_join_handle = worker_tokio_handle.clone().spawn(
      // Renamed from tokio_handle to worker_tokio_handle for clarity
      async move {
        Self::run_worker_loop(
          worker_pool_name,
          worker_semaphore,
          rx,
          worker_tokio_handle,
          worker_active_task_info,
          worker_shutdown_token,
        )
        .await;
      }
      .instrument(info_span!("future_pool_worker_loop", name = %pool_name)),
    );

    // Store the join handle internally ONLY
    *worker_join_handle_internal_arc.lock().unwrap() = Some(worker_loop_join_handle);

    manager_arc // Return only the Arc<Self>
  }

  pub fn name(&self) -> &str {
    &self.pool_name
  }

  pub fn active_task_count(&self) -> usize {
    self.active_task_info.len()
  }

  /// Returns the current number of tasks in the pending queue.
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

    let task_id = NEXT_POOL_TASK_ID_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let token = CancellationToken::new();
    let (result_tx, result_rx) = oneshot::channel::<Result<R, PoolError>>();
    let arc_labels = Arc::new(labels);

    let managed_task_internal = ManagedTaskInternal {
      task_id,
      labels: (*arc_labels).clone(),
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
        // send_error is kanal::SendError<ManagedTaskInternal<R>>
        // The task that failed to send is send_error.0 (if SendError were a tuple struct)
        // or send_error.into_inner() if kanal provides such a method, or just by accessing its field.
        // For kanal, SendError<T> holds T. We can reclaim it with send_error.into_inner()
        // let _task_not_sent = send_error.into_inner(); // Reclaim the task if needed

        // The primary reason for send failure is channel closed, usually due to shutdown.
        error!(
          pool_name = %self.pool_name,
          %task_id, // task_id is defined above, still relevant for logging context
          "Submit: Failed to send task to queue. Kanal SendError: {:?}", // Log the specific error
          send_error
        );
        // Check reason for closed channel
        if self.shutdown_token.is_cancelled() || self.task_queue_tx.is_closed() {
          Err(PoolError::PoolShuttingDown)
        } else {
          // This case (send error but not apparently shutting down) should be rare.
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

  pub async fn shutdown(self: Arc<Self>, mode: ShutdownMode) -> Result<(), PoolError> {
    let already_initiating_shutdown = self.shutdown_token.is_cancelled();

    if !already_initiating_shutdown {
      info!(pool_name = %self.pool_name, "Initiating explicit pool shutdown (mode: {:?}).", mode);
      self.shutdown_token.cancel();
      let _ = self.task_queue_tx.close();
      info!(pool_name = %self.pool_name, "Shutdown token cancelled and task queue sender closed.");

      if mode == ShutdownMode::ForcefulCancel {
        info!(pool_name = %self.pool_name, "Forceful shutdown: Cancelling all active tasks.");
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
        }
      } else {
        info!(pool_name = %self.pool_name, "Graceful shutdown: Allowing active tasks to complete.");
      }
    } else {
      info!(pool_name = %self.pool_name, "Shutdown already in progress or initiated by another call/Drop.");
    }

    // Extract the handle from the Mutex protected Option, then drop the guard.
    let handle_to_await: Option<JoinHandle<()>> = {
      // New scope for the lock
      let mut guard = self.worker_join_handle_internal.lock().unwrap();
      guard.take() // Take the handle from the Option, leaving None
    }; // MutexGuard (`guard`) is dropped here, releasing the lock.

    if let Some(handle) = handle_to_await {
      // Now `handle` is owned, no lock held.
      info!(pool_name = %self.pool_name, "Waiting for worker loop to join.");
      match handle.await {
        // .await is now outside the lock's scope.
        Ok(()) => info!(pool_name = %self.pool_name, "Worker loop successfully joined."),
        Err(join_error) => {
          error!(pool_name = %self.pool_name, "Error joining worker loop during shutdown: {:?}. Worker task might have panicked.", join_error);
        }
      }
    } else {
      trace!(pool_name = %self.pool_name, "Worker join handle already taken or was not set (e.g., by a concurrent shutdown call or Drop).");
    }

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
      // Allow to proceed, might still be useful for tasks that don't respond to global shutdown token immediately.
    }
    info!(pool_name = %self.pool_name, "Requesting cancellation for active tasks with labels: {:?}", labels_to_cancel);
    // DashMap iterators are designed to be safe with concurrent modifications (insertions/removals).
    // Cancellation just flips a CancellationToken, which is thread-safe.
    for entry in self.active_task_info.iter() {
      let (task_id, (token, task_labels)) = entry.pair();
      if !task_labels.is_disjoint(labels_to_cancel) {
        debug!(pool_name = %self.pool_name, %task_id, "Signaling cancellation for active task due to label match.");
        token.cancel();
      }
    }
  }

  async fn run_worker_loop(
    pool_name: Arc<String>,
    semaphore: Arc<Semaphore>,
    task_queue_rx: kanal::AsyncReceiver<ManagedTaskInternal<R>>,
    tasks_tokio_handle: TokioHandle,
    active_task_info_map: Arc<DashMap<u64, (CancellationToken, Arc<HashSet<TaskLabel>>)>>,
    shutdown_token: CancellationToken,
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
                Err(_) => {
                  info!(name = %*pool_name, "Task queue closed and empty. Releasing permit.");
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
              drop(permit_for_task);
              continue;
            }

            let task_id = managed_task.task_id;
            let task_labels_for_active_map = Arc::new(managed_task.labels.clone());
            let task_specific_token = managed_task.token.clone();
            let task_future = managed_task.future;
            let result_sender = managed_task.result_sender;

            active_task_info_map.insert(
              task_id,
              (task_specific_token.clone(), task_labels_for_active_map)
            );
            debug!(
              name = %*pool_name,
              %task_id,
              labels = ?managed_task.labels,
              "Dequeued task. Spawning with permit."
            );

            let active_task_info_map_cleanup = active_task_info_map.clone();

            let pool_name_for_task_execution = pool_name.clone();
            let pool_name_for_instrument_span = pool_name.clone();
            let pool_name_for_then_block = pool_name.clone();

            tasks_tokio_handle.spawn({
              let permit_guard = permit_for_task;
              // This async block captures `pool_name_for_task_execution`
              async move {
                let _local_permit_guard = permit_guard;

                let execution_outcome: Result<R, PoolError> = tokio::select! {
                  biased;
                  _ = task_specific_token.cancelled() => {
                    debug!(
                      pool_name = %*pool_name_for_task_execution, // Use this clone
                      %task_id,
                      "Task execution cancelled by its specific token."
                    );
                    Err(PoolError::TaskCancelled)
                  },
                  task_result = AssertUnwindSafe(task_future).catch_unwind() => {
                    match task_result {
                      Ok(actual_result) => {
                        trace!(
                          pool_name = %*pool_name_for_task_execution, // Use this clone
                          %task_id,
                          "Task executed successfully."
                        );
                        Ok(actual_result)
                      },
                      Err(_panic_payload) => {
                        error!(
                          pool_name = %*pool_name_for_task_execution, // Use this clone
                          %task_id,
                          "Task panicked during execution."
                        );
                        Err(PoolError::TaskPanicked)
                      }
                    }
                  }
                };

                if let Some(tx_result) = result_sender {
                  if tx_result.send(execution_outcome).is_err() {
                    warn!(
                      pool_name = %*pool_name_for_task_execution, // Use this clone
                      %task_id,
                      "Result receiver for task was dropped. Task outcome may have been lost."
                    );
                  }
                }
              }
              // This .instrument() captures `pool_name_for_instrument_span`
              .instrument(info_span!(
                "managed_task",
                pool_name = %*pool_name_for_instrument_span, // Use this clone
                %task_id
              ))
              // This .then() captures `pool_name_for_then_block`
              .then(move |_| {
                active_task_info_map_cleanup.remove(&task_id);
                debug!(
                  name = %*pool_name_for_then_block, // Use this clone
                  %task_id,
                  "Managed task finished processing, removed active info."
                );
                async {}
              })
            });
          } else {
            trace!(name = %*pool_name, "No task processed in this iteration. Continuing worker loop.");
          }
        }
      }
    }

    info!(
      name = %*pool_name,
      "Worker loop stopped. Active tasks remaining (e.g., after forceful shutdown or panic): {}",
      active_task_info_map.len()
    );
  }
}

impl<R: Send  + 'static> Drop for FuturePoolManager<R> {
  fn drop(&mut self) {
    // Check if shutdown has already been initiated (e.g., by an explicit call to `shutdown()`)
    // or if this is the first time we're triggering shutdown signals due to Drop.
    // The `is_cancelled()` check is quick and doesn't require locking.
    if !self.shutdown_token.is_cancelled() {
      // Log that an implicit shutdown is happening because the manager is being dropped.
      // `self.pool_name` is an Arc<String>, so `&*self.pool_name` or `%*self.pool_name` gets the &str.
      info!(
        pool_name = %*self.pool_name,
        "FuturePoolManager instance dropped. Initiating implicit shutdown (signaling worker to stop, closing queue)."
      );

      // 1. Signal the global shutdown token.
      // This is the primary signal for the worker loop and other operations
      // to know that shutdown is in progress.
      self.shutdown_token.cancel();

      // 2. Close the sender part of the task queue.
      // - This prevents any further tasks from being successfully submitted.
      //   (Though `submit` also checks `shutdown_token.is_cancelled()`).
      // - More importantly, this will cause `task_queue_rx.recv()` in the worker loop
      //   to eventually return `Err`, which is a condition for the worker loop to terminate
      //   if it's blocked on receiving from an empty queue.
      let _ = self.task_queue_tx.close();

      // Regarding `self.worker_join_handle_internal`:
      // This field is an `Arc<Mutex<Option<JoinHandle<()>>>>`.
      // We DO NOT await the `JoinHandle` here in `drop`. `drop` should be non-blocking
      // or at least complete quickly. Awaiting the worker loop could block indefinitely
      // if tasks are long-running or if there's a deadlock.
      //
      // The worker loop is expected to terminate gracefully (or forcefully, depending on
      // its internal logic reacting to the shutdown token) on its own.
      // The `JoinHandle`, if still present in the `Option` wrapped by the `Arc<Mutex<...>>`,
      // will be dropped when the last `Arc` pointing to that `Mutex` is dropped.
      // If an explicit `shutdown()` was called, it might have already `take()`-n and awaited
      // the handle. If not, the `JoinHandle` (and the thread it represents) will eventually
      // be cleaned up by the Tokio runtime once the worker task finishes.
      //
      // The key actions for `Drop` are to signal shutdown and prevent new work,
      // allowing existing mechanisms to handle the actual worker termination.
      debug!(
        pool_name = %*self.pool_name,
        "Drop: Shutdown token cancelled and task queue sender closed. Worker loop will terminate."
      );
    } else {
      // Shutdown was already initiated (e.g., by an explicit `self.shutdown(mode).await` call).
      // No further action is needed in `drop` in this case, as the explicit shutdown
      // process is either complete or in progress.
      trace!(
        pool_name = %*self.pool_name,
        "Drop: Shutdown already in progress or completed. No new signals sent."
      );
    }
  }
}
