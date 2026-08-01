use crate::error::PoolError;
use crate::task::TaskLabel;

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock};
use std::time::SystemTime;

use fibre::mpsc::{self, UnboundedAsyncReceiver, UnboundedAsyncSender, RecvError};
use parking_lot::Mutex;
use tokio::runtime::Handle as TokioHandle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, info_span, trace, Instrument};

// --- Public Event Structs for Handlers ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCompletionStatus {
  Success,
  Cancelled,
  Panicked,
  PoolErrorOccurred,
}

impl<R> From<&Result<R, PoolError>> for TaskCompletionStatus {
  fn from(result: &Result<R, PoolError>) -> Self {
    match result {
      Ok(_) => TaskCompletionStatus::Success,
      Err(PoolError::TaskCancelled) => TaskCompletionStatus::Cancelled,
      Err(PoolError::TaskPanicked) => TaskCompletionStatus::Panicked,
      Err(_) => TaskCompletionStatus::PoolErrorOccurred,
    }
  }
}

#[derive(Debug, Clone)]
pub struct TaskCompletionInfo {
  pub task_id: u64,
  pub pool_name: Arc<String>,
  pub labels: Arc<HashSet<TaskLabel>>,
  pub status: TaskCompletionStatus,
  pub completion_time: SystemTime,
}

// --- Internal Message (crate-public) ---
#[derive(Debug)]
pub(crate) struct InternalCompletionMessage {
  pub(crate) task_id: u64,
  pub(crate) pool_name: Arc<String>,
  pub(crate) labels: Arc<HashSet<TaskLabel>>,
  pub(crate) status: TaskCompletionStatus,
}

// --- Completion Sink ---

/// Holds the pool-wide sender for the completion queue.
///
/// Lives inside a `OnceLock`, giving three distinct states: the cell is unset while no
/// completion handler has ever been registered (no channel exists and producers stay
/// silent), it holds `Some` sender once a handler has been added, and it holds `None`
/// after shutdown. The last two must stay distinguishable so a handler registered after
/// shutdown cannot resurrect the channel.
pub(crate) struct CompletionSink {
  tx: Mutex<Option<UnboundedAsyncSender<InternalCompletionMessage>>>,
}

impl CompletionSink {
  fn new(tx: UnboundedAsyncSender<InternalCompletionMessage>) -> Self {
    Self {
      tx: Mutex::new(Some(tx)),
    }
  }

  pub(crate) fn sender(&self) -> Option<UnboundedAsyncSender<InternalCompletionMessage>> {
    self.tx.lock().clone()
  }

  /// Drops the pool-wide sender so the queue disconnects once every per-task clone is
  /// gone, which is what terminates the notification worker.
  pub(crate) fn close(&self) {
    drop(self.tx.lock().take());
  }
}

impl fmt::Debug for CompletionSink {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("CompletionSink")
      .field("open", &self.tx.lock().is_some())
      .finish()
  }
}

pub(crate) type SharedCompletionSink = Arc<OnceLock<CompletionSink>>;

// --- CompletionNotifier Struct ---

struct NotifierInternalState {
  tokio_handle: TokioHandle,
  pool_shutdown_token: CancellationToken,
  pool_name_for_logging: Arc<String>,
  worker_join_handle: Option<JoinHandle<()>>,
}

type CompletionHandler = Arc<dyn Fn(TaskCompletionInfo) + Send + Sync + 'static>;
type HandlerList = Arc<RwLock<Vec<CompletionHandler>>>;

pub(crate) struct CompletionNotifier {
  handlers: HandlerList,
  sink: SharedCompletionSink,
  internal_state_for_init: StdMutex<NotifierInternalState>,
}

impl fmt::Debug for CompletionNotifier {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let handler_count = self.handlers.try_read().map_or(0, |guard| guard.len());
    let initialized = self.sink.get().is_some();

    f.debug_struct("CompletionNotifier")
      .field("handler_count", &handler_count)
      .field("initialized", &initialized)
      .field("worker_status", &"details_in_internal_state")
      .finish()
  }
}

impl fmt::Debug for NotifierInternalState {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("NotifierInternalState")
      .field("pool_name_for_logging", &self.pool_name_for_logging)
      .field("worker_join_handle_is_some", &self.worker_join_handle.is_some())
      .finish()
  }
}

impl CompletionNotifier {
  pub(crate) fn new(
    sink: SharedCompletionSink,
    tokio_handle: TokioHandle,
    pool_shutdown_token: CancellationToken,
    pool_name_for_logging: Arc<String>,
  ) -> Arc<Self> {
    Arc::new(Self {
      handlers: Arc::new(RwLock::new(Vec::new())),
      sink,
      internal_state_for_init: StdMutex::new(NotifierInternalState {
        tokio_handle,
        pool_shutdown_token,
        pool_name_for_logging,
        worker_join_handle: None,
      }),
    })
  }

