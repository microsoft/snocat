// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![deny(dead_code, unused_imports)]

use std::sync::{atomic::AtomicUsize, Arc, Mutex};

/// A trait describing the concept of "dropkicking", in an allusion to percusive maintenance.
///
/// Dropkicking an object tells it to do something specific to its type when dropped.
/// Generally, this is a way to notify some listener that the holder is being destroyed.
pub trait DropkickSync {
  fn dropkick(self);
}

/// A wrapper-type which [DropkickSync::dropkick]s its contents unless `counter`ed
#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Dropkick<T: DropkickSync> {
  inner: Option<T>,
}

impl<T> Dropkick<T>
where
  T: DropkickSync,
{
  /// Create a new [Dropkick] instance, which will kick
  /// the provided target when dropped, unless countered
  pub fn new(target: T) -> Self {
    Self {
      inner: Some(target),
    }
  }

  /// Allows creating a dropkick which calls a function as a notification
  ///
  /// Equivalent to [Dropkick::new], but produces tighter type constraints to aid closure inference.
  pub fn callback<R>(callback_fn: T) -> Self
  where
    T: FnOnce() -> R,
  {
    Self::new(callback_fn)
  }

  /// Consumes the drop-kick, preventing [DropkickSync::drop_kick] from being called on its content
  pub fn counter(mut self) {
    self.inner.take();
    drop(self);
  }

  /// Consumes the drop-kick, preventing [DropkickSync::drop_kick] from being called on its content
  ///
  /// Also returns the originally-provided value, allowing you to recover its ownership.
  pub fn counter_take(mut self) -> T {
    let value = self
      .inner
      .take()
      .expect("Dropkick dropped before countered?");
    drop(self);
    value
  }
}

impl<T> Dropkick<Arc<Mutex<Option<T>>>>
where
  T: DropkickSync,
  Arc<Mutex<Option<T>>>: DropkickSync,
{
  /// Takes the content out of the mutex, preventing [DropkickSync::drop_kick] from being called on its content within an Arc
  ///
  /// Blocking; Locks the [std::sync::Mutex] synchronously, and will deadlock if the lock is held elsewhere by the same thread.
  pub fn counter_take_mutex(&self) -> Option<T> {
    self
      .inner
      .as_ref()
      .and_then(|inner| match inner.lock().ok() {
        Some(mut lock) => {
          let value = lock.take();
          drop(lock);
          value
        }
        None => None,
      })
  }
}

impl<T> ::std::ops::Deref for Dropkick<T>
where
  T: DropkickSync,
{
  type Target = T;

  fn deref(&self) -> &Self::Target {
    self
      .inner
      .as_ref()
      .expect("Dropkick dropped before countered?")
  }
}

impl<T> ::std::ops::DerefMut for Dropkick<T>
where
  T: DropkickSync,
{
  fn deref_mut(&mut self) -> &mut Self::Target {
    self
      .inner
      .as_mut()
      .expect("Dropkick dropped before countered?")
  }
}

impl<T> Drop for Dropkick<T>
where
  T: DropkickSync,
{
  fn drop(&mut self) {
    if let Some(inner) = self.inner.take() {
      DropkickSync::dropkick(inner);
    }
  }
}

/// Allows for synchronous firing of a message to a broadcast channel when dropped
impl DropkickSync for ::tokio::sync::broadcast::Sender<()> {
  fn dropkick(self) {
    DropkickSync::dropkick(((), self));
  }
}

/// Allows for synchronous firing of a custom message to a broadcast channel when dropped
impl<T> DropkickSync for (T, ::tokio::sync::broadcast::Sender<T>) {
  fn dropkick(self) {
    let (value, sender) = self;
    let _ = sender.send(value);
  }
}

/// Allows for synchronous firing of a message to an unbounded mpsc channel when dropped
impl DropkickSync for ::tokio::sync::mpsc::UnboundedSender<()> {
  fn dropkick(self) {
    DropkickSync::dropkick(((), self));
  }
}

/// Allows for synchronous firing of a custom message to an unbounded mpsc channel when dropped
impl<T> DropkickSync for (T, ::tokio::sync::mpsc::UnboundedSender<T>) {
  fn dropkick(self) {
    let (value, sender) = self;
    let _ = sender.send(value);
  }
}

/// Allows a unit-oneshot to be inverted
/// Normally, dropping closes the channel, and sending fulfills it.
/// With this utility, dropping fulfills it, and `counter` drops (or returns) it.
impl DropkickSync for ::tokio::sync::oneshot::Sender<()> {
  fn dropkick(self) {
    DropkickSync::dropkick(((), self));
  }
}

/// Allows a unit-oneshot to be inverted, but with a custom send value
/// Normally, dropping closes the channel, and sending fulfills it.
/// With this utility, dropping fulfills it, and `counter` drops (or returns) it.
impl<T> DropkickSync for (T, ::tokio::sync::oneshot::Sender<T>) {
  fn dropkick(self) {
    let (value, sender) = self;
    let _ = sender.send(value);
  }
}

