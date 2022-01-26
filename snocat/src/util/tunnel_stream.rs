// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use std::io::Error as IOError;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};

/// A duplex stream abstracting over a connection, allowing use of memory streams and Quinn connections
pub trait TunnelStream: AsyncRead + AsyncWrite + Send + Unpin {
  fn as_dyn_mut<'a>(self: &'a mut Self) -> &'a mut dyn TunnelStream
  where
    Self: Sized,
  {
    self
  }
}

impl<'stream, TInner: TunnelStream + ?Sized + 'stream> TunnelStream for &'stream mut TInner {}
impl<TInner: TunnelStream + ?Sized> TunnelStream for Box<TInner> {}

pub struct QuinnTunnelRefStream<'a>(&'a mut quinn::SendStream, &'a mut quinn::RecvStream);

impl<'a> QuinnTunnelRefStream<'a> {
  pub fn new(send: &'a mut quinn::SendStream, recv: &'a mut quinn::RecvStream) -> Self {
    Self(send, recv)
  }
}

pub struct QuinnTunnelStream(quinn::SendStream, quinn::RecvStream);

impl QuinnTunnelStream {
  pub fn new(streams: (quinn::SendStream, quinn::RecvStream)) -> Self {
    Self(streams.0, streams.1)
  }

  pub fn as_ref_tunnel_stream(&mut self) -> QuinnTunnelRefStream {
    QuinnTunnelRefStream(&mut self.0, &mut self.1)
  }
}

impl AsyncWrite for QuinnTunnelRefStream<'_> {
  fn poll_write(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &[u8],
  ) -> Poll<Result<usize, IOError>> {
    let mut parent_ref = self.as_mut();
    AsyncWrite::poll_write(Pin::new(&mut parent_ref.0), cx, buf)
  }

  fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
    let mut parent_ref = self.as_mut();
    AsyncWrite::poll_flush(Pin::new(&mut parent_ref.0), cx)
  }

  fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
    let mut parent_ref = self.as_mut();
    AsyncWrite::poll_shutdown(Pin::new(&mut parent_ref.0), cx)
  }
}

impl AsyncWrite for QuinnTunnelStream {
  fn poll_write(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &[u8],
  ) -> Poll<Result<usize, IOError>> {
    let parent_ref = Pin::into_inner(self);
    let mut ref_stream = QuinnTunnelRefStream::new(&mut parent_ref.0, &mut parent_ref.1);
    AsyncWrite::poll_write(Pin::new(&mut ref_stream), cx, buf)
  }

  fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
    let parent_ref = Pin::into_inner(self);
    let mut ref_stream = QuinnTunnelRefStream::new(&mut parent_ref.0, &mut parent_ref.1);
    AsyncWrite::poll_flush(Pin::new(&mut ref_stream), cx)
  }

  fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
    let parent_ref = Pin::into_inner(self);
    let mut ref_stream = QuinnTunnelRefStream::new(&mut parent_ref.0, &mut parent_ref.1);
    AsyncWrite::poll_shutdown(Pin::new(&mut ref_stream), cx)
  }
}

impl AsyncRead for QuinnTunnelRefStream<'_> {
  fn poll_read(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut tokio::io::ReadBuf<'_>,
  ) -> Poll<futures_io::Result<()>> {
    let parent_ref = Pin::into_inner(self);
    let mut ref_stream = QuinnTunnelRefStream::new(&mut parent_ref.0, &mut parent_ref.1);
    AsyncRead::poll_read(Pin::new(&mut ref_stream), cx, buf)
  }
}

impl AsyncRead for QuinnTunnelStream {
  fn poll_read(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut tokio::io::ReadBuf<'_>,
  ) -> Poll<futures_io::Result<()>> {
    let mut parent_ref = self.as_mut();
    AsyncRead::poll_read(Pin::new(&mut parent_ref.1), cx, buf)
  }
}

impl TunnelStream for QuinnTunnelRefStream<'_> {}
impl TunnelStream for QuinnTunnelStream {}

/// futures-rs compatibility for TunnelStream
// noinspection DuplicatedCode
mod futures_traits {
  use super::QuinnTunnelStream;
  use crate::util::tunnel_stream::TunnelStream;
  use futures::AsyncWrite;
  use std::io::Error as IOError;
  use std::pin::Pin;
  use std::task::{Context, Poll};