  fn ensure_worker_initialized(&self) {
    self.sink.get_or_init(|| {
      let mut state_guard = self.internal_state_for_init.lock().unwrap();
      info!(pool_name = %*state_guard.pool_name_for_logging, "First completion handler added. Initializing notification worker.");

      let (tx, rx) = mpsc::unbounded_async::<InternalCompletionMessage>();

      let worker_handlers = self.handlers.clone();
      let worker_shutdown_token = state_guard.pool_shutdown_token.clone();
      let worker_pool_name = state_guard.pool_name_for_logging.clone();

      let worker_jh = state_guard.tokio_handle.spawn(
        Self::run_notification_worker_loop(rx, worker_handlers, worker_shutdown_token)
          .instrument(info_span!("notification_worker_loop", pool_name = %*worker_pool_name)),
      );
      state_guard.worker_join_handle = Some(worker_jh);

      CompletionSink::new(tx)
    });
  }

  pub(crate) fn add_handler(&self, handler: impl Fn(TaskCompletionInfo) + Send + Sync + 'static) {
    self.ensure_worker_initialized();

    let pool_name_for_logging = {
      let state_guard = self.internal_state_for_init.lock().unwrap();
      state_guard.pool_name_for_logging.clone()
    };

    let mut handlers_guard = self.handlers.write().unwrap();
    handlers_guard.push(Arc::new(handler));
    info!(pool_name = %*pool_name_for_logging, "Notifier: Added new completion handler. Total handlers: {}", handlers_guard.len());
  }

  /// The main worker loop for processing completion notifications.
  ///
  /// This loop receives completion messages and executes all registered handlers
  /// sequentially within its own task.
  ///
  /// IMPORTANT:
  /// - Handlers are expected to be **non-blocking**. A slow handler will delay
  ///   all subsequent handlers for the same event and the processing of future events.
  /// - If a handler needs to perform long-running or blocking work, it should
  ///   spawn its own task (e.g., using `tokio::spawn`).
  /// - Panics within handlers are caught and logged, preventing them from crashing
  ///   the entire notification system.
  async fn run_notification_worker_loop(
    mut queue_rx: UnboundedAsyncReceiver<InternalCompletionMessage>,
    handlers_list_arc: HandlerList,
    pool_shutdown_token: CancellationToken,
  ) {
    info!("Notification worker started. Will process messages until its input queue is closed.");
    let mut pool_shutdown_signaled_once = false;

    loop {
      tokio::select! {
        biased;

        recv_result = queue_rx.recv() => {
          match recv_result {
            Ok(internal_msg) => {
              trace!("Notification worker: processing message for task_id: {}", internal_msg.task_id);

              let handlers_guard = handlers_list_arc.read().unwrap();
              if handlers_guard.is_empty() {
                trace!(task_id = %internal_msg.task_id, "No completion handlers registered, dropping notification.");
                continue;
              }

              let public_info = TaskCompletionInfo {
                task_id: internal_msg.task_id,
                pool_name: internal_msg.pool_name.clone(),
                labels: internal_msg.labels.clone(),
                status: internal_msg.status,
                completion_time: SystemTime::now(),
              };

              debug!(task_id = %public_info.task_id, "Executing {} handlers sequentially.", handlers_guard.len());

              for handler_arc in handlers_guard.iter() {
                let info_clone = public_info.clone();
                // Wrap each handler call to catch panics.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handler_arc(info_clone);
                }));

                if result.is_err() {
                    error!(
                        pool_name = %public_info.pool_name,
                        task_id = %public_info.task_id,
                        "A completion handler panicked during execution."
                    );
                }
              }
            }
            Err(RecvError::Disconnected) => {
              info!("Notification worker: Message queue closed. Terminating loop.");
              break;
            }
          }
        },
        _ = pool_shutdown_token.cancelled(), if !pool_shutdown_signaled_once => {
          info!("Notification worker: Detected pool shutdown. Will process remaining messages and then terminate.");
          pool_shutdown_signaled_once = true;
          // Do not break here. Let the queue_rx.recv() error handle termination.
        }
      }
    }
    info!("Notification worker stopped.");
  }

  pub(crate) async fn await_shutdown(&self) {
    let (handle_option, pool_name) = {
      let mut guard = self.internal_state_for_init.lock().unwrap();
      let handle = guard.worker_join_handle.take();
      let name = guard.pool_name_for_logging.clone();
      (handle, name)
    };

    if let Some(handle) = handle_option {
      info!(pool_name = %*pool_name, "Notifier: Waiting for notification worker loop to join.");
      if let Err(e) = handle.await {
        error!(pool_name = %*pool_name, "Notifier: Error joining notification worker: {:?}", e);
      } else {
        debug!(pool_name = %*pool_name, "Notifier: Notification worker loop successfully joined.");
      }
    } else {
      trace!(pool_name = %*pool_name, "Notifier: Worker was not initialized or handle already taken; no join needed.");
    }
  }
}