/// Allows for synchronous firing of a message to a watch channel when dropped
///
/// Note that this follows `watch`'s semantics, and may block if the watch is locked.
/// Thus, deadlocks may occur if the watch content is held by a reader in the same thread or task.
impl DropkickSync for ::tokio::sync::watch::Sender<()> {
  fn dropkick(self) {
    DropkickSync::dropkick(((), self));
  }
}

/// Allows for synchronous firing of a custom message to a watch channel when dropped
///
/// Note that this follows `watch`'s semantics, and may block if the watch is locked.
/// Thus, deadlocks may occur if the watch content is held by a reader in the same thread or task.
impl<T> DropkickSync for (T, ::tokio::sync::watch::Sender<T>) {
  fn dropkick(self) {
    let (value, sender) = self;
    let _ = sender.send(value);
  }
}

/// Changes the semantics of a [::tokio_util::sync::CancellationToken] to cancel on drop
impl DropkickSync for ::tokio_util::sync::CancellationToken {
  fn dropkick(self) {
    if !self.is_cancelled() {
      self.cancel()
    }
  }
}

/// Calls a [DropNotifier]'s callback when dropkicked.
impl<F, R> DropkickSync for F
where
  F: FnOnce() -> R,
{
  fn dropkick(self) {
    (self)();
  }
}

/// Options drop when set, and clear their contents
impl<T> DropkickSync for Option<T>
where
  T: DropkickSync,
{
  fn dropkick(mut self) {
    if let Some(inner) = self.take() {
      DropkickSync::dropkick(inner);
    }
  }
}

/// Mutexes drop their contents by unwrapping and clearing
impl<T> DropkickSync for std::sync::Mutex<T>
where
  T: DropkickSync,
{
  fn dropkick(self) {
    if let Ok(lock) = self.into_inner() {
      DropkickSync::dropkick(lock);
    }
  }
}

/// Records the number of active mutex background tasks/threads
static DROPKICK_MUTEX_OUTSTANDING_COUNT: AtomicUsize = AtomicUsize::new(0);

fn dropkick_mutex_background_task<T>(counter: &'static AtomicUsize, mutarc: Arc<Mutex<Option<T>>>)
where
  T: DropkickSync,
{
  tracing::trace_span!("Dropkick Mutex background cleanup").in_scope(|| {
    let active_drop_count = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    tracing::trace!(
      "Dropkick Mutex background cleanup thread started due to contention, active count: {}",
      active_drop_count,
    );
    let res = mutarc.lock().expect("Dropkick Mutex poisoned").take();
    <Option<T> as DropkickSync>::dropkick(res);
    let active_drop_count = counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) - 1;
    tracing::trace!(
      "Dropkick Mutex background cleanup thread completed, remaining active count: {}",
      active_drop_count,
    );
  });
}

/// Arc-Mut-Opts drop their contents by locking, emptying the Option, and executing the drop
///
/// There are three paths involved- each varying in optimism:
///
/// - The Arc is the only reference, so we can [function@Arc::try_unwrap] it,
///   then dispatch to the "owned mutex" implementation of [DropkickSync].
///
/// - The Mutex successfully locks immediately during the synchronous call, so we can use
///   [Option::take], then dispatch to the Option implementation of [DropkickSync].
///
/// - A blocking, background task is instantiated to lock the mutex asynchronously, eventually.
///   This path may deadlock if the mutex is held by another source indefinitely, but -
///   as it is on another thread - will not hang the dropping thread directly.
impl<T> DropkickSync for std::sync::Arc<std::sync::Mutex<Option<T>>>
where
  T: Send + DropkickSync + 'static,
{
  fn dropkick(self) {
    let this = match Arc::try_unwrap(self) {
      // Optimistic path- if we're the only reference, we can avoid creating locking overhead
      Ok(inner) => return <Mutex<Option<T>> as DropkickSync>::dropkick(inner),
      Err(this) => this,
    };
    // Attempt try_lock, then take the Option contents if immediately successful
    let taken = match this.try_lock() {
      Ok(mut lock) => {
        let taken: Option<T> = lock.take();
        // By exiting scope, we exit the lock context before executing the dropper,
        // as we want to minimize lock overlap time and have already taken the contents
        drop(lock);
        // Delegate using a new Dropkick to ensure all paths must drop it or opt-out
        Some(Dropkick::new(taken))
      }
      Err(std::sync::TryLockError::Poisoned(poisoned)) => {
        panic!("Dropkick Lock was poisoned: {:?}", poisoned)
      }
      Err(std::sync::TryLockError::WouldBlock) => None,
    };
    // By only operating on `None`, we allow our subsequent drop call to run on the content after the scope ends
    if taken.is_none() {
      tokio::task::spawn_blocking(move || {
        dropkick_mutex_background_task(&DROPKICK_MUTEX_OUTSTANDING_COUNT, this)
      });
    }
  }
}

