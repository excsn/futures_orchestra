use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// The flat subset of `CancellationToken` that the pool actually uses: no parent/child
/// tree, so cloning is a refcount bump instead of a mutex acquisition.
struct LiteToken {
  cancelled: AtomicBool,
  notify: Notify,
}

#[derive(Clone)]
struct Lite(Arc<LiteToken>);

impl Lite {
  fn new() -> Self {
    Lite(Arc::new(LiteToken {
      cancelled: AtomicBool::new(false),
      notify: Notify::new(),
    }))
  }

  fn is_cancelled(&self) -> bool {
    self.0.cancelled.load(Ordering::Acquire)
  }

  fn cancel(&self) {
    self.0.cancelled.store(true, Ordering::Release);
    self.0.notify.notify_waiters();
  }
}

/// One task's worth of token traffic in the pool: create it, clone it into the queued
/// task, the worker's local copy, the active-task map, and the handle, then check it
/// twice and drop everything.
fn per_task_traffic_tokio(c: &mut Criterion) {
  c.bench_function("token/tokio_util_per_task", |b| {
    b.iter(|| {
      let token = CancellationToken::new();
      let in_task = token.clone();
      let worker_local = in_task.clone();
      let in_active_map = worker_local.clone();
      let in_handle = token.clone();
      black_box(in_task.is_cancelled());
      black_box(in_handle.is_cancelled());
      black_box((token, in_task, worker_local, in_active_map, in_handle));
    });
  });
}

fn per_task_traffic_lite(c: &mut Criterion) {
  c.bench_function("token/lite_per_task", |b| {
    b.iter(|| {
      let token = Lite::new();
      let in_task = token.clone();
      let worker_local = in_task.clone();
      let in_active_map = worker_local.clone();
      let in_handle = token.clone();
      black_box(in_task.is_cancelled());
      black_box(in_handle.is_cancelled());
      black_box((token, in_task, worker_local, in_active_map, in_handle));
    });
  });
}

fn new_only(c: &mut Criterion) {
  c.bench_function("token/tokio_util_new", |b| {
    b.iter(|| black_box(CancellationToken::new()));
  });
  c.bench_function("token/lite_new", |b| {
    b.iter(|| black_box(Lite::new()));
  });
}

fn clone_only(c: &mut Criterion) {
  let tokio_token = CancellationToken::new();
  let lite_token = Lite::new();

  c.bench_function("token/tokio_util_clone", |b| {
    b.iter(|| black_box(tokio_token.clone()));
  });
  c.bench_function("token/lite_clone", |b| {
    b.iter(|| black_box(lite_token.clone()));
  });
}

fn cancel_check(c: &mut Criterion) {
  let tokio_token = CancellationToken::new();
  let lite_token = Lite::new();

  c.bench_function("token/tokio_util_is_cancelled", |b| {
    b.iter(|| black_box(tokio_token.is_cancelled()));
  });
  c.bench_function("token/lite_is_cancelled", |b| {
    b.iter(|| black_box(lite_token.is_cancelled()));
  });
}

fn cancel_signal(c: &mut Criterion) {
  c.bench_function("token/tokio_util_new_clone_cancel", |b| {
    b.iter(|| {
      let token = CancellationToken::new();
      let observer = token.clone();
      token.cancel();
      black_box(observer.is_cancelled());
    });
  });
  c.bench_function("token/lite_new_clone_cancel", |b| {
    b.iter(|| {
      let token = Lite::new();
      let observer = token.clone();
      token.cancel();
      black_box(observer.is_cancelled());
    });
  });
}

criterion_group!(
  benches,
  per_task_traffic_tokio,
  per_task_traffic_lite,
  new_only,
  clone_only,
  cancel_check,
  cancel_signal
);
criterion_main!(benches);
