// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use std::{pin::Pin, time::Duration};

use futures::{future::Either, Future, FutureExt, TryFuture, TryFutureExt};

mod delayed;
pub use delayed::{Delayed, DelayedValue, TryDelayed};

mod poll_until;
pub use poll_until::{PollUntil, TryPollUntil, TryPollUntilOrElse};

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