impl<T> From<T> for Dropkick<T>
where
  T: DropkickSync,
{
  fn from(target: T) -> Self {
    Dropkick::new(target)
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{atomic::AtomicBool, Arc, Mutex};

  use super::{Dropkick, DropkickSync};

  #[repr(transparent)]
  struct DropkickFlag(pub bool);

  impl DropkickSync for &mut DropkickFlag {
    fn dropkick(self) {
      self.0 = true;
    }
  }

  #[test]
  fn dropkick_notifies() {
    let mut m = DropkickFlag(false);
    drop(Dropkick::new(&mut m));
    assert!(
      m.0,
      "Dropkick must call drop_kick when allowed to drop naturally"
    );
  }

  #[test]
  fn dropkick_consumable() {
    let mut m = DropkickFlag(false);
    Dropkick::new(&mut m).counter();
    assert!(!m.0, "Dropkick must not call drop_kick when consumed");
  }

  #[test]
  fn dropkick_callback_notifies() {
    let mut m = false;
    drop(Dropkick::callback(|| m = true));
    assert!(
      m,
      "Callback Dropkick must call drop_kick when allowed to drop naturally"
    );
  }

  #[test]
  fn dropkick_callback_consumable() {
    let mut m = false;
    Dropkick::callback(|| m = true).counter();
    assert!(
      !m,
      "Callback Dropkick must not call drop_kick when consumed"
    );
  }

  /// Verifies that an exclusively-held Arc-Mut-Opt calls no async-runtime functionality in Dropkick
  ///
  /// This ensures we take a fast-path in dropping, but cannot verify whether
  /// it is [Arc::try_unwrap] or [Mutex::try_lock] that is selected for use.
  #[test]
  fn dropkick_exclusive_arc_try_unwrap_optimization() {
    let m = Arc::new(AtomicBool::new(false));
    let target_arcmutopt: Arc<Mutex<_>> = Arc::new(Mutex::new(Some({
      let m = m.clone();
      move || m.store(true, std::sync::atomic::Ordering::Relaxed)
    })));
    let dropkick = Dropkick::new(target_arcmutopt);

    assert!(
      !m.load(std::sync::atomic::Ordering::Relaxed),
      "Dropkick must not execute until dropped"
    );
    drop(dropkick);
    assert!(
      m.load(std::sync::atomic::Ordering::Relaxed),
      "Dropkick must call drop_kick"
    );
  }

  /// Verifies that an externally-held but unlocked Arc-Mut-Opt calls no async-runtime functionality in Dropkick
  ///
  /// This ensures we take the try_lock fast-path in dropping.
  #[test]
  fn dropkick_exclusive_mutex_try_lock_optimization() {
    let m = Arc::new(AtomicBool::new(false));
    let target_arcmutopt: Arc<Mutex<_>> = Arc::new(Mutex::new(Some({
      let m = m.clone();
      move || m.store(true, std::sync::atomic::Ordering::Relaxed)
    })));
    let secondary_hold: Arc<Mutex<_>> = Arc::clone(&target_arcmutopt);
    let dropkick = Dropkick::new(target_arcmutopt);

    assert!(
      !m.load(std::sync::atomic::Ordering::Relaxed),
      "Dropkick must not execute until dropped"
    );
    drop(dropkick);
    assert!(
      m.load(std::sync::atomic::Ordering::Relaxed),
      "Dropkick must call drop_kick"
    );
    drop(secondary_hold);
  }

  #[tokio::test]
  async fn dropkick_mutex_background_task() {
    let m = Arc::new(AtomicBool::new(false));
    let target_arcmutopt = Arc::new(Mutex::new(Some({
      let m = m.clone();
      move || m.store(true, std::sync::atomic::Ordering::Relaxed)
    })));
    let secondary_hold: Arc<Mutex<_>> = Arc::clone(&target_arcmutopt);
    let dropkick = Dropkick::new(target_arcmutopt);
    let held_lock = secondary_hold.lock().unwrap();

    assert!(
      !m.load(std::sync::atomic::Ordering::Relaxed),
      "Dropkick must not execute until dropped"
    );
    drop(dropkick);
    assert!(
      !m.load(std::sync::atomic::Ordering::Relaxed),
      "Dropkick cannot have dropped while still locked"
    );
    drop(held_lock);
    for _ in 0..100 {
      if m.load(std::sync::atomic::Ordering::Relaxed) {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
      m.load(std::sync::atomic::Ordering::Relaxed),
      "Dropkick must call drop_kick on background thread after it is unlocked"
    );
  }

  /// Verifies that an Arc-Mut-Opt [Dropkick] can have its contents removed cleanly to conditionally prevent drop callbacks
  #[test]
  fn dropkick_counter_take_mutex() {
    let m = Arc::new(AtomicBool::new(false));
    let target_arcmutopt: Arc<Mutex<_>> = Arc::new(Mutex::new(Some({
      let m = m.clone();
      move || m.store(true, std::sync::atomic::Ordering::Relaxed)
    })));
    let dropkick = Dropkick::new(target_arcmutopt);

    drop(dropkick.counter_take_mutex());
    assert!(
      !m.load(std::sync::atomic::Ordering::Relaxed),
      "Dropkick::counter_take_mutex must not invoke dropkick event"
    );
    drop(dropkick);
    assert!(
      !m.load(std::sync::atomic::Ordering::Relaxed),
      "Dropkick must not invoke counter_take_mutex-removed target"
    );
  }
}
