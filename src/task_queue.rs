use crate::error::PoolError;
use crate::capacity_gate::CapacityGate;
use crate::task::ManagedTaskInternal;

use fibre::mpsc::{self, AsyncReceiver, AsyncSender, RecvError};
use std::fmt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// An internal message type that pairs a task with the permit it holds.
///
/// The permit is implicitly released when this message is dropped, which happens
/// after the worker loop has successfully received it from the channel. This
/// mechanism ensures that a queue slot is only freed up after a task has been
/// fully dequeued.
pub(crate) struct QueueMessage<R: Send + 'static> {
  /// The actual task to be executed.
  pub(crate) task: ManagedTaskInternal<R>,
  /// The permit acquired from the `CapacityGate`. Its `Drop` impl handles release.
  _permit: Permit,
}

impl<R: Send + 'static> fmt::Debug for QueueMessage<R> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("QueueMessage")
      .field("task_id", &self.task.task_id)
      .finish_non_exhaustive()
  }
}

/// A permit acquired from the `CapacityGate`. Its `Drop` implementation
/// calls `release()` on the gate, signaling that a queue slot is now free.
/// This struct is essential for the "hold-until-dequeue" semantic.
#[derive(Debug)]
pub(crate) struct Permit {
  gate: Arc<CapacityGate>,
}

impl Drop for Permit {
  fn drop(&mut self) {
    self.gate.release();
  }
}

/// A bounded, lock-free, multi-producer, single-consumer queue for tasks.
///
/// This queue uses a `CapacityGate` to enforce a capacity limit on top of an
/// underlying unbounded `fibre::mpsc` channel. This provides backpressure for
/// task submission without using any locks in the hot path.
#[derive(Debug)]
pub(crate) struct TaskQueue<R: Send + 'static> {
  tx: AsyncSender<QueueMessage<R>>,
  rx: AsyncReceiver<QueueMessage<R>>,
  gate: Arc<CapacityGate>,
}

impl<R: Send + 'static> TaskQueue<R> {
  /// Creates a new `TaskQueue` with a specified capacity.
  pub(crate) fn new(capacity: usize) -> Self {
    let (tx, rx) = mpsc::unbounded_async();
    Self {
      tx,
      rx,
      gate: Arc::new(CapacityGate::new(capacity)),
    }
  }

  /// Splits the queue into its producer and consumer halves.
  pub(crate) fn split(self) -> (QueueProducer<R>, QueueConsumer<R>) {
    (
      QueueProducer {
        tx: self.tx,
        gate: self.gate,
      },
      QueueConsumer { rx: self.rx },
    )
  }
}

/// The producer handle for the `TaskQueue`. It can be cloned and shared across
/// multiple submission sites.
#[derive(Clone)]
pub(crate) struct QueueProducer<R: Send + 'static> {
  tx: AsyncSender<QueueMessage<R>>,
  gate: Arc<CapacityGate>,
}

/// The consumer handle for the `TaskQueue`. It cannot be cloned, enforcing
/// the single-consumer pattern.
#[derive(Debug)]
pub(crate) struct QueueConsumer<R: Send + 'static> {
  rx: AsyncReceiver<QueueMessage<R>>,
}

impl<R: Send + 'static> fmt::Debug for QueueProducer<R> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("QueueProducer")
      .field("len", &self.len())
      .field("gate_permits", &self.gate.get_permits())
      .finish_non_exhaustive()
  }
}

impl<R: Send + 'static> QueueProducer<R> {
  /// Sends a task into the queue.
  ///
  /// This method first acquires a permit from the `CapacityGate`, asynchronously
  /// waiting if the queue is at full capacity. Once acquired, it wraps the task
  /// and a manual `Permit` into a `QueueMessage` and sends it.
  /// The responsibility for releasing the permit is transferred to the `QueueMessage`.
  pub(crate) async fn send(
    &self,
    task: ManagedTaskInternal<R>,
    shutdown_token: &CancellationToken,
  ) -> Result<(), PoolError> {
    if shutdown_token.is_cancelled() || self.tx.is_closed() {
      return Err(PoolError::PoolShuttingDown);
    }

    // This temporary RAII guard will ensure the permit is released if we exit
    // this function early due to an error.
    let temp_permit_guard;

    // Acquire a permit from the gate. This will wait if the queue is full.
    tokio::select! {
        biased;
        _ = shutdown_token.cancelled() => return Err(PoolError::PoolShuttingDown),
        guard = self.gate.acquire() => {
            temp_permit_guard = guard;
        },
    };

    // At this point, `temp_permit_guard` holds the permit.

    // Create our manual, long-lived permit that will travel with the message.
    let long_lived_permit = Permit {
      gate: self.gate.clone(),
    };
    let message = QueueMessage {
      task,
      _permit: long_lived_permit,
    };

    // Try to send the message.
    if self.tx.send(message).await.is_ok() {
      // SUCCESS: The message is now in the queue. The `long_lived_permit` inside
      // it is now responsible for releasing the gate slot. We must prevent the
      // `temp_permit_guard` from also releasing it when it goes out of scope.
      std::mem::forget(temp_permit_guard);
      Ok(())
    } else {
      // FAILURE: The receiver was dropped. The `long_lived_permit` is dropped
      // here, but so is the `temp_permit_guard`. We want only one release.
      // The `temp_permit_guard` will handle it correctly when it goes out of
      // scope at the end of the function.
      Err(PoolError::QueueSendChannelClosed)
    }
  }

  /// Closes the sending side of the queue.
  pub(crate) fn close(&self) {
    let _ = self.tx.close();
  }

  /// Returns `true` if the queue's sender has been closed.
  pub(crate) fn is_closed(&self) -> bool {
    self.tx.is_closed()
  }

  /// Returns the number of tasks in the underlying channel.
  pub(crate) fn len(&self) -> usize {
    self.tx.len()
  }
}

