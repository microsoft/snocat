// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use std::{pin::Pin, task::Poll, time::Duration};

use futures::{
  future::{Either, FusedFuture},
  Future, FutureExt, TryFuture, TryFutureExt,
};

pub struct DelayedValue<V> {
  fulfilled: bool,
  sleep_future: Pin<Box<tokio::time::Sleep>>,
  value: Option<V>,
}

impl<V> Future for DelayedValue<V>
where
  V: Unpin,
{
  type Output = V;

  fn poll(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Self::Output> {
    if self.fulfilled {
      return Poll::Pending;
    }
    match futures::ready!(self.as_mut().sleep_future.poll_unpin(cx)) {
      () => {
        let value = std::mem::replace(&mut self.as_mut().value, None);
        match value {
          None => {
            // We were re-polled after finishing; Poll pending forever, like a Fused future
            Poll::Pending
          }
          Some(value) => {
            self.fulfilled = true;
            Poll::Ready(value)
          }
        }
      }
    }
  }
}

impl<V> FusedFuture for DelayedValue<V>
where
  V: Unpin,
{
  fn is_terminated(&self) -> bool {
    self.fulfilled
  }
}

pub struct Delayed<F: Future> {
  fulfilled: bool,
  duration: Duration,
  inner: Either<Pin<Box<F>>, DelayedValue<F::Output>>,
}

impl<F> Future for Delayed<F>
where
  F: Future,
  F::Output: Unpin,
{
  type Output = F::Output;

  fn poll(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Self::Output> {
    if self.fulfilled {
      return Poll::Pending;
    }
    let duration = self.duration;
    match &mut self.as_mut().inner {
      Either::Left(x) => {
        let res = futures::ready!(x.poll_unpin(cx));
        self.as_mut().inner = Either::Right(DelayedValue {
          fulfilled: false,
          value: Some(res),
          sleep_future: Box::pin(tokio::time::sleep(duration)),
        });
        Self::poll(self, cx) // Recurse once, to start the delay polling immediately
      }
      Either::Right(x) => {
        let res = futures::ready!(x.poll_unpin(cx));
        self.fulfilled = true;
        Poll::Ready(res)
      }
    }
  }
}

impl<F> FusedFuture for Delayed<F>
where
  F: Future,
  F::Output: Unpin,
{
  fn is_terminated(&self) -> bool {
    self.fulfilled
  }
}

pub struct TryDelayed<F: TryFuture + ?Sized> {
  fulfilled: bool,
  duration: Duration,
  inner: Either<Pin<Box<F>>, DelayedValue<F::Ok>>,
}

impl<F, Success, Error> Future for TryDelayed<F>
where
  F: TryFuture<Ok = Success, Error = Error> + Future<Output = Result<Success, Error>> + Unpin,
  Success: Unpin,
  Error: Unpin,
{
  type Output = Result<F::Ok, F::Error>;

  fn poll(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Self::Output> {
    if self.fulfilled {
      return Poll::Pending;
    }
    let duration = self.duration;
    match &mut self.as_mut().inner {
      Either::Left(x) => {
        let res = futures::ready!(x.try_poll_unpin(cx));
        match res {
          Err(e) => Poll::Ready(Err(e)),
          Ok(res) => {
            self.as_mut().inner = Either::Right(DelayedValue {
              fulfilled: false,
              value: Some(res),
              sleep_future: Box::pin(tokio::time::sleep(duration)),
            });
            Self::poll(self, cx) // Recurse once, to start the delay polling immediately
          }
        }
      }
      Either::Right(x) => {
        let res = futures::ready!(x.poll_unpin(cx));
        self.fulfilled = true;
        Poll::Ready(Ok(res))
      }
    }
  }
}

impl<F, Success, Error> FusedFuture for TryDelayed<F>
where
  F: TryFuture<Ok = Success, Error = Error> + Future<Output = Result<Success, Error>> + Unpin,
  Success: Unpin,
  Error: Unpin,
{
  fn is_terminated(&self) -> bool {
    self.fulfilled
  }
}

pub struct PollUntil<F, C> {
  task: Pin<Box<F>>,
  canceller: Option<Pin<Box<C>>>,
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

pub struct TryPollUntil<F, C> {
  inner: PollUntil<F, C>,
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
  inner: PollUntil<F, C>,
  make_timeout_error: Option<Box<FE>>,
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

pub trait FutureExtExt: FutureExt {
  fn delay(self, duration: Duration) -> Delayed<Self>
  where
    Self: Sized,
  {
    Delayed {
      fulfilled: false,
      duration,
      inner: Either::Left(Box::pin(self)),
    }
  }

  fn poll_until<C>(self, cancellation: C) -> PollUntil<Self, C>
  where
    C: Future,
    Self: Sized,
  {
    PollUntil {
      canceller: Some(Box::pin(cancellation)),
      task: Box::pin(self),
    }
  }
}

impl<Fut: ?Sized + FutureExt> FutureExtExt for Fut {}

pub trait TryFutureExtExt: TryFutureExt {
  fn try_delay(self, duration: Duration) -> TryDelayed<Self>
  where
    Pin<Box<Self>>: TryFutureExt,
    Self: Sized,
  {
    TryDelayed {
      fulfilled: false,
      duration,
      inner: Either::Left(Box::pin(self)),
    }
  }

  fn try_poll_until<Success, Error, C>(self, cancellation: C) -> TryPollUntil<Self, C>
  where
    Self: TryFuture<Ok = Success, Error = Error> + Sized,
    Success: Unpin,
    Error: Unpin,
    C: Future,
  {
    TryPollUntil {
      inner: self.poll_until(cancellation),
    }
  }

  fn try_poll_until_or_else<Success, Error, C, MakeAlternate>(
    self,
    cancellation: C,
    make_alternate: MakeAlternate,
  ) -> TryPollUntilOrElse<Self, C, MakeAlternate>
  where
    Self: TryFuture<Ok = Success, Error = Error> + Sized,
    MakeAlternate: (FnOnce() -> Result<Success, Error>),
    Success: Unpin,
    Error: Unpin,
    C: Future,
  {
    TryPollUntilOrElse {
      inner: self.poll_until(cancellation),
      make_timeout_error: Some(Box::new(make_alternate)),
    }
  }
}

impl<Fut: ?Sized + TryFutureExt> TryFutureExtExt for Fut {}

#[cfg(test)]
mod tests {
  use futures::future::{pending, ready, FutureExt};
  use std::time::Duration;

  use super::TryFutureExtExt;
  use crate::ext::future::FutureExtExt;

  #[tokio::test]
  async fn try_poll_until_or_else_failover() {
    async {
      ready(Ok(()))
        .try_poll_until_or_else(pending::<()>(), || Err(()))
        .now_or_never()
        .expect("must be instant")
        .expect("must be successful");
      pending::<Result<_, ()>>()
        .try_poll_until_or_else(ready(()), || Ok(()))
        .now_or_never()
        .expect("must be instant")
        .expect("must failover to a success");
      pending::<Result<(), _>>()
        .try_poll_until_or_else(ready(()), || Err(()))
        .now_or_never()
        .expect("must be instant")
        .expect_err("must failover to a failure");
    }
    .poll_until(tokio::time::sleep(Duration::from_secs(5)))
    .await
    .expect("Timed out");
  }
}