  impl futures::io::AsyncWrite for QuinnTunnelStream {
    fn poll_write(
      mut self: Pin<&mut Self>,
      cx: &mut Context<'_>,
      buf: &[u8],
    ) -> Poll<Result<usize, IOError>> {
      AsyncWrite::poll_write(Pin::new(&mut (*self).0), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
      AsyncWrite::poll_flush(Pin::new(&mut (*self).0), cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
      AsyncWrite::poll_close(Pin::new(&mut (*self).0), cx)
    }
  }

  impl futures::io::AsyncRead for QuinnTunnelStream {
    fn poll_read(
      self: Pin<&mut Self>,
      cx: &mut Context<'_>,
      buf: &mut [u8],
    ) -> Poll<Result<usize, IOError>> {
      use tokio::io::ReadBuf;
      let mut buf = ReadBuf::new(buf);
      futures::ready!(tokio::io::AsyncRead::poll_read(self, cx, &mut buf))?;
      Poll::Ready(Ok(buf.filled().len()))
    }
  }

  impl futures::io::AsyncWrite for dyn TunnelStream {
    fn poll_write(
      self: Pin<&mut Self>,
      cx: &mut Context<'_>,
      buf: &[u8],
    ) -> Poll<Result<usize, IOError>> {
      tokio::io::AsyncWrite::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
      tokio::io::AsyncWrite::poll_flush(self, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
      tokio::io::AsyncWrite::poll_shutdown(self, cx)
    }
  }

  impl futures::io::AsyncRead for dyn TunnelStream {
    fn poll_read(
      self: Pin<&mut Self>,
      cx: &mut Context<'_>,
      buf: &mut [u8],
    ) -> Poll<Result<usize, IOError>> {
      use tokio::io::ReadBuf;
      let mut buf = ReadBuf::new(buf);
      futures::ready!(tokio::io::AsyncRead::poll_read(self, cx, &mut buf))?;
      Poll::Ready(Ok(buf.filled().len()))
    }
  }

  impl futures::io::AsyncWrite for super::WrappedStream {
    fn poll_write(
      self: Pin<&mut Self>,
      cx: &mut Context<'_>,
      buf: &[u8],
    ) -> Poll<Result<usize, IOError>> {
      tokio::io::AsyncWrite::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
      tokio::io::AsyncWrite::poll_flush(self, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
      tokio::io::AsyncWrite::poll_shutdown(self, cx)
    }
  }

  impl futures::io::AsyncRead for super::WrappedStream {
    fn poll_read(
      self: Pin<&mut Self>,
      cx: &mut Context<'_>,
      buf: &mut [u8],
    ) -> Poll<Result<usize, IOError>> {
      use tokio::io::ReadBuf;
      let mut buf = ReadBuf::new(buf);
      futures::ready!(tokio::io::AsyncRead::poll_read(self, cx, &mut buf))?;
      Poll::Ready(Ok(buf.filled().len()))
    }
  }
}

pub enum WrappedStream {
  Boxed(
    Box<dyn AsyncRead + Send + Sync + Unpin + 'static>,
    Box<dyn AsyncWrite + Send + Sync + Unpin + 'static>,
  ),
  Quinn(QuinnTunnelStream),
  QuinnRef(QuinnTunnelRefStream<'static>),
  DuplexStream(tokio::io::DuplexStream),
}

impl WrappedStream {
  #[cfg(test)]
  /// Asserts that WrappedStream complies with TunnelStream, Send, and Unpin traits
  fn _assert_traits() {
    let _x: &(dyn TunnelStream + Send + Sync + Unpin) =
      &WrappedStream::DuplexStream(tokio::io::duplex(64).0);
    unreachable!("Compile-time static assertion function should never be called");
  }

  pub fn duplex(max_buf_size: usize) -> (WrappedStream, WrappedStream) {
    let (a, b) = tokio::io::duplex(max_buf_size);
    (a.into(), b.into())
  }
}

impl Into<WrappedStream> for tokio::io::DuplexStream {
  fn into(self) -> WrappedStream {
    WrappedStream::DuplexStream(self)
  }
}

impl AsyncRead for WrappedStream {
  fn poll_read(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut tokio::io::ReadBuf<'_>,
  ) -> Poll<futures_io::Result<()>> {
    match self.get_mut() {
      WrappedStream::Quinn(ref mut s) => AsyncRead::poll_read(Pin::new(&mut s.1), cx, buf),
      WrappedStream::QuinnRef(ref mut s) => AsyncRead::poll_read(Pin::new(&mut s.1), cx, buf),
      WrappedStream::DuplexStream(ref mut s) => AsyncRead::poll_read(Pin::new(s), cx, buf),
      WrappedStream::Boxed(ref mut s, _) => AsyncRead::poll_read(Pin::new(&mut *s), cx, buf),
    }
  }
}

impl AsyncWrite for WrappedStream {
  fn poll_write(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &[u8],
  ) -> Poll<Result<usize, IOError>> {
    match self.get_mut() {
      WrappedStream::Quinn(ref mut s) => AsyncWrite::poll_write(Pin::new(&mut s.0), cx, buf),
      WrappedStream::QuinnRef(ref mut s) => AsyncWrite::poll_write(Pin::new(&mut s.0), cx, buf),
      WrappedStream::DuplexStream(ref mut s) => AsyncWrite::poll_write(Pin::new(s), cx, buf),
      WrappedStream::Boxed(_, ref mut s) => AsyncWrite::poll_write(Pin::new(&mut *s), cx, buf),
    }
  }

  fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
    match self.get_mut() {
      WrappedStream::Quinn(ref mut s) => AsyncWrite::poll_flush(Pin::new(&mut s.0), cx),
      WrappedStream::QuinnRef(ref mut s) => AsyncWrite::poll_flush(Pin::new(&mut s.0), cx),
      WrappedStream::DuplexStream(ref mut s) => AsyncWrite::poll_flush(Pin::new(s), cx),
      WrappedStream::Boxed(_, ref mut s) => AsyncWrite::poll_flush(Pin::new(&mut *s), cx),
    }
  }

  fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
    match self.get_mut() {
      WrappedStream::Quinn(ref mut s) => AsyncWrite::poll_shutdown(Pin::new(&mut s.0), cx),
      WrappedStream::QuinnRef(ref mut s) => AsyncWrite::poll_shutdown(Pin::new(&mut s.0), cx),
      WrappedStream::DuplexStream(ref mut s) => AsyncWrite::poll_shutdown(Pin::new(s), cx),
      WrappedStream::Boxed(_, ref mut s) => AsyncWrite::poll_shutdown(Pin::new(&mut *s), cx),
    }
  }
}

impl TunnelStream for WrappedStream {}
