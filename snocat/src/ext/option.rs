// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
mod private {
  pub trait Sealed {}

  impl<T> Sealed for Option<T> {}
}

pub trait OptionExt: private::Sealed {
  type Inner;

  fn into_option(self) -> Option<Self::Inner>
  where
    Self: Sized;

  fn lift_future(self) -> futures::future::Ready<Option<Self::Inner>>
  where
    Self: Sized,
  {
    futures::future::ready(self.into_option())
  }
}

impl<T> OptionExt for Option<T> {
  type Inner = T;

  fn into_option(self) -> Option<T>
  where
    Self: Sized,
  {
    self
  }
}

#[cfg(test)]
mod tests {

  use super::OptionExt;

  #[tokio::test]
  async fn lift_future() {
    let initial = Some(42u32);
    let res = initial.lift_future().await;
    assert_eq!(res, Some(42));
  }
}
