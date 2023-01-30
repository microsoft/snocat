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
  pub(super) fulfilled: bool,
  pub(super) duration: Duration,
  pub(super) inner: Either<Pin<Box<F>>, DelayedValue<F::Output>>,
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
  pub(super) fulfilled: bool,
  pub(super) duration: Duration,
  pub(super) inner: Either<Pin<Box<F>>, DelayedValue<F::Ok>>,
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
