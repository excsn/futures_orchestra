use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::task::{TaskCore, TaskLabel};

/// The tasks currently executing, held in a dense slot array rather than a map.
///
/// Membership is bounded by the pool's concurrency limit, and each task is handed the
/// index of its own slot, so a map's keyspace machinery buys nothing here. Insert and
/// remove are an index write and a free-list push.
///
/// A task's slot is released after its concurrency permit, so occupancy can briefly
/// exceed the concurrency limit and the array grows to that high-water mark.
pub(crate) struct ActiveSlots {
  slots: Vec<Option<Arc<TaskCore>>>,
  free: Vec<usize>,
}

impl ActiveSlots {
  pub(crate) fn with_capacity(capacity: usize) -> Self {
    ActiveSlots {
      slots: Vec::with_capacity(capacity),
      free: Vec::with_capacity(capacity),
    }
  }

  pub(crate) fn insert(&mut self, core: Arc<TaskCore>) -> usize {
    match self.free.pop() {
      Some(slot) => {
        self.slots[slot] = Some(core);
        slot
      }
      None => {
        self.slots.push(Some(core));
        self.slots.len() - 1
      }
    }
  }

  pub(crate) fn remove(&mut self, slot: usize) {
    if self.slots.get(slot).is_some_and(Option::is_some) {
      self.slots[slot] = None;
      self.free.push(slot);
    }
  }

  pub(crate) fn len(&self) -> usize {
    self.slots.len() - self.free.len()
  }

  pub(crate) fn all_tokens(&self) -> Vec<Arc<TaskCore>> {
    self.slots.iter().flatten().cloned().collect()
  }

  pub(crate) fn tokens_matching(&self, labels: &HashSet<TaskLabel>) -> Vec<Arc<TaskCore>> {
    self
      .slots
      .iter()
      .flatten()
      .filter(|core| !core.labels().is_disjoint(labels))
      .cloned()
      .collect()
  }

  fn sweep_finished(&mut self) {
    for (index, slot) in self.slots.iter_mut().enumerate() {
      if slot.as_ref().is_some_and(|core| core.is_finished()) {
        *slot = None;
        self.free.push(index);
      }
    }
  }
}

/// Registry with removal deferred to the next inserter, for pools where no single
/// party owns removal.
///
/// A finishing task never touches the slot lock: it marks its core finished and
/// decrements `in_flight`, and whoever inserts next reclaims finished slots in bulk
/// when the free list runs dry. `in_flight` is what shutdown waits on; the slot array
/// can lag it by holding finished entries until the next sweep, so every reader of
/// the entries must skip finished ones.
///
/// A side effect of lazy reclamation is that finished entries (their `TaskCore`
/// `Arc`s) stay alive in an idle pool until the next insert or the registry is
/// dropped, bounded by the slot array's high-water mark.
pub(crate) struct DeferredRegistry {
  in_flight: AtomicUsize,
  slots: RwLock<ActiveSlots>,
}

impl DeferredRegistry {
  pub(crate) fn with_capacity(capacity: usize) -> Self {
    DeferredRegistry {
      in_flight: AtomicUsize::new(0),
      slots: RwLock::new(ActiveSlots::with_capacity(capacity)),
    }
  }

  pub(crate) fn insert(&self, core: Arc<TaskCore>) {
    self.in_flight.fetch_add(1, Ordering::Relaxed);

    let mut slots = self.slots.write();
    if slots.free.is_empty() {
      slots.sweep_finished();
    }
    slots.insert(core);
  }

  /// The finishing task's half of removal. Call after `TaskCore::mark_finished`.
  pub(crate) fn finish(&self) {
    self.in_flight.fetch_sub(1, Ordering::Release);
  }

  pub(crate) fn len(&self) -> usize {
    self.in_flight.load(Ordering::Acquire)
  }

  pub(crate) fn all_tokens(&self) -> Vec<Arc<TaskCore>> {
    let slots = self.slots.read();
    slots
      .slots
      .iter()
      .flatten()
      .filter(|core| !core.is_finished())
      .cloned()
      .collect()
  }

