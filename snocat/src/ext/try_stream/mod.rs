// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

mod try_stream_ext_ext {
  use ::futures::stream::TryStreamExt;
  pub trait TryStreamExtExt: TryStreamExt + private::Sealed {}
  impl<S: ?Sized + TryStreamExt> TryStreamExtExt for S {}

  mod private {
    use super::TryStreamExt;
    pub trait Sealed {}

    impl<S: ?Sized + TryStreamExt> Sealed for S {}
  }
}

pub use try_stream_ext_ext::TryStreamExtExt;
