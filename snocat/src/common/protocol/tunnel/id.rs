use std::ops::Deref;

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct TunnelId(u64);

impl TunnelId {
  pub fn new(inner: u64) -> TunnelId {
    Self(inner)
  }

  pub fn inner(&self) -> u64 {
    self.0
  }
}

impl From<u64> for TunnelId {
  fn from(inner: u64) -> Self {
    Self::new(inner)
  }
}

impl From<TunnelId> for u64 {
  fn from(tunnel_id: TunnelId) -> u64 {
    tunnel_id.inner()
  }
}

pub trait TunnelIdGenerator {
  fn next(&self) -> TunnelId;
}

#[deprecated(note = "Use TunnelIdGenerator for adjusted casing")]
pub use self::TunnelIdGenerator as TunnelIDGenerator;

mod tunnel_id_generator_ext {
  use std::task::Poll;

  use futures::{stream::FusedStream, Stream, TryStream};

  use super::TunnelIdGenerator;
  use crate::common::protocol::tunnel::IntoTunnel;

  pin_project_lite::pin_project! {
    #[project = ConstructedTunnelStreamProjection]
    #[project_replace = ConstructedTunnelStreamProjectionReplacement]
    #[derive(Debug, Clone)]
    pub enum ConstructedTunnelStream<S, G> {
      Active {
        #[pin]
        source: S,
        generator: G,
      },
      Ended,
    }
  }

  impl<S, G> Stream for ConstructedTunnelStream<S, G>
  where
    S: Stream,
    <S as Stream>::Item: IntoTunnel,
    G: TunnelIdGenerator,
  {
    type Item = <<S as Stream>::Item as IntoTunnel>::Tunnel;

    fn poll_next(
      mut self: std::pin::Pin<&mut Self>,
      cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
      match self.as_mut().project() {
        ConstructedTunnelStreamProjection::Active { source, generator } => {
          match source.poll_next(cx) {
            // Stream ended, terminate and dispose of our source and ID generator
            Poll::Ready(None) => {
              // Clear our state through projected replacement
              // See https://docs.rs/pin-project/latest/pin_project/attr.pin_project.html#project_replace-method
              self.project_replace(ConstructedTunnelStream::Ended);
              Poll::Ready(None)
            }
            // New item received, construct it into a tunnel and yield the result
            Poll::Ready(Some(item)) => {
              let id = <G as TunnelIdGenerator>::next(generator);
              let result = <<S as Stream>::Item as IntoTunnel>::into_tunnel(item, id);
              Poll::Ready(Some(result))
            }
            Poll::Pending => Poll::Pending,
          }
        }
        ConstructedTunnelStreamProjection::Ended => return Poll::Ready(None),
      }
    }
  }

  impl<S, G> FusedStream for ConstructedTunnelStream<S, G>
  where
    Self: Stream,
  {
    fn is_terminated(&self) -> bool {
      match self {
        Self::Active { .. } => false,
        Self::Ended => true,
      }
    }
  }

  pin_project_lite::pin_project! {
    #[project = ConstructedTunnelTryStreamProjection]
    #[project_replace = ConstructedTunnelTryStreamProjectionReplacement]
    #[derive(Debug, Clone)]
    pub enum ConstructedTunnelTryStream<S, G> {
      Active {
        #[pin]
        source: S,
        generator: G,
      },
      Ended {
        failed: bool,
      }
    }
  }