impl<R: Send + 'static> QueueConsumer<R> {
  /// Receives a task from the queue.
  ///
  /// The `_permit` inside the received `QueueMessage` is dropped upon successful
  /// receive, automatically calling `gate.release()` and freeing a slot.
  pub(crate) async fn recv(&self) -> Result<ManagedTaskInternal<R>, RecvError> {
    match self.rx.recv().await {
      Ok(message) => Ok(message.task),
      Err(e) => Err(e),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::task::TaskToExecute;
  use std::collections::HashSet;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::time::Duration;
  use fibre::oneshot::oneshot;

  // Helper to create a dummy ManagedTaskInternal for testing the queue.
  fn dummy_task(id: u64) -> ManagedTaskInternal<String> {
    let future: TaskToExecute<String> = Box::pin(async move { "done".to_string() });
    let (tx, _) = oneshot();
    ManagedTaskInternal {
      task_id: id,
      labels: HashSet::new(),
      future,
      token: CancellationToken::new(),
      result_sender: Some(tx),
    }
  }

  #[tokio::test]
  async fn test_queue_send_recv() {
    let queue = TaskQueue::<String>::new(5);
    let (producer, consumer) = queue.split();
    let shutdown_token = CancellationToken::new();

    assert_eq!(producer.gate.get_permits(), 5);
    producer.send(dummy_task(1), &shutdown_token).await.unwrap();
    // Permit is now held by the message in the queue.
    assert_eq!(producer.gate.get_permits(), 4);

    let received_task = consumer.recv().await.unwrap();
    assert_eq!(received_task.task_id, 1);
    // After recv, the permit is released.
    assert_eq!(producer.gate.get_permits(), 5);
  }

  #[tokio::test]
  async fn test_queue_capacity_blocks_send() {
    let queue = TaskQueue::<String>::new(1);
    let (producer, consumer) = queue.split();
    let shutdown_token = CancellationToken::new();

    // Send one task, filling the queue capacity
    producer.send(dummy_task(1), &shutdown_token).await.unwrap();
    assert_eq!(producer.gate.get_permits(), 0);

    // The next send should block. We use `tokio::select!` to test this.
    let send_future = producer.send(dummy_task(2), &shutdown_token);
    tokio::pin!(send_future);

    tokio::select! {
        _ = &mut send_future => {
            panic!("Send should have blocked because the queue is full.");
        },
        _ = tokio::time::sleep(Duration::from_millis(50)) => {
            // This is the expected outcome.
        }
    }
    assert_eq!(producer.gate.get_permits(), 0);

    // Now, receive a task, which should unblock the waiting send.
    let received_task = consumer.recv().await.unwrap();
    assert_eq!(received_task.task_id, 1);
    assert_eq!(producer.gate.get_permits(), 1);

    // The second send should now complete quickly.
    tokio::time::timeout(Duration::from_millis(50), send_future)
      .await
      .expect("Send did not complete after queue was drained.")
      .unwrap();
    assert_eq!(producer.gate.get_permits(), 0);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn test_queue_concurrent_sends() {
    let queue = TaskQueue::<String>::new(4);
    let (producer, consumer) = queue.split();
    let shutdown_token = CancellationToken::new();
    let num_tasks: u64 = 20;
    let received_count = Arc::new(AtomicUsize::new(0));

    let producer_handle = {
      let producer = producer.clone();
      tokio::spawn(async move {
        let mut handles = Vec::new();
        for i in 0..num_tasks {
          let p = producer.clone();
          let s = shutdown_token.clone();
          handles.push(tokio::spawn(async move {
            p.send(dummy_task(i), &s).await.unwrap();
          }));
        }
        for handle in handles {
          handle.await.unwrap();
        }
      })
    };

    let consumer_handle = {
      let received_count = received_count.clone();
      tokio::spawn(async move {
        for _ in 0..num_tasks {
          if consumer.recv().await.is_ok() {
            received_count.fetch_add(1, Ordering::SeqCst);
          }
        }
      })
    };

    producer_handle.await.unwrap();
    consumer_handle.await.unwrap();

    assert_eq!(received_count.load(Ordering::SeqCst), num_tasks as usize);
    assert_eq!(producer.gate.get_permits(), 4);
  }

  #[tokio::test]
  async fn test_send_respects_shutdown_token() {
    let queue = TaskQueue::<String>::new(1);
    let (producer, _consumer) = queue.split();
    let shutdown_token = CancellationToken::new();

    producer.send(dummy_task(1), &shutdown_token).await.unwrap();
    shutdown_token.cancel();

    let result = producer.send(dummy_task(2), &shutdown_token).await;
    assert!(matches!(result, Err(PoolError::PoolShuttingDown)));
    assert_eq!(
      producer.gate.get_permits(),
      0,
      "Permit should still be held by first task"
    );
  }

  #[tokio::test]
  async fn test_close_sender_stops_consumer() {
    let queue = TaskQueue::<String>::new(2);
    let (producer, consumer) = queue.split();
    let shutdown_token = CancellationToken::new();

    producer.send(dummy_task(1), &shutdown_token).await.unwrap();
    producer.close();

    assert_eq!(consumer.recv().await.unwrap().task_id, 1);
    assert_eq!(producer.gate.get_permits(), 2);

    let result = consumer.recv().await;
    assert!(matches!(result, Err(RecvError::Disconnected)));
  }
}
