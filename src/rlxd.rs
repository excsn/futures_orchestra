//! Relaxed-ordering variant of the pool: N dispatcher lanes, each a dedicated MPSC
//! queue drained by its own dispatcher loop, with submissions routed round-robin.
//!
//! Ordering is strict within a lane and best-effort across lanes: lanes drain
//! independently, so a task can start ahead of an earlier submission that landed in a
//! busier lane. Everything else carries the same guarantees as
//! [`FuturePoolManager`](crate::FuturePoolManager): bounded queue with backpressure,
//! cooperative cancellation by handle or label, completion notifications, panic
//! isolation, and graceful or forceful shutdown.

use crate::active::DeferredRegistry;
use crate::capacity_gate::CapacityGate;
use crate::error::PoolError;
use crate::handle::TaskHandle;
use crate::manager::ShutdownMode;
use crate::notifier::{
  CompletionNotifier, CompletionSink, InternalCompletionMessage, SharedCompletionSink, TaskCompletionStatus,
};
use crate::task::{ManagedTaskInternal, TaskCore, TaskLabel, TaskToExecute};
use crate::token::CancellationToken;
use crate::TaskCompletionInfo;

use std::collections::HashSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use fibre::mpsc::{UnboundedAsyncReceiver, UnboundedAsyncSender};

use fibre::oneshot::{exclusive, ExclusiveSender};
use futures::FutureExt;
use parking_lot::Mutex;
use tokio::runtime::Handle as TokioHandle;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{self, debug, error, info, info_span, trace, warn, Instrument};

/// One registry shard per lane, with removal deferred: a finishing task marks its
/// token finished and decrements the shard's counter, and the next inserter into that
/// shard sweeps reclaimable slots. Label cancels and shutdown sweeps read every shard
/// and skip finished entries.
///
/// Measured (M4 Pro, trivial tasks, 500K/run, 20 pairs): sharding is within noise of
/// one shared lock at 2 and 4 dispatchers (+4 ns [-27, +36] and +6 ns [-22, +34]);
/// kept so registry writes stay uncontended if lane counts grow.
type ActiveTaskInfo = Arc<Vec<DeferredRegistry>>;

/// Borrowed context for spawning one task, shared by the dispatcher loops and the
/// direct-dispatch fast path.
struct DispatchContext<'a> {
  tokio_handle: &'a TokioHandle,
  pool_name: &'a Arc<String>,
  notification_sink: &'a SharedCompletionSink,
  shard: &'a ShardHandle,
}

/// A dispatch site's view of one registry shard.
#[derive(Clone)]
struct ShardHandle {
  shards: ActiveTaskInfo,
  index: usize,
}

impl ShardHandle {
  fn insert(&self, core: Arc<TaskCore>) {
    self.shards[self.index].insert(core);
  }

  fn finish(&self) {
    self.shards[self.index].finish();
  }
}

struct QueueMessage<R: Send + 'static> {
  task: ManagedTaskInternal<R>,
  _permit: QueuePermit,
}

struct QueuePermit {
  gate: Arc<CapacityGate>,
}

impl Drop for QueuePermit {
  fn drop(&mut self) {
    self.gate.release();
  }
}

/// One bounded lane per dispatcher: a shared `CapacityGate` for global backpressure
/// over N unbounded MPSC channels, submissions routed round-robin.
///
/// Isolated measurement (M4 Pro, high power, pool-shaped messages, one producer):
/// per-lane MPSC costs +34 ns/msg at 2 lanes and +92 at 4 over a single MPSC, versus
/// +63 and +208 for a shared MPMC with the same consumer counts, so routing beats a
/// shared queue at every lane count above one.
struct RlxdQueueProducer<R: Send + 'static> {
  txs: Vec<UnboundedAsyncSender<QueueMessage<R>>>,
  next_lane: Arc<AtomicUsize>,
  gate: Arc<CapacityGate>,
}

impl<R: Send + 'static> Clone for RlxdQueueProducer<R> {
  fn clone(&self) -> Self {
    RlxdQueueProducer {
      txs: self.txs.clone(),
      next_lane: self.next_lane.clone(),
      gate: self.gate.clone(),
    }
  }
}

