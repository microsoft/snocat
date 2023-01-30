// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use std::{pin::Pin, task::Poll};

use futures::{future::FusedFuture, Future, FutureExt, TryFuture};

pub struct PollUntil<F, C> {
  pub(super) task: Pin<Box<F>>,
  pub(super) canceller: Option<Pin<Box<C>>>,
}

impl<F, C, O> Future for PollUntil<F, C>
where
  F: Future<Output = O>,
  O: Unpin,
  C: Future,
{
  type Output = Option<O>;

  fn poll(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Self::Output> {
    let Self {
      canceller, task, ..
    } = std::ops::DerefMut::deref_mut(&mut self);
    match canceller {
      None => {
        // This allows us to act as a FusedFuture without an additional field
        Poll::Pending
      }
      Some(canceller) => match FutureExt::poll_unpin(canceller, cx) {
        Poll::Ready(..) => {
          self.canceller = None;
          Poll::Ready(None)
        }
        Poll::Pending => {
          let res = futures::ready!(task.poll_unpin(cx));
          self.canceller = None;
          Poll::Ready(Some(res))
        }
      },
    }
  }
}

impl<F, C, O> FusedFuture for PollUntil<F, C>
where
  F: Future<Output = O>,
  O: Unpin,
  C: Future,
{
  fn is_terminated(&self) -> bool {
    self.canceller.is_none()
  }
}

#[repr(transparent)]
pub struct TryPollUntil<F, C> {
  pub(super) inner: PollUntil<F, C>,
}

impl<F, C, T, E> Future for TryPollUntil<F, C>
where
  F: TryFuture<Output = Result<T, E>>,
  T: Unpin,
  E: Unpin,
  C: Future,
{
  type Output = Result<Option<T>, E>;

  fn poll(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Self::Output> {
    let res = futures::ready!(self.as_mut().inner.poll_unpin(cx));
    Poll::Ready(match res {
      Some(Err(e)) => Err(e),
      Some(Ok(r)) => Ok(Some(r)),
      None => Ok(None),
    })
  }
}

impl<F, C, T, E> FusedFuture for TryPollUntil<F, C>
where
  F: TryFuture<Output = Result<T, E>>,
  T: Unpin,
  E: Unpin,
  C: Future,
{
  fn is_terminated(&self) -> bool {
    self.inner.is_terminated()
  }
}

pub struct TryPollUntilOrElse<F, C, FE> {
  pub(super) inner: PollUntil<F, C>,
  pub(super) make_timeout_error: Option<Box<FE>>,
}

impl<F, C, T, E, MakeAlternative> Future for TryPollUntilOrElse<F, C, MakeAlternative>
where
  F: TryFuture<Output = Result<T, E>>,
  MakeAlternative: FnOnce() -> Result<T, E>,
  T: Unpin,
  E: Unpin,
  C: Future,
{
  type Output = Result<T, E>;

  fn poll(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Self::Output> {
    let Self {
      make_timeout_error,
      inner,
      ..
    } = std::ops::DerefMut::deref_mut(&mut self);
    // Checks that error-maker is present; we `expect` it later
    // It'd be nice for Rust to allow some sort of `maybe_take` mechanism
    // so we could avoid the `expect` and encode it into the type system.
    if make_timeout_error.is_none() {
      // This allows us to act as a FusedFuture without an additional field
      return Poll::Pending;
    }

    let res = futures::ready!(inner.poll_unpin(cx));
    Poll::Ready(match res {
      Some(Ok(r)) => Ok(r),
      Some(Err(e)) => Err(e),
      None => {
        let cb = make_timeout_error
          .take()
          .expect("Error-maker callback missing");
        cb()
      }
    })
  }
}

impl<F, C, T, E, MakeAlternative> FusedFuture for TryPollUntilOrElse<F, C, MakeAlternative>
where
  F: TryFuture<Output = Result<T, E>>,
  MakeAlternative: (FnOnce() -> Result<T, E>) + Unpin,
  T: Unpin,
  E: Unpin,
  C: Future,
{
  fn is_terminated(&self) -> bool {
    self.make_timeout_error.is_none() || self.inner.is_terminated()
  }
}
