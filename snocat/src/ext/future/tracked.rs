// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use std::{
  pin::Pin,
  task::{Context, Poll},
};

use futures::{
  future::{Fuse, FusedFuture},
  Future, FutureExt,
};

/// Allows asynchronous registration but only synchronous deregistration of
/// a wrapped [Future] to a tracking mechanism, such as a worker queue.
pub trait TaskTracker {
  type RegistrationFuture: Future<Output = Self>;
  fn register(self) -> Self::RegistrationFuture;
}

enum TrackingState {
  /// The task has never been polled, and register has not been invoked
  NotStarted,
  /// The task has not been polled, but registration is happening
  Registering,
  /// The task is being polled, and the tracker is registered.
  /// Dropping the task will synchronously deregister it.
  Running,
  /// The task has reached a fused state and future calls will return [Poll::Pending].
  /// The inner future has been destroyed to free resources, as has the tracker.
  /// The tracker is only destroyed *after* the inner future is destroyed.
  Completed,
}

pin_project_lite::pin_project! {
  pub struct Tracked<Fut, Tracker, RegistrationFuture> {
    #[pin]
    fut: Fuse<Fut>,
    tracker: Option<Tracker>,
    #[pin]
    registration: Option<Fuse<RegistrationFuture>>,
    state: TrackingState,
  }
}

impl<Fut, Tracker> Tracked<Fut, Tracker, Tracker::RegistrationFuture>
where
  Fut: Future,
  Tracker: TaskTracker,
{
  pub(super) fn new(fut: Fut, tracker: Tracker) -> Self {
    Self {
      state: TrackingState::NotStarted,
      registration: None,
      tracker: Some(tracker),
      fut: fut.fuse(),
    }
  }

  pub(super) fn new_fused(fut: Fut, tracker: Tracker) -> Self
  where
    Fut: FusedFuture,
  {
    Self {
      state: TrackingState::NotStarted,
      registration: None,
      tracker: Some(tracker),
      fut: fut.fuse(),
    }
  }
}

impl<Fut, Tracker> Future for Tracked<Fut, Tracker, <Tracker as TaskTracker>::RegistrationFuture>
where
  Fut: Future,
  Tracker: TaskTracker,
{
  type Output = Fut::Output;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    loop {
      let mut this = self.as_mut().project();
      use TrackingState as State;
      match this.state {
        State::NotStarted => {
          let registration = this.tracker.take().map(|t| t.register().fuse());
          this.registration.set(registration);
          *this.state = State::Registering;
          // re-poll
        }
        State::Registering => {
          let Some(registration) = this.registration.as_pin_mut() else { panic!("Registration state evaporated"); };
          let res = futures::ready!(Fuse::<Tracker::RegistrationFuture>::poll(registration, cx));
          *this.tracker = Some(res);
          *this.state = State::Running;
          // re-poll
        }
        State::Running => {
          let res = futures::ready!(Future::poll(this.fut, cx));
          *this.state = State::Completed;
          *this.tracker = None;
          return Poll::Ready(res);
        }
        State::Completed => return Poll::Pending,
      }
    }
  }
}

impl<Fut, Tracker> FusedFuture
  for Tracked<Fut, Tracker, <Tracker as TaskTracker>::RegistrationFuture>
where
  Fut: Future,
  Tracker: TaskTracker,
{
  fn is_terminated(&self) -> bool {
    matches!(&self.state, TrackingState::Completed) || self.fut.is_terminated()
  }
}
