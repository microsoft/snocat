#[warn(unused_imports)]
use crate::server::deferred::SnocatClientIdentifier;
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use futures::future::BoxFuture;
use futures::{AsyncWriteExt, FutureExt};
use std::io::Error;
use std::marker::Unpin;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::stream::StreamExt;

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
  ) -> Poll<Result<usize, Error>> {
    let mut parent_ref = self.as_mut();
    AsyncWrite::poll_write(Pin::new(&mut parent_ref.0), cx, buf)
  }

  fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
    let mut parent_ref = self.as_mut();
    AsyncWrite::poll_flush(Pin::new(&mut parent_ref.0), cx)
  }

  fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
    let mut parent_ref = self.as_mut();
    AsyncWrite::poll_shutdown(Pin::new(&mut parent_ref.0), cx)
  }
}

impl<TSession: quinn::crypto::Session> AsyncWrite for QuinnTunnelStream<TSession> {
  fn poll_write(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &[u8],
  ) -> Poll<Result<usize, Error>> {
    let parent_ref = Pin::into_inner(self);
    let mut ref_stream = QuinnTunnelRefStream::new(&mut parent_ref.0, &mut parent_ref.1);
    AsyncWrite::poll_write(Pin::new(&mut ref_stream), cx, buf)
  }

  fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
    let parent_ref = Pin::into_inner(self);
    let mut ref_stream = QuinnTunnelRefStream::new(&mut parent_ref.0, &mut parent_ref.1);
    AsyncWrite::poll_flush(Pin::new(&mut ref_stream), cx)
  }

  fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
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

pub struct TunnelInfo {
  pub remote_address: std::net::SocketAddr,
}

impl TunnelInfo {
  pub fn from_connection<T: quinn::crypto::Session>(
    connection: &quinn::generic::Connection<T>,
  ) -> Self {
    Self {
      remote_address: connection.remote_address(),
    }
  }
  pub fn from_new_connection(new_connection: &quinn::NewConnection) -> Self {
    Self::from_connection(&new_connection.connection)
  }

  pub fn remote_address(&self) -> std::net::SocketAddr {
    self.remote_address
  }
}

pub trait AuthenticationHandler: std::fmt::Debug + Send + Sync {
  fn authenticate<'a>(
    &'a self,
    tunnel: &'a mut quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<SnocatClientIdentifier>>;
}

pub trait AuthenticationClient: std::fmt::Debug + Send + Sync {
  fn authenticate_client<'a>(
    &'a self,
    tunnel: &'a mut quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>>;
}

pub trait BidiChannelAuthenticationHandler: AuthenticationHandler {
  fn authenticate_channel<'a>(
    &'a self,
    channel: &'a mut (dyn TunnelStream + Send + Unpin),
    tunnel: TunnelInfo,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<SnocatClientIdentifier>>;
}

impl<T: BidiChannelAuthenticationHandler> AuthenticationHandler for T {
  fn authenticate<'a>(
    &'a self,
    tunnel: &'a mut quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<SnocatClientIdentifier>> {
    async move {
      let mut auth_channel = tunnel.connection.open_bi().await?;
      let tunnel_info = TunnelInfo::from_new_connection(&tunnel);
      let res = self
        .authenticate_channel(
          &mut QuinnTunnelRefStream::new(&mut auth_channel.0, &mut auth_channel.1),
          tunnel_info,
          shutdown_notifier,
        )
        .await;
      let closed = auth_channel
        .0
        .close()
        .await
        .context("Failure closing authentication channel");
      match (res, closed) {
        (Ok(id), Err(e)) => {
          tracing::warn!(
            "Failure in closing of client authentication channel {:?}",
            e
          );
          Ok(id) // Failure here means the channel was already closed by the other party
        }
        (Err(e), _) => Err(e),
        (Ok(id), _) => Ok(id),
      }
    }
    .boxed()
  }
}

pub trait BidiChannelAuthenticationClient: AuthenticationClient {
  fn authenticate_client_channel<'a>(
    &'a self,
    channel: &'a mut (dyn TunnelStream + Send + Unpin),
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>>;
}

impl<T: BidiChannelAuthenticationClient> AuthenticationClient for T {
  fn authenticate_client<'a>(
    &'a self,
    tunnel: &'a mut quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>> {
    async move {
      let mut auth_channel = tunnel
        .bi_streams
        .next()
        .await
        .map(|i| i.map_err(|e| AnyErr::new(e)))
        .unwrap_or_else(|| {
          Err(AnyErr::msg(
            "Tunnel connection closed remotely before authentication",
          ))
        })?;
      let tunnel_info = TunnelInfo::from_new_connection(&tunnel);
      let res = self
        .authenticate_client_channel(
          &mut QuinnTunnelRefStream::new(&mut auth_channel.0, &mut auth_channel.1),
          tunnel_info,
          shutdown_notifier,
        )
        .await;
      tracing::debug!("Authenticated with result {:?}", &res);
      let closed = auth_channel
        .0
        .close()
        .await
        .context("Failure closing authentication channel");
      match (res, closed) {
        (Ok(id), Err(e)) => {
          tracing::warn!("Failure in closing of authentication channel {:?}", e);
          Ok(id) // Failure here means the channel was already closed by the other party
        }
        (Err(e), _) => Err(e),
        (Ok(id), _) => Ok(id),
      }
    }
    .boxed()
  }
}