impl<R: Send + 'static> RlxdQueueProducer<R> {
  async fn send(&self, task: ManagedTaskInternal<R>, shutdown_token: &CancellationToken) -> Result<(), PoolError> {
    if shutdown_token.is_cancelled() || self.is_closed() {
      return Err(PoolError::PoolShuttingDown);
    }

    let temp_permit_guard;
    tokio::select! {
        biased;
        guard = self.gate.acquire() => {
            temp_permit_guard = guard;
        },
        _ = shutdown_token.cancelled() => return Err(PoolError::PoolShuttingDown),
    };

    let message = QueueMessage {
      task,
      _permit: QueuePermit {
        gate: self.gate.clone(),
      },
    };

    let lane = self.route() % self.txs.len();
    let mut tx = self.txs[lane].clone();
    if tx.send(message).await.is_ok() {
      // The permit now travels with the queued message and is released on dequeue.
      std::mem::forget(temp_permit_guard);
      Ok(())
    } else {
      Err(PoolError::QueueSendChannelClosed)
    }
  }

  fn route(&self) -> usize {
    self.next_lane.fetch_add(1, AtomicOrdering::Relaxed)
  }

  fn close(&mut self) {
    for tx in &mut self.txs {
      let _ = tx.close();
    }
  }

  fn is_closed(&self) -> bool {
    self.txs.iter().all(UnboundedAsyncSender::is_closed)
  }

  fn len(&self) -> usize {
    self.txs.iter().map(UnboundedAsyncSender::len).sum()
  }
}

/// A relaxed-FIFO, Tokio-based pool for managing the concurrent execution of futures.
///
/// Identical to [`FuturePoolManager`](crate::FuturePoolManager) in features and API,
/// except dispatch runs on `dispatchers` concurrent loops instead of one, trading
/// strict start-order FIFO for dispatch throughput. See the module docs for what the
/// relaxation means precisely.
#[derive(Clone)]
pub struct FuturePoolManagerRlxd<R: Send + 'static> {
  shutdown_guard: Arc<()>,
  pool_name: Arc<String>,
  concurrency_gate: Arc<CapacityGate>,
  task_queue: RlxdQueueProducer<R>,
  active_task_info: ActiveTaskInfo,
  shutdown_token: CancellationToken,
  worker_join_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
  tokio_handle: TokioHandle,
  next_task_id: Arc<AtomicU64>,
  completion_notifier: Arc<CompletionNotifier>,
  notification_sink: SharedCompletionSink,
}

