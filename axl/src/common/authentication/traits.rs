#[warn(unused_imports)]
use crate::server::deferred::AxlClientIdentifier;
use anyhow::{Context, Error as AnyErr, Result};
use futures::future::BoxFuture;
use futures::{AsyncWriteExt, FutureExt};
use tokio::stream::StreamExt;

pub trait AuthenticationHandler: std::fmt::Debug + Send + Sync {
  fn authenticate<'a>(
    &'a self,
    tunnel: &'a mut quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<AxlClientIdentifier>>;
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
    channel: &'a mut (quinn::SendStream, quinn::RecvStream),
    tunnel: &'a quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<AxlClientIdentifier>>;
}

impl<T: BidiChannelAuthenticationHandler> AuthenticationHandler for T {
  fn authenticate<'a>(
    &'a self,
    tunnel: &'a mut quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<AxlClientIdentifier>> {
    async move {
      let mut auth_channel = tunnel.connection.open_bi().await?;
      let res = self
        .authenticate_channel(&mut auth_channel, &tunnel, shutdown_notifier)
        .await;
      let closed = auth_channel
        .0
        .close()
        .await
        .context("Failure closing authentication channel");
      match (res, closed) {
        (Err(e), _) | (_, Err(e)) => Err(e),
        (Ok(id), _) => Ok(id),
      }
    }
      .boxed()
  }
}

pub trait BidiChannelAuthenticationClient: AuthenticationClient {
  fn authenticate_client_channel<'a>(
    &'a self,
    channel: &'a mut (quinn::SendStream, quinn::RecvStream),
    tunnel: &'a quinn::NewConnection,
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
      let res = self
        .authenticate_client_channel(&mut auth_channel, &tunnel, shutdown_notifier)
        .await;
      let closed = auth_channel
        .0
        .close()
        .await
        .context("Failure closing authentication channel");
      match (res, closed) {
        (Err(e), _) | (_, Err(e)) => Err(e),
        (Ok(id), _) => Ok(id),
      }
    }
      .boxed()
  }
}

