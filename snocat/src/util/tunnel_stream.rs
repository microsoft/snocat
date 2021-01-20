use std::io::Error as IOError;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
/// A duplex stream abstracting over a connection, allowing use of memory streams and Quinn connections
pub trait TunnelStream: AsyncRead + AsyncWrite + Send + Unpin {}

impl TunnelStream for tokio::io::DuplexStream {}

pub struct QuinnTunnelRefStream<'a, TSession: quinn::crypto::Session>(
  &'a mut quinn::generic::SendStream<TSession>,
  &'a mut quinn::generic::RecvStream<TSession>,
);

impl<'a, TSession: quinn::crypto::Session> QuinnTunnelRefStream<'a, TSession> {
  pub fn new(
    send: &'a mut quinn::generic::SendStream<TSession>,
    recv: &'a mut quinn::generic::RecvStream<TSession>,
  ) -> Self {
    Self(send, recv)
  }
}

pub struct QuinnTunnelStream<TSession: quinn::crypto::Session>(
  quinn::generic::SendStream<TSession>,
  quinn::generic::RecvStream<TSession>,
);

impl<TSession: quinn::crypto::Session> QuinnTunnelStream<TSession> {
  pub fn as_ref_tunnel_stream(&mut self) -> QuinnTunnelRefStream<TSession> {
    QuinnTunnelRefStream(&mut self.0, &mut self.1)
  }
}

impl<TSession: quinn::crypto::Session> AsyncWrite for QuinnTunnelRefStream<'_, TSession> {
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

impl<TSession: quinn::crypto::Session> AsyncWrite for QuinnTunnelStream<TSession> {
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

impl<TSession: quinn::crypto::Session> AsyncRead for QuinnTunnelRefStream<'_, TSession> {
  fn poll_read(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut [u8],
  ) -> Poll<Result<usize, std::io::Error>> {
    let parent_ref = Pin::into_inner(self);
    let mut ref_stream = QuinnTunnelRefStream::new(&mut parent_ref.0, &mut parent_ref.1);
    AsyncRead::poll_read(Pin::new(&mut ref_stream), cx, buf)
  }
}

impl<TSession: quinn::crypto::Session> AsyncRead for QuinnTunnelStream<TSession> {
  fn poll_read(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut [u8],
  ) -> Poll<Result<usize, std::io::Error>> {
    let mut parent_ref = self.as_mut();
    AsyncRead::poll_read(Pin::new(&mut parent_ref.1), cx, buf)
  }
}

impl<TSession: quinn::crypto::Session> TunnelStream for QuinnTunnelRefStream<'_, TSession> {}
impl<TSession: quinn::crypto::Session> TunnelStream for QuinnTunnelStream<TSession> {}
