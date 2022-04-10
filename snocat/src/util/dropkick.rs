// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![deny(dead_code, unused_imports)]

use std::sync::{Arc, Mutex};

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
{
  /// Takes the content out of the mutex, preventing [DropkickSync::drop_kick] from being called on its content within an Arc
  pub fn counter_take_mutex(&self) -> Option<T> {
    self
      .inner
      .as_ref()
      .and_then(|inner| inner.lock().ok())
      .and_then(|mut inner| inner.take())
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

/// Mutexes drop their contents by locking
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

/// Arc-Mut-Opts drop their contents by locking, emptying the Option, and dropping it
impl<T> DropkickSync for std::sync::Arc<std::sync::Mutex<Option<T>>>
where
  T: DropkickSync,
{
  fn dropkick(self) {
    if let Some(inner) = self.lock().ok().map(|mut x| x.take()) {
      DropkickSync::dropkick(inner);
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
}
