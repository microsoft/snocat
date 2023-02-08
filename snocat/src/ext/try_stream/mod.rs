// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

mod try_fuse_stream;
pub use try_fuse_stream::TryFuseStream;

mod try_stream_ext_ext {
  use ::futures::stream::TryStreamExt;
  pub trait TryStreamExtExt: TryStreamExt + private::Sealed {
    /// Implements FusedFuture atop any TryStream and ensures that polls
    /// occurring after completion or any error return `Poll::Ready(None)`.
    fn try_fuse(self) -> super::TryFuseStream<Self>
    where
      Self: Sized,
    {
      super::TryFuseStream::new(self)
    }
  }
  impl<S: ?Sized + TryStreamExt> TryStreamExtExt for S {}

  mod private {
    use super::TryStreamExt;
    pub trait Sealed {}

    impl<S: ?Sized + TryStreamExt> Sealed for S {}
  }
}

pub use try_stream_ext_ext::TryStreamExtExt;