  impl<S, G> Stream for ConstructedTunnelTryStream<S, G>
  where
    S: TryStream,
    <S as TryStream>::Ok: IntoTunnel,
    G: TunnelIdGenerator,
  {
    type Item = Result<<<S as TryStream>::Ok as IntoTunnel>::Tunnel, <S as TryStream>::Error>;

    fn poll_next(
      mut self: std::pin::Pin<&mut Self>,
      cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
      match self.as_mut().project() {
        ConstructedTunnelTryStreamProjection::Active { source, generator } => {
          match source.try_poll_next(cx) {
            // Stream ended, terminate and dispose of our source and ID generator
            Poll::Ready(None) => {
              // Clear our state through projected replacement
              // See https://docs.rs/pin-project/latest/pin_project/attr.pin_project.html#project_replace-method
              self.project_replace(ConstructedTunnelTryStream::Ended { failed: false });
              Poll::Ready(None)
            }
            // Error produced by TryStream, end the stream and dispose of its source and generator
            Poll::Ready(Some(Err(item))) => {
              // Clear our state through projected replacement
              // See https://docs.rs/pin-project/latest/pin_project/attr.pin_project.html#project_replace-method
              self.project_replace(ConstructedTunnelTryStream::Ended { failed: true });
              Poll::Ready(Some(Err(item)))
            }
            // New item received, construct it into a tunnel and yield the result
            Poll::Ready(Some(Ok(item))) => {
              let id = <G as TunnelIdGenerator>::next(generator);
              let result = <<S as TryStream>::Ok as IntoTunnel>::into_tunnel(item, id);
              Poll::Ready(Some(Ok(result)))
            }
            Poll::Pending => Poll::Pending,
          }
        }
        ConstructedTunnelTryStreamProjection::Ended { .. } => return Poll::Ready(None),
      }
    }
  }

  impl<S, G> FusedStream for ConstructedTunnelTryStream<S, G>
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

  pub trait TunnelIdGeneratorExt: TunnelIdGenerator + private::Sealed {
    fn construct_tunnels<TunnelSource>(
      self,
      tunnel_source: TunnelSource,
    ) -> ConstructedTunnelStream<TunnelSource, Self>
    where
      TunnelSource: Stream,
      <TunnelSource as Stream>::Item: IntoTunnel,
      Self: Sized,
    {
      ConstructedTunnelStream::Active {
        source: tunnel_source,
        generator: self,
      }
    }

    fn try_construct_tunnels<TunnelSource>(
      self,
      tunnel_source: TunnelSource,
    ) -> ConstructedTunnelTryStream<TunnelSource, Self>
    where
      TunnelSource: TryStream,
      <TunnelSource as TryStream>::Ok: IntoTunnel,
      Self: Sized,
    {
      ConstructedTunnelTryStream::Active {
        source: tunnel_source,
        generator: self,
      }
    }
  }

  impl<G: ?Sized + TunnelIdGenerator> TunnelIdGeneratorExt for G {}

  mod private {
    use super::TunnelIdGenerator;
    pub trait Sealed {}

    impl<G: ?Sized + TunnelIdGenerator> Sealed for G {}
  }

  #[cfg(test)]
  mod tests {
    use std::assert_matches::assert_matches;

    use futures::{
      stream::{self, FusedStream},
      StreamExt, TryStreamExt,
    };

    use crate::common::protocol::tunnel::{
      id::MonotonicAtomicGenerator, IntoTunnel, TunnelId, WithTunnelId,
    };

    use super::{ConstructedTunnelStream, ConstructedTunnelTryStream, TunnelIdGeneratorExt};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FakeTunnelParams;

    #[derive(PartialEq, Eq)]
    struct FakeTunnel {
      tunnel_id: TunnelId,
    }

    impl std::fmt::Debug for FakeTunnel {
      fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FakeTunnel")
          .field(&self.tunnel_id.inner())
          .finish()
      }
    }

    impl WithTunnelId for FakeTunnel {
      fn id(&self) -> &TunnelId {
        &self.tunnel_id
      }
    }

    impl IntoTunnel for FakeTunnelParams {
      type Tunnel = FakeTunnel;

      fn into_tunnel(self, tunnel_id: TunnelId) -> Self::Tunnel {
        FakeTunnel { tunnel_id }
      }
    }

    #[tokio::test]
    async fn fused_tunnel_id_stream() {
      let s = stream::empty::<FakeTunnelParams>();
      let g = MonotonicAtomicGenerator::new(0);
      let mut outputs = g.construct_tunnels(s);
      let res: Vec<_> = (&mut outputs).collect().await;
      assert!(
        res.is_empty(),
        "No items may be present in the result in this test"
      );
      assert!(
        FusedStream::is_terminated(&outputs),
        "Construction stream must be terminated after exhaustion"
      );
      assert_matches!(outputs, ConstructedTunnelStream::Ended);
    }

