#[warn(unused_imports)]
use crate::server::deferred::SnocatClientIdentifier;
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use futures::future::BoxFuture;
use futures::{AsyncWriteExt, FutureExt};
use tokio::stream::StreamExt;
use std::marker::Unpin;
use std::sync::{Arc};
use tokio::io::{AsyncRead, AsyncWrite};
use std::pin::Pin;
use std::task::{Poll, Context};
use std::io::Error;

pub struct QuinnTunnelStream(quinn::SendStream, quinn::RecvStream);

pub trait TunnelStream :  AsyncRead + AsyncWrite + Send + Sync + Unpin {
}

impl AsyncWrite for QuinnTunnelStream {
  fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
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

impl AsyncRead for QuinnTunnelStream {
  fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, std::io::Error>> {
    let mut parent_ref = self.as_mut();
    AsyncRead::poll_read(Pin::new(&mut parent_ref.1), cx, buf)
  }
}

impl TunnelStream for QuinnTunnelStream {
}

pub struct TunnelServer {
}

impl TunnelServer {
  pub fn new() -> Self {
    Self {
    }
  }

  pub async fn create_channel(self: &Arc<Self>) -> Box<dyn TunnelStream + 'static> {
    todo!("Implement based on the provided backend")
  }
}

pub struct TunnelClient {
}

impl TunnelClient {
  pub fn new() -> Self {
    Self {
    }
  }

  pub async fn wait_channel(self: &Arc<Self>) -> Box<dyn TunnelStream> {
    todo!("Implement based on the provided backend")
  }
}

pub fn fake_tunnel() -> (TunnelClient, TunnelServer) {
  todo!("Implement with DuplexStream channels")
}

pub struct TunnelInfo {
  pub remote_address: std::net::SocketAddr,
}

impl TunnelInfo {
  pub fn from_connection<T: quinn::crypto::Session>(connection: &quinn::generic::Connection<T>) -> Self {
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
    channel: (&'a mut (dyn tokio::io::AsyncWrite + Send + Unpin), &'a mut (dyn tokio::io::AsyncRead + Send + Unpin)),
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
        .authenticate_channel( (&mut auth_channel.0, &mut auth_channel.1), tunnel_info, shutdown_notifier)
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
    channel: (&'a mut (dyn tokio::io::AsyncWrite + Send + Unpin), &'a mut (dyn tokio::io::AsyncRead + Send + Unpin)),
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
        .authenticate_client_channel((&mut auth_channel.0, &mut auth_channel.1), tunnel_info, shutdown_notifier)
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