  pub(crate) fn tokens_matching(&self, labels: &HashSet<TaskLabel>) -> Vec<Arc<TaskCore>> {
    let slots = self.slots.read();
    slots
      .slots
      .iter()
      .flatten()
      .filter(|core| !core.is_finished() && !core.labels().is_disjoint(labels))
      .cloned()
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn core(id: u64, names: &[&str]) -> Arc<TaskCore> {
    Arc::new(TaskCore::new(id, names.iter().map(|s| (*s).to_string()).collect()))
  }

  #[test]
  fn insert_and_remove_tracks_len() {
    let mut slots = ActiveSlots::with_capacity(4);
    assert_eq!(slots.len(), 0);

    let a = slots.insert(core(1, &[]));
    let b = slots.insert(core(2, &[]));
    assert_eq!(slots.len(), 2);

    slots.remove(a);
    assert_eq!(slots.len(), 1);
    slots.remove(b);
    assert_eq!(slots.len(), 0);
  }

  #[test]
  fn freed_slots_are_reused() {
    let mut slots = ActiveSlots::with_capacity(4);
    let a = slots.insert(core(1, &[]));
    slots.remove(a);
    let b = slots.insert(core(2, &[]));

    assert_eq!(a, b);
    assert_eq!(slots.slots.len(), 1);
  }

  #[test]
  fn removing_an_empty_slot_is_inert() {
    let mut slots = ActiveSlots::with_capacity(4);
    let a = slots.insert(core(1, &[]));
    slots.remove(a);
    slots.remove(a);
    slots.remove(99);

    assert_eq!(slots.len(), 0);
    assert_eq!(slots.insert(core(2, &[])), a);
  }

  #[test]
  fn matches_only_overlapping_labels() {
    let mut slots = ActiveSlots::with_capacity(4);
    slots.insert(core(1, &["red"]));
    slots.insert(core(2, &["blue"]));
    slots.insert(core(3, &["red", "green"]));

    let wanted: HashSet<TaskLabel> = ["red".to_string()].into_iter().collect();
    let mut matched: Vec<u64> = slots
      .tokens_matching(&wanted)
      .into_iter()
      .map(|c| c.task_id())
      .collect();
    matched.sort_unstable();

    assert_eq!(matched, vec![1, 3]);
  }

  #[test]
  fn removed_entries_stop_matching() {
    let mut slots = ActiveSlots::with_capacity(4);
    let a = slots.insert(core(1, &["red"]));
    slots.insert(core(2, &["red"]));
    slots.remove(a);

    let wanted: HashSet<TaskLabel> = ["red".to_string()].into_iter().collect();
    assert_eq!(slots.tokens_matching(&wanted).len(), 1);
    assert_eq!(slots.all_tokens().len(), 1);
  }

  #[test]
  fn deferred_insert_and_finish_track_len() {
    let registry = DeferredRegistry::with_capacity(4);
    assert_eq!(registry.len(), 0);

    let a = core(1, &[]);
    let b = core(2, &[]);
    registry.insert(a.clone());
    registry.insert(b.clone());
    assert_eq!(registry.len(), 2);

    a.mark_finished();
    registry.finish();
    assert_eq!(registry.len(), 1);
    b.mark_finished();
    registry.finish();
    assert_eq!(registry.len(), 0);
  }

  #[test]
  fn deferred_finished_entries_are_invisible_before_the_sweep() {
    let registry = DeferredRegistry::with_capacity(4);
    let a = core(1, &["red"]);
    registry.insert(a.clone());
    registry.insert(core(2, &["red"]));
    a.mark_finished();
    registry.finish();

    let wanted: HashSet<TaskLabel> = ["red".to_string()].into_iter().collect();
    assert_eq!(registry.tokens_matching(&wanted).len(), 1);
    assert_eq!(registry.all_tokens().len(), 1);
  }

  #[test]
  fn deferred_sweep_reclaims_finished_slots() {
    let registry = DeferredRegistry::with_capacity(2);

    for round in 0..20u64 {
      let batch: Vec<_> = (0..4).map(|i| core(round * 4 + i, &[])).collect();
      for c in &batch {
        registry.insert(c.clone());
      }
      for c in &batch {
        c.mark_finished();
        registry.finish();
      }
    }

    assert_eq!(registry.len(), 0);
    assert!(
      registry.slots.read().slots.len() <= 8,
      "sweep failed to reclaim slots: array grew to {}",
      registry.slots.read().slots.len()
    );
  }

  #[test]
  fn deferred_grows_when_nothing_has_finished() {
    let registry = DeferredRegistry::with_capacity(1);
    for id in 0..8 {
      registry.insert(core(id, &[]));
    }
    assert_eq!(registry.len(), 8);
    assert_eq!(registry.all_tokens().len(), 8);
  }
}
