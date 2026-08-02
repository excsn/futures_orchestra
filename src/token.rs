use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

/// Cancellation and completion flags for one task, embeddable in a larger allocation.
///
/// Cancellation is monotonic: once set it never clears, which is what makes
/// [`CancelState::cancelled`] safe to drop and re-create inside a `select!`.
#[derive(Debug)]
pub(crate) struct CancelState {
  cancelled: AtomicBool,
  finished: AtomicBool,
  notify: Notify,
}

impl CancelState {
  pub(crate) fn new() -> Self {
    CancelState {
      cancelled: AtomicBool::new(false),
      finished: AtomicBool::new(false),
      notify: Notify::new(),
    }
  }

  pub(crate) fn is_cancelled(&self) -> bool {
    self.cancelled.load(Ordering::Acquire)
  }

  pub(crate) fn cancel(&self) {
    self.cancelled.store(true, Ordering::Release);
    self.notify.notify_waiters();
  }

  /// Marks the task done so a deferred registry sweep can reclaim its slot.
  pub(crate) fn mark_finished(&self) {
    self.finished.store(true, Ordering::Release);
  }

  pub(crate) fn is_finished(&self) -> bool {
    self.finished.load(Ordering::Acquire)
  }

  /// Resolves once cancelled, immediately if that already happened.
  pub(crate) async fn cancelled(&self) {
    loop {
      if self.is_cancelled() {
        return;
      }

      let notified = self.notify.notified();
      tokio::pin!(notified);
      // Register before the second check: `notify_waiters` only wakes waiters that are
      // already registered, so checking first would drop a cancel landing in between
      // and the caller would wait forever.
      notified.as_mut().enable();

      if self.is_cancelled() {
        return;
      }

      notified.await;
    }
  }
}

/// A one-shot cancellation signal shared between pool components, used for pool-wide
/// shutdown. Per-task cancellation lives in [`crate::task::TaskCore`], which embeds
/// the same [`CancelState`].
#[derive(Clone)]
pub(crate) struct CancellationToken(Arc<CancelState>);

impl CancellationToken {
  pub(crate) fn new() -> Self {
    CancellationToken(Arc::new(CancelState::new()))
  }

  pub(crate) fn is_cancelled(&self) -> bool {
    self.0.is_cancelled()
  }

  pub(crate) fn cancel(&self) {
    self.0.cancel();
  }

  /// Resolves once cancelled, immediately if that already happened.
  pub(crate) async fn cancelled(&self) {
    self.0.cancelled().await;
  }
}

impl fmt::Debug for CancellationToken {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("CancellationToken")
      .field("cancelled", &self.is_cancelled())
      .finish()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  #[tokio::test]
  async fn cancelled_resolves_when_already_cancelled() {
    let token = CancellationToken::new();
    token.cancel();
    tokio::time::timeout(Duration::from_secs(1), token.cancelled())
      .await
      .expect("cancelled() hung on an already-cancelled token");
  }

  #[tokio::test]
  async fn cancelled_resolves_on_later_cancel() {
    let token = CancellationToken::new();
    let waiter = token.clone();
    let joined = tokio::spawn(async move { waiter.cancelled().await });

    tokio::task::yield_now().await;
    token.cancel();

    tokio::time::timeout(Duration::from_secs(1), joined)
      .await
      .expect("cancelled() hung after cancel")
      .unwrap();
  }

  #[tokio::test]
  async fn clones_share_state() {
    let token = CancellationToken::new();
    let clone = token.clone();
    assert!(!clone.is_cancelled());
    token.cancel();
    assert!(clone.is_cancelled());
  }

  #[tokio::test]
  async fn cancelled_is_safe_to_drop_and_retry() {
    let token = CancellationToken::new();

    for _ in 0..100 {
      tokio::select! {
        _ = token.cancelled() => panic!("resolved before cancel"),
        _ = tokio::task::yield_now() => {}
      }
    }

    token.cancel();
    tokio::time::timeout(Duration::from_secs(1), token.cancelled())
      .await
      .expect("cancelled() hung after repeated select! drops");
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn cancel_races_with_waiter_registration() {
    for _ in 0..20_000 {
      let token = CancellationToken::new();
      let waiter = token.clone();
      let joined = tokio::spawn(async move { waiter.cancelled().await });
      token.cancel();

      tokio::time::timeout(Duration::from_secs(5), joined)
        .await
        .expect("cancelled() missed a concurrent cancel")
        .unwrap();
    }
  }
}