    #[tokio::test]
    async fn fused_tunnel_id_try_stream() {
      let s = stream::empty::<Result<FakeTunnelParams, ()>>();
      let g = MonotonicAtomicGenerator::new(0);
      let mut outputs = g.try_construct_tunnels(s);
      let res: Vec<_> = (&mut outputs)
        .try_collect()
        .await
        .expect("Must not have produced a failure for an empty input set");
      assert!(
        res.is_empty(),
        "No items may be present in the result in this test"
      );
      assert!(
        FusedStream::is_terminated(&outputs),
        "Construction try-stream must be terminated after exhaustion"
      );
      assert_matches!(outputs, ConstructedTunnelTryStream::Ended { .. });
    }

    #[tokio::test]
    async fn tunnel_id_stream_incrementing() {
      const SAMPLE_COUNT: usize = 3;
      let s = stream::repeat(FakeTunnelParams).take(SAMPLE_COUNT);
      let g = MonotonicAtomicGenerator::new(0);
      let mut outputs = g.construct_tunnels(s);
      let res: Vec<_> = (&mut outputs).collect().await;
      assert_eq!(
        res,
        (0..SAMPLE_COUNT)
          .into_iter()
          .map(|x| FakeTunnel {
            tunnel_id: (x as u64).into()
          })
          .collect::<Vec<_>>(),
        "Test results must match the expected output count and values"
      );
      assert!(
        FusedStream::is_terminated(&outputs),
        "Construction stream must be terminated after exhaustion"
      );
    }

    #[tokio::test]
    async fn tunnel_id_try_stream_incrementing() {
      const SAMPLE_COUNT: usize = 3;
      let s = stream::repeat(FakeTunnelParams)
        .take(SAMPLE_COUNT)
        .map(Result::<_, ()>::Ok);
      let g = MonotonicAtomicGenerator::new(0);
      let mut outputs = g.try_construct_tunnels(s);
      let res: Vec<_> = (&mut outputs)
        .try_collect()
        .await
        .expect("Must not have produced an error");
      assert_eq!(
        res,
        (0..SAMPLE_COUNT)
          .into_iter()
          .map(|x| FakeTunnel {
            tunnel_id: (x as u64).into()
          })
          .collect::<Vec<_>>(),
        "Test results must match the expected output count and values"
      );
      assert!(
        FusedStream::is_terminated(&outputs),
        "Construction try-stream must be terminated after exhaustion"
      );
    }
  }
}

pub use tunnel_id_generator_ext::{
  ConstructedTunnelStream, ConstructedTunnelTryStream, TunnelIdGeneratorExt,
};

pub struct MonotonicAtomicGenerator {
  next: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for MonotonicAtomicGenerator {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct(std::any::type_name::<MonotonicAtomicGenerator>())
      .finish_non_exhaustive()
  }
}

impl MonotonicAtomicGenerator {
  pub fn new(next: u64) -> Self {
    Self {
      next: std::sync::atomic::AtomicU64::new(next),
    }
  }

  pub fn next(&self) -> TunnelId {
    TunnelId::new(self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
  }
}

impl TunnelIdGenerator for MonotonicAtomicGenerator {
  fn next(&self) -> TunnelId {
    MonotonicAtomicGenerator::next(&self)
  }
}

impl<Wrapper> TunnelIdGenerator for Wrapper
where
  Wrapper: Deref,
  <Wrapper as Deref>::Target: TunnelIdGenerator,
{
  fn next(&self) -> TunnelId {
    <<Wrapper as Deref>::Target as TunnelIdGenerator>::next(&self)
  }
}

impl std::fmt::Debug for TunnelId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TunnelID")
      .field("inner", &self.inner())
      .finish()
  }
}
