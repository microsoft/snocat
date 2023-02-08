// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use std::task::Poll;

use futures::{stream::FusedStream, Stream, TryStream};

::pin_project_lite::pin_project! {
  #[project = TryFuseStreamProjection]
  #[project_replace = TryFuseStreamProjectionReplacement]
  #[derive(Debug, Clone)]
  pub enum TryFuseStream<S> {
    Active {
      #[pin]
      source: S,
    },
    Ended {
      failed: bool
    },
  }
}

impl<S> Stream for TryFuseStream<S>
where
  S: TryStream,
{
  type Item = Result<<S as TryStream>::Ok, <S as TryStream>::Error>;

  fn poll_next(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> Poll<Option<Self::Item>> {
    use self::TryFuseStreamProjection as Projection;
    match self.as_mut().project() {
      Projection::Active { source } => {
        match source.try_poll_next(cx) {
          // Stream ended, terminate and dispose of our source and ID generator
          Poll::Ready(None) => {
            // Clear our state through projected replacement
            // See https://docs.rs/pin-project/latest/pin_project/attr.pin_project.html#project_replace-method
            self.project_replace(Self::Ended { failed: false });
            Poll::Ready(None)
          }
          // Error produced by TryStream, end the stream and dispose of its source and generator
          Poll::Ready(Some(Err(item))) => {
            // Clear our state through projected replacement
            // See https://docs.rs/pin-project/latest/pin_project/attr.pin_project.html#project_replace-method
            self.project_replace(Self::Ended { failed: true });
            Poll::Ready(Some(Err(item)))
          }
          // New item received, forward it out and remain in receive state
          Poll::Ready(Some(Ok(item))) => Poll::Ready(Some(Ok(item))),
          Poll::Pending => Poll::Pending,
        }
      }
      Projection::Ended { .. } => return Poll::Ready(None),
    }
  }
}

impl<S> FusedStream for TryFuseStream<S>
where
  Self: Stream,
{
  fn is_terminated(&self) -> bool {
    match self {
      Self::Active { .. } => false,
      Self::Ended { .. } => true,
    }
  }
}

impl<S> TryFuseStream<S> {
  pub(super) fn new(source: S) -> Self {
    TryFuseStream::Active { source }
  }
}

#[cfg(test)]
mod tests {
  use super::super::TryStreamExtExt;
  use super::TryFuseStream;
  use futures::stream::{self, FusedStream, StreamExt, TryStreamExt};

  #[tokio::test]
  async fn try_fused_stream_terminate_after_error() {
    let mut fused: TryFuseStream<_> =
      stream::iter([Ok(0), Ok(1), Err(()), Ok(2), Ok(3)]).try_fuse();
    // Collect the items prior to our induced error
    let res: Vec<_> = (&mut fused)
      .take(2)
      .try_collect()
      .await
      .expect("Items preceding error must return as expected");
    assert_eq!(
      res,
      vec![0, 1],
      "Elements prior to error must match expectations"
    );
    let Err(()) = (&mut fused).try_next().await else {
    panic!("Third item in test-set must be the anticipated error");
  };
    assert!(
      FusedStream::is_terminated(&fused),
      "Fused try-stream must be terminated after error"
    );
    let next_after_error = (&mut fused)
      .try_next()
      .await
      .expect("Entries after failure should return successful termination state");
    assert_eq!(
      next_after_error, None,
      "Third item in test-set must be the anticipated error"
    );
    std::assert_matches::assert_matches!(fused, TryFuseStream::Ended { failed: true });
  }

  #[tokio::test]
  async fn try_fused_stream_terminate_after_end() {
    let mut fused: TryFuseStream<_> = stream::iter([
      Result::<usize, std::convert::Infallible>::Ok(0),
      Ok(1),
      Ok(2),
    ])
    .try_fuse();
    let res: Vec<_> = (&mut fused)
      .try_collect()
      .await
      .expect("Error returned from error-free try-stream");
    assert_eq!(
      res,
      vec![0, 1, 2],
      "Elements must be returned to exhaustive end-point"
    );
    assert!(
      FusedStream::is_terminated(&fused),
      "Fused try-stream must be terminated after source stream is terminated"
    );
    std::assert_matches::assert_matches!(fused, TryFuseStream::Ended { failed: false });
  }
}
