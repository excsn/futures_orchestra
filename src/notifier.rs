use crate::error::PoolError;
use crate::task::TaskLabel;
use fibre::mpsc::{AsyncReceiver, RecvError};
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex, Once, RwLock};
use std::time::{Duration, SystemTime};
use tokio::runtime::Handle as TokioHandle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, info_span, trace, warn, Instrument};

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

// --- CompletionNotifier Struct ---

struct NotifierInternalState {
  internal_rx_for_init: Option<AsyncReceiver<InternalCompletionMessage>>,
  tokio_handle: TokioHandle,
  pool_shutdown_token: CancellationToken,
  pool_name_for_logging: Arc<String>,
  worker_join_handle: Option<JoinHandle<()>>,
}

pub(crate) struct CompletionNotifier {
  handlers: Arc<RwLock<Vec<Arc<dyn Fn(TaskCompletionInfo) + Send + Sync + 'static>>>>,
  init_once: Once,
  internal_state_for_init: StdMutex<NotifierInternalState>,
}

impl fmt::Debug for CompletionNotifier {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Attempt to get handler count without blocking for too long or panicking if poisoned
    let handler_count = self.handlers.try_read().map_or(0, |guard| guard.len());
    let initialized = self.init_once.is_completed(); // Check if init_once has run

    f.debug_struct("CompletionNotifier")
      .field("handler_count", &handler_count)
      .field("initialized", &initialized)
      // internal_state_for_init contains non-Debug JoinHandle, so we summarize
      .field("worker_status", &"details_in_internal_state")
      .finish()
  }
}

impl fmt::Debug for NotifierInternalState {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("NotifierInternalState")
      .field("internal_rx_for_init_is_some", &self.internal_rx_for_init.is_some())
      // tokio_handle, pool_shutdown_token don't typically need detailed debug
      .field("pool_name_for_logging", &self.pool_name_for_logging)
      .field("worker_join_handle_is_some", &self.worker_join_handle.is_some())
      .finish()
  }
}

impl CompletionNotifier {
  pub(crate) fn new(
    internal_rx: AsyncReceiver<InternalCompletionMessage>,
    tokio_handle: TokioHandle,
    pool_shutdown_token: CancellationToken,
    pool_name_for_logging: Arc<String>,
  ) -> Arc<Self> {
    Arc::new(Self {
      handlers: Arc::new(RwLock::new(Vec::new())),
      init_once: Once::new(),
      internal_state_for_init: StdMutex::new(NotifierInternalState {
        internal_rx_for_init: Some(internal_rx),
        tokio_handle,
        pool_shutdown_token,
        pool_name_for_logging,
        worker_join_handle: None,
      }),
    })
  }

  fn ensure_worker_initialized(&self) {
    self.init_once.call_once(|| {
      let mut state_guard = self.internal_state_for_init.lock().unwrap();
      if let Some(rx_to_use) = state_guard.internal_rx_for_init.take() {
        info!(pool_name = %*state_guard.pool_name_for_logging, "First completion handler added or initialization triggered. Initializing notification worker.");

        let worker_handlers = self.handlers.clone();
        let worker_tokio_handle = state_guard.tokio_handle.clone();
        let worker_shutdown_token = state_guard.pool_shutdown_token.clone();
        let worker_pool_name = state_guard.pool_name_for_logging.clone();

        let worker_jh = state_guard.tokio_handle.spawn(
          Self::run_notification_worker_loop(
            rx_to_use,
            worker_handlers,
            worker_tokio_handle,
            worker_shutdown_token,
          )
          .instrument(info_span!("notification_worker_loop", pool_name = %*worker_pool_name)),
        );
        state_guard.worker_join_handle = Some(worker_jh);
      } else {
        warn!(pool_name = %*state_guard.pool_name_for_logging, "Notifier initialization: RX already taken, worker might have been initialized concurrently (unexpected with Once).");
      }
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

  async fn run_notification_worker_loop(
    mut queue_rx: AsyncReceiver<InternalCompletionMessage>,
    handlers_list_arc: Arc<RwLock<Vec<Arc<dyn Fn(TaskCompletionInfo) + Send + Sync + 'static>>>>,
    tokio_handle_for_spawning_handlers: TokioHandle,
    pool_shutdown_token: CancellationToken, // Kept for logging, but primary exit is queue closure
  ) {
    info!("Notification worker started. Will process messages until its input queue is closed by all senders.");
    let mut pool_shutdown_signaled_once = false;

    // Helper closure to process a single message
    let process_message = |internal_msg_payload: InternalCompletionMessage, _is_drain: bool| {
      // _is_drain flag is not strictly needed anymore with this loop structure, but kept for consistency if logging differs.
      // let log_prefix = if is_drain { "(drain)" } else { "" };
      trace!(
        "Notification worker: processing message for task_id: {}",
        internal_msg_payload.task_id
      );

      let handlers_guard = handlers_list_arc.read().unwrap();
      if handlers_guard.is_empty() {
        trace!(
          task_id = %internal_msg_payload.task_id,
          "No completion handlers registered, dropping notification."
        );
        return; // Early return if no handlers
      }

      let public_info = TaskCompletionInfo {
        task_id: internal_msg_payload.task_id,
        pool_name: internal_msg_payload.pool_name.clone(),
        labels: internal_msg_payload.labels.clone(),
        status: internal_msg_payload.status,
        completion_time: SystemTime::now(),
      };

      debug!(
        task_id = %public_info.task_id,
        "Dispatching notification to {} handlers.",
        handlers_guard.len()
      );

      for handler_arc in handlers_guard.iter() {
        let current_handler_clone = handler_arc.clone();
        let info_clone_for_handler = public_info.clone();

        tokio_handle_for_spawning_handlers.spawn(async move {
          let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            current_handler_clone(info_clone_for_handler.clone());
          }));
          if result.is_err() {
            error!(
              "A completion handler panicked during execution. Pool: {}, Task ID: {}",
              info_clone_for_handler.pool_name, info_clone_for_handler.task_id
            );
          }
        });
      }
    };

    loop {
      tokio::select! {
        biased; // Prioritize receiving a message if one is ready.

        recv_result = queue_rx.recv() => {
          match recv_result {
            Ok(internal_msg_payload) => {
              process_message(internal_msg_payload, false); // false indicates not explicitly in a "drain" phase by name
            }
            Err(receive_error) => {
              // This error (e.g., ReceiveError::Closed or ReceiveError::SendClosed)
              // means the channel is closed AND empty. All senders must have been dropped.
              // This is the primary, correct way for this loop to terminate.
              match receive_error {
                RecvError::Disconnected => {
                    info!("Notification worker: Message queue explicitly closed or all senders dropped (Closed from recv()). Terminating.");
                }
              }
              break; // Exit the loop, worker will stop.
            }
          }
        },
        // This branch is now secondary and mostly for logging.
        // The loop continues until queue_rx.recv() errors out.
        _ = pool_shutdown_token.cancelled(), if !pool_shutdown_signaled_once => {
          info!("Notification worker: Detected that the pool's global shutdown token is cancelled. Worker will continue to process messages until its input queue is fully closed by all senders dropping their references.");
          pool_shutdown_signaled_once = true;
          // DO NOT break the loop here.
        }
      }
    }

    // No separate drain loop is strictly necessary if queue_rx.recv() only returns Err
    // when the channel is closed and empty, as Kanal's recv() is expected to do.
    // Any messages sent before the last sender was dropped would have been processed
    // by the main loop above.
    info!("Notification worker stopped (input queue fully closed and processed).");
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