impl<R: Send + 'static> FuturePoolManagerRlxd<R> {
  /// Creates a new `FuturePoolManagerRlxd` running `dispatchers` dispatch loops.
  ///
  /// `dispatchers` is clamped to at least 1; 1 behaves like `FuturePoolManager`
  /// except that dequeue order is still strict, so it is strictly FIFO too.
  ///
  /// Measured under sustained load (M4 Pro, high power, trivial tasks, 500K/run, 20
  /// pairs): with the direct-dispatch fast path, 2 lanes run ~570 ns/task (1.75 M/s)
  /// and beat the strict pool by 130-165 ns/task on both a default and a 4-thread
  /// runtime. The fast path alone is worth ~130-140 ns/task over queued dispatch.
  ///
  /// Lane-count sweep (1 and 4 submitters): 1 and 2 lanes are within noise of each
  /// other, 4+ lanes measure strictly worse (idle park/wake churn at light load,
  /// dispatcher thrash under submitter pressure). 2 is a sensible default: free when
  /// the fast path is absorbing traffic, and worth ~110-124 ns/task over 1 lane when
  /// dispatch itself saturates.
  pub fn new(
    concurrency_limit: usize,
    queue_capacity: usize,
    dispatchers: usize,
    tokio_handle: TokioHandle,
    pool_name: &str,
  ) -> Self {
    let pool_name_arc = Arc::new(pool_name.to_string());
    let shutdown_token = CancellationToken::new();
    let dispatchers = dispatchers.max(1);

    let mut lane_txs = Vec::with_capacity(dispatchers);
    let mut lane_rxs = Vec::with_capacity(dispatchers);
    for _ in 0..dispatchers {
      let (tx, rx) = fibre::mpsc::unbounded_async::<QueueMessage<R>>();
      lane_txs.push(tx);
      lane_rxs.push(rx);
    }
    let queue_gate = Arc::new(CapacityGate::new(queue_capacity.max(1)));

    let notification_sink: SharedCompletionSink = Arc::new(OnceLock::new());
    let notifier_arc = CompletionNotifier::new(
      notification_sink.clone(),
      tokio_handle.clone(),
      shutdown_token.clone(),
      pool_name_arc.clone(),
    );

    let manager = Self {
      shutdown_guard: Arc::new(()),
      pool_name: pool_name_arc,
      concurrency_gate: Arc::new(CapacityGate::new(concurrency_limit.max(1))),
      task_queue: RlxdQueueProducer {
        txs: lane_txs,
        next_lane: Arc::new(AtomicUsize::new(0)),
        gate: queue_gate,
      },
      active_task_info: Arc::new(
        (0..dispatchers)
          .map(|_| DeferredRegistry::with_capacity(concurrency_limit.max(1)))
          .collect(),
      ),
      shutdown_token: shutdown_token.clone(),
      worker_join_handles: Arc::new(Mutex::new(Vec::with_capacity(dispatchers))),
      tokio_handle: tokio_handle.clone(),
      next_task_id: Arc::new(AtomicU64::new(0)),
      completion_notifier: notifier_arc,
      notification_sink,
    };

    let mut handles = Vec::with_capacity(dispatchers);
    for (dispatcher_index, lane_rx) in lane_rxs.into_iter().enumerate() {
      let handle = tokio_handle.spawn(
        Self::run_worker_loop(
          manager.pool_name.clone(),
          manager.concurrency_gate.clone(),
          lane_rx,
          tokio_handle.clone(),
          ShardHandle {
            shards: manager.active_task_info.clone(),
            index: dispatcher_index,
          },
          shutdown_token.clone(),
          manager.notification_sink.clone(),
        )
        .instrument(info_span!("rlxd_pool_worker_loop", name = %pool_name, dispatcher = dispatcher_index)),
      );
      handles.push(handle);
    }
    *manager.worker_join_handles.lock() = handles;

    manager
  }

  /// Returns the configured name of the pool.
  pub fn name(&self) -> &str {
    &self.pool_name
  }

  /// Returns the current number of tasks actively running.
  pub fn active_task_count(&self) -> usize {
    self.active_task_info.iter().map(DeferredRegistry::len).sum()
  }

  /// Returns the approximate number of tasks waiting in the queue.
  pub fn queued_task_count(&self) -> usize {
    self.task_queue.len()
  }

  /// Submits a boxed future to the pool for execution, waiting if the queue is full.
  pub async fn submit(
    &self,
    labels: HashSet<TaskLabel>,
    task_future: TaskToExecute<R>,
  ) -> Result<TaskHandle<R>, PoolError> {
    self.submit_inner(labels, task_future, |future| future).await
  }

  /// Like [`submit`](Self::submit), but takes the future unboxed. When the task is
  /// dispatched directly (queue empty, capacity free) it runs without the `Box::pin`
  /// allocation; it is boxed only if it has to queue.
  pub async fn submit_future<F>(&self, labels: HashSet<TaskLabel>, task_future: F) -> Result<TaskHandle<R>, PoolError>
  where
    F: Future<Output = R> + Send + 'static,
  {
    self.submit_inner(labels, task_future, |future| Box::pin(future)).await
  }

  async fn submit_inner<F>(
    &self,
    labels: HashSet<TaskLabel>,
    task_future: F,
    into_boxed: impl FnOnce(F) -> TaskToExecute<R>,
  ) -> Result<TaskHandle<R>, PoolError>
  where
    F: Future<Output = R> + Send + 'static,
  {
    if self.shutdown_token.is_cancelled() || self.task_queue.is_closed() {
      warn!(pool_name = %self.pool_name, "Submit: Attempted to submit task to a pool that is shutting down or closed.");
      return Err(PoolError::PoolShuttingDown);
    }

    let task_id = self.next_task_id.fetch_add(1, AtomicOrdering::Relaxed);
    let core = Arc::new(TaskCore::new(task_id, labels));
    let (result_tx, result_rx) = exclusive::<Result<R, PoolError>>();

    // Fast path: with nothing queued and capacity free, spawn directly and skip the
    // queue hop (and, for `submit_future`, the boxing) entirely. Bypassing only when
    // the queue is empty means no queued task is ever overtaken.
    if self.task_queue.len() == 0
      && let Some(permit) = self.concurrency_gate.try_acquire_owned()
    {
      let shard_index = self.task_queue.route() % self.active_task_info.len();
      let shard = ShardHandle {
        shards: self.active_task_info.clone(),
        index: shard_index,
      };
      debug!(pool_name = %self.pool_name, %task_id, "Submitting task via direct dispatch.");
      Self::spawn_task(
        DispatchContext {
          tokio_handle: &self.tokio_handle,
          pool_name: &self.pool_name,
          notification_sink: &self.notification_sink,
          shard: &shard,
        },
        core.clone(),
        task_future,
        Some(result_tx),
        permit,
      );
      return Ok(TaskHandle {
        core,
        result_receiver: Some(result_rx),
        is_detached: false,
      });
    }

    let managed_task = ManagedTaskInternal {
      core: core.clone(),
      future: into_boxed(task_future),
      result_sender: Some(result_tx),
    };

    debug!(pool_name = %self.pool_name, %task_id, labels = ?core.labels(), "Submitting task to queue.");

    match self.task_queue.send(managed_task, &self.shutdown_token).await {
      Ok(()) => Ok(TaskHandle {
        core,
        result_receiver: Some(result_rx),
        is_detached: false,
      }),
      Err(e) => {
        error!(pool_name = %self.pool_name, %task_id, "Submit: Failed to send task to queue. Error: {:?}", e);
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

  /// Registers a handler invoked on task completion, cancellation, or panic.
  ///
  /// Handlers only observe tasks that complete after registration: the pool does not
  /// record completions while no handler is registered.
  pub fn add_completion_handler(&self, handler: impl Fn(TaskCompletionInfo) + Send + Sync + 'static) {
    self.completion_notifier.add_handler(handler);
  }

  /// Shuts down the pool, consuming this instance. See
  /// [`FuturePoolManager::shutdown`](crate::FuturePoolManager::shutdown).
  pub async fn shutdown(mut self, mode: ShutdownMode) -> Result<(), PoolError> {
    let already_initiating_shutdown = self.shutdown_token.is_cancelled();

    if !already_initiating_shutdown {
      info!(pool_name = %self.pool_name, "Initiating explicit pool shutdown (mode: {:?}).", mode);
      self.shutdown_token.cancel();
      self.task_queue.close();

      if mode == ShutdownMode::ForcefulCancel {
        let tasks_to_cancel: Vec<Arc<TaskCore>> = self
          .active_task_info
          .iter()
          .flat_map(DeferredRegistry::all_tokens)
          .collect();
        for core in tasks_to_cancel {
          debug!(pool_name = %self.pool_name, task_id = %core.task_id(), "Forcefully cancelling active task during shutdown.");
          core.cancel();
        }
      }
    } else {
      info!(pool_name = %self.pool_name, "Shutdown already in progress.");
    }

    let initially_active = self.active_task_count();
    if initially_active > 0 {
      info!(pool_name = %self.pool_name, "Waiting for {} active task(s) to complete...", initially_active);
      let mut check_interval = tokio::time::interval(Duration::from_millis(50));
      let shutdown_wait_timeout = tokio::time::sleep(Duration::from_secs(30));
      tokio::pin!(shutdown_wait_timeout);

      loop {
        tokio::select! {
            _ = &mut shutdown_wait_timeout => {
                let remaining = self.active_task_count();
                warn!(pool_name = %self.pool_name, "Timeout waiting for active tasks to complete during shutdown. {} tasks still active.", remaining);
                break;
            }
            _ = check_interval.tick() => {
                if self.active_task_count() == 0 {
                    info!(pool_name = %self.pool_name, "All active tasks have completed.");
                    break;
                }
            }
        }
      }
    }

    let handles_to_await: Vec<JoinHandle<()>> = {
      let mut guard = self.worker_join_handles.lock();
      guard.drain(..).collect()
    };

    for handle in handles_to_await {
      if let Err(join_error) = timeout(Duration::from_secs(5), handle).await {
        error!(pool_name = %self.pool_name, "Timeout or error joining dispatcher loop: {:?}.", join_error);
      }
    }

    if !already_initiating_shutdown
      && let Some(sink) = self.notification_sink.get()
    {
      sink.close();
    }

    if timeout(Duration::from_secs(5), self.completion_notifier.await_shutdown())
      .await
      .is_err()
    {
      error!(pool_name = %self.pool_name, "Timeout waiting for completion notifier to shutdown.");
    }

    Ok(())
  }

  fn cancel_tasks_by_labels_internal(&self, labels_to_cancel: &HashSet<TaskLabel>) {
    if labels_to_cancel.is_empty() {
      return;
    }
    let matched: Vec<Arc<TaskCore>> = self
      .active_task_info
      .iter()
      .flat_map(|shard| shard.tokens_matching(labels_to_cancel))
      .collect();
    for core in matched {
      debug!(pool_name = %self.pool_name, task_id = %core.task_id(), "Signaling cancellation for active task due to label match.");
      core.cancel();
    }
  }

  async fn run_worker_loop(
    pool_name: Arc<String>,
    concurrency_gate: Arc<CapacityGate>,
    mut task_queue_rx: UnboundedAsyncReceiver<QueueMessage<R>>,
    tasks_tokio_handle: TokioHandle,
    shard: ShardHandle,
    shutdown_token: CancellationToken,
    notification_sink: SharedCompletionSink,
  ) {
    info!(name = %*pool_name, "Dispatcher loop started.");

    // Productive arms poll first so the token arm only registers a waiter when the
    // loop actually parks. Unlike the strict pool, the permit is acquired AFTER the
    // task is dequeued: an idle lane must hold nothing, or a dispatcher parked on an
    // empty lane would pin a concurrency permit and starve the other lanes' tasks of
    // capacity (up to N-1 permits lost, and a lane stalled until some task finishes).
    loop {
      if shutdown_token.is_cancelled() {
        break;
      }

      let managed_task_option = tokio::select! {
          biased;
          recv_result = task_queue_rx.recv() => {
              match recv_result {
                  Ok(_message) if shutdown_token.is_cancelled() => None,
                  Ok(message) => Some(message.task),
                  Err(_) => {
                      info!(name = %*pool_name, "Task queue closed and empty. Dispatcher loop will exit.");
                      None
                  }
              }
          }
          _ = shutdown_token.cancelled() => None,
      };

      if let Some(mut managed_task) = managed_task_option {
        if managed_task.core.is_cancelled() {
          debug!(name = %*pool_name, task_id = managed_task.core.task_id(), "Dequeued task already cancelled.");
          if let Some(tx) = managed_task.result_sender.take() {
            let _ = tx.send(Err(PoolError::TaskCancelled));
          }

          if let Some(mut noti_tx) = notification_sink.get().and_then(CompletionSink::sender) {
            let completion_msg = InternalCompletionMessage {
              pool_name: pool_name.clone(),
              core: managed_task.core.clone(),
              status: TaskCompletionStatus::Cancelled,
            };
            if noti_tx.send(completion_msg).await.is_err() {
              error!(pool_name = %*pool_name, "Failed to send completion for pre-cancelled task.");
            }
          }
          continue;
        }

        let concurrency_permit = tokio::select! {
            biased;
            permit = concurrency_gate.clone().acquire_owned() => permit,
            _ = shutdown_token.cancelled() => {
                info!(name = %*pool_name, "Shutdown signal (token) received while waiting for capacity. Dropping dequeued task.");
                break;
            }
        };

        if shutdown_token.is_cancelled() {
          info!(name = %*pool_name, "Shutdown signal (token) received. Dropping dequeued task.");
          break;
        }

        let ManagedTaskInternal {
          core,
          future,
          result_sender,
        } = managed_task;
        Self::spawn_task(
          DispatchContext {
            tokio_handle: &tasks_tokio_handle,
            pool_name: &pool_name,
            notification_sink: &notification_sink,
            shard: &shard,
          },
          core,
          future,
          result_sender,
          concurrency_permit,
        );
      } else {
        break;
      }
    }

    info!(name = %*pool_name, "Dispatcher loop stopped.");
  }

  fn spawn_task<F>(
    context: DispatchContext<'_>,
    core: Arc<TaskCore>,
    future: F,
    result_sender: Option<ExclusiveSender<Result<R, PoolError>>>,
    concurrency_permit: crate::capacity_gate::OwnedPermitGuard,
  ) where
    F: Future<Output = R> + Send + 'static,
  {
    let DispatchContext {
      tokio_handle,
      pool_name,
      notification_sink,
      shard,
    } = context;
    let task_id = core.task_id();

    shard.insert(core.clone());

    let pool_name_for_notification = pool_name.clone();
    let pool_name_for_task_execution = pool_name.clone();

    tokio_handle.spawn({
      let notification_tx_for_spawned_task = notification_sink.get().and_then(CompletionSink::sender);
      let shard = shard.clone();

      async move {
        let _permit_guard = concurrency_permit;
        let guarded_future = AssertUnwindSafe(future).catch_unwind();
        tokio::pin!(guarded_future);
        let execution_outcome: Result<R, PoolError> = tokio::select! {
            biased;
            task_result = &mut guarded_future => {
              if core.is_cancelled() {
                Err(PoolError::TaskCancelled)
              } else {
                match task_result {
                  Ok(actual_result) => Ok(actual_result),
                  Err(_panic_payload) => {
                    error!(pool_name = %*pool_name_for_task_execution, %task_id, "Task panicked during execution.");
                    Err(PoolError::TaskPanicked)
                  }
                }
              }
            },
            _ = core.cancelled() => Err(PoolError::TaskCancelled),
        };

        let completion_status = TaskCompletionStatus::from(&execution_outcome);
        if let Some(tx_result) = result_sender
          && tx_result.send(execution_outcome).is_err()
        {
          trace!(pool_name = %*pool_name_for_task_execution, %task_id, "Result receiver for task handle was dropped.");
        }

        if let Some(mut noti_tx) = notification_tx_for_spawned_task {
          let completion_msg = InternalCompletionMessage {
            pool_name: pool_name_for_notification,
            core: core.clone(),
            status: completion_status,
          };

          if noti_tx.send(completion_msg).await.is_err() {
            error!(pool_name = %*pool_name_for_task_execution, %task_id, "Failed to send completion notification for task.");
          }
        }

        core.mark_finished();
        shard.finish();
        debug!(name = %*pool_name_for_task_execution, %task_id, "Managed task finished.");
      }
      .instrument(info_span!("rlxd_managed_task", pool_name = %**pool_name, %task_id))
    });
  }
}

impl<R: Send + 'static> Drop for FuturePoolManagerRlxd<R> {
  fn drop(&mut self) {
    if Arc::strong_count(&self.shutdown_guard) > 1 {
      return;
    }

    if !self.shutdown_token.is_cancelled() {
      info!(pool_name = %*self.pool_name, "FuturePoolManagerRlxd instance dropped. Initiating implicit shutdown.");
      self.shutdown_token.cancel();
      self.task_queue.close();
      if let Some(sink) = self.notification_sink.get() {
        sink.close();
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::AtomicU64;

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn submit_future_works_on_both_paths() {
    let manager = FuturePoolManagerRlxd::<u32>::new(2, 32, 2, TokioHandle::current(), "rlxd_unboxed");

    let mut blockers = Vec::new();
    for _ in 0..2 {
      blockers.push(
        manager
          .submit_future(HashSet::new(), async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            0u32
          })
          .await
          .unwrap(),
      );
    }

    let mut queued = Vec::new();
    for i in 1..20u32 {
      queued.push(manager.submit_future(HashSet::new(), async move { i * 3 }).await.unwrap());
    }

    for handle in blockers {
      assert_eq!(handle.await_result().await, Ok(0));
    }
    for (i, handle) in queued.into_iter().enumerate() {
      assert_eq!(handle.await_result().await, Ok((i as u32 + 1) * 3));
    }

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn tasks_complete_across_dispatchers() {
    let manager = FuturePoolManagerRlxd::<u64>::new(8, 64, 4, TokioHandle::current(), "rlxd_basic");

    let mut handles = Vec::new();
    for i in 0..500u64 {
      handles.push(manager.submit(HashSet::new(), Box::pin(async move { i * 2 })).await.unwrap());
    }
    for (i, handle) in handles.into_iter().enumerate() {
      assert_eq!(handle.await_result().await, Ok(i as u64 * 2));
    }

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn cancellation_by_label_stops_running_tasks() {
    let manager = FuturePoolManagerRlxd::<u32>::new(4, 16, 2, TokioHandle::current(), "rlxd_cancel");

    let labels: HashSet<TaskLabel> = ["stoppable".to_string()].into_iter().collect();
    let mut handles = Vec::new();
    for _ in 0..4 {
      handles.push(
        manager
          .submit(
            labels.clone(),
            Box::pin(async {
              tokio::time::sleep(Duration::from_secs(30)).await;
              1u32
            }),
          )
          .await
          .unwrap(),
      );
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    manager.cancel_tasks_by_label(&"stoppable".to_string());

    for handle in handles {
      assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), handle.await_result())
          .await
          .expect("cancelled task did not resolve"),
        Err(PoolError::TaskCancelled)
      );
    }

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  }

  #[tokio::test]
  async fn no_completion_queue_without_a_handler() {
    let manager = FuturePoolManagerRlxd::<u32>::new(2, 8, 2, TokioHandle::current(), "rlxd_lazy_notifier");

    for i in 0..8u32 {
      let handle = manager.submit(HashSet::new(), Box::pin(async move { i })).await.unwrap();
      assert_eq!(handle.await_result().await, Ok(i));
    }
    assert!(manager.notification_sink.get().is_none());

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn completion_handler_sees_every_task() {
    let manager = FuturePoolManagerRlxd::<u32>::new(4, 32, 3, TokioHandle::current(), "rlxd_handler");

    let seen = Arc::new(AtomicU64::new(0));
    let seen_in_handler = seen.clone();
    manager.add_completion_handler(move |_| {
      seen_in_handler.fetch_add(1, AtomicOrdering::Relaxed);
    });

    let mut handles = Vec::new();
    for i in 0..100u32 {
      handles.push(manager.submit(HashSet::new(), Box::pin(async move { i })).await.unwrap());
    }
    for handle in handles {
      handle.await_result().await.unwrap();
    }

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();
    assert_eq!(seen.load(AtomicOrdering::Relaxed), 100);
  }

  #[tokio::test]
  async fn shutdown_completes_while_other_pool_clone_alive() {
    let manager = FuturePoolManagerRlxd::<u32>::new(1, 8, 2, TokioHandle::current(), "rlxd_live_clone");
    let clone = manager.clone();

    let handle = manager.submit(HashSet::new(), Box::pin(async { 5u32 })).await.unwrap();
    assert_eq!(handle.await_result().await, Ok(5));

    tokio::time::timeout(Duration::from_secs(10), manager.shutdown(ShutdownMode::Graceful))
      .await
      .expect("shutdown() deadlocked while another pool clone was alive")
      .unwrap();

    drop(clone);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn forceful_shutdown_cancels_running_tasks() {
    let manager = FuturePoolManagerRlxd::<u32>::new(4, 16, 2, TokioHandle::current(), "rlxd_forceful");

    let mut handles = Vec::new();
    for _ in 0..4 {
      handles.push(
        manager
          .submit(
            HashSet::new(),
            Box::pin(async {
              tokio::time::sleep(Duration::from_secs(30)).await;
              1u32
            }),
          )
          .await
          .unwrap(),
      );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    tokio::time::timeout(Duration::from_secs(10), manager.shutdown(ShutdownMode::ForcefulCancel))
      .await
      .expect("forceful shutdown hung")
      .unwrap();

    for handle in handles {
      assert_eq!(handle.await_result().await, Err(PoolError::TaskCancelled));
    }
  }

  #[tokio::test]
  async fn panicking_task_reports_and_pool_survives() {
    let manager = FuturePoolManagerRlxd::<u32>::new(2, 8, 2, TokioHandle::current(), "rlxd_panic");

    let bad = manager
      .submit(HashSet::new(), Box::pin(async { panic!("boom") }))
      .await
      .unwrap();
    assert_eq!(bad.await_result().await, Err(PoolError::TaskPanicked));

    let good = manager.submit(HashSet::new(), Box::pin(async { 7u32 })).await.unwrap();
    assert_eq!(good.await_result().await, Ok(7));

    manager.shutdown(ShutdownMode::Graceful).await.unwrap();
  }
}
