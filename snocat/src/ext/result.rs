// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
mod private {
  pub trait Sealed {}

  impl<S, E> Sealed for Result<S, E> {}
}

pub trait ResultExt: private::Sealed {
  type Ok;
  type Err;

  fn into_result(self) -> Result<Self::Ok, Self::Err>
  where
    Self: Sized;

  fn lift_future(self) -> futures::future::Ready<Result<Self::Ok, Self::Err>>
  where
    Self: Sized,
  {
    futures::future::ready(self.into_result())
  }

  // fn map_ok_async<F, Fut>(self, f: F) -> AndThen<ReadyFuture<Result<S, E>>, PureOk<S, E>, F>
  // where
  //   Self: Sized,
  //   F: FnOnce(S) -> Fut,
  //   Fut: Future,
  // {
  //   todo!()
  // }

  // fn map_err_async<F, Fut>(self, f: F) -> MapErr<ReadyFuture<Result<S, E>>, F>
  // where
  //   Self: Sized,
  //   F: FnOnce(E) -> Fut,
  //   Fut: Future,
  // {
  //   todo!()
  // }

  fn unwrap_infallible(self) -> Self::Ok
  where
    Self: Sized + ResultExt<Err = std::convert::Infallible>,
  {
    let as_result: Result<_, std::convert::Infallible> = self.into_result();
    match as_result {
      Ok(ok) => ok,
      #[allow(unreachable_patterns, dead_code)]
      // Explain to the compiler just how impossible this scenario is
      Err(e) => match e {},
    }
  }
}

impl<S, E> ResultExt for Result<S, E> {
  type Ok = S;
  type Err = E;

  fn into_result(self) -> Result<S, E>
  where
    Self: Sized,
  {
    self
  }
}

#[cfg(test)]
mod tests {
  use std::convert::Infallible;

  use futures::{
    future::{ready, AndThen},
    TryFutureExt,
  };

  use super::ResultExt;

  #[tokio::test]
  async fn lift_future() {
    let initial = Result::<u32, Infallible>::Ok(42);
    let futurized = initial.lift_future();
    let bound: AndThen<_, _, _> = futurized.and_then(|x| ready(Ok(x)));
    let res = bound.await;
    assert_eq!(res, Ok(42));
  }

  // #[tokio::test]
  // async fn map_err_async() {
  //   let initial = Result::<u32, Infallible>::Ok(42);
  //   assert_eq!(initial.map_err_async(|x| futures::future::ready(x)).await.unwrap(), 42);
  //   let error_case = Result::<(), u32>::Err(13);
  //   assert_eq!(error_case.map_err_async(|x| futures::future::ready(x)).await.unwrap_err(), 13);
  // }

  // #[tokio::test]
  // async fn map_ok_async() {
  //   let initial = Result::<u32, Infallible>::Ok(21);
  //   assert_eq!(initial.map_ok_async(|x| futures::future::ready(x * 2)).await, Ok(42))
  // }
}
