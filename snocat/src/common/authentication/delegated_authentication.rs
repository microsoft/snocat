#[warn(unused_imports)]
use crate::server::deferred::SnocatClientIdentifier;
use anyhow::{Context, Error as AnyErr, Result};
use futures::future::BoxFuture;
use futures::{AsyncWriteExt, FutureExt};
use tokio::stream::StreamExt;
use super::traits::*;

pub struct DelegatedAuthenticationHandler {}

impl DelegatedAuthenticationHandler {
  pub fn new() -> DelegatedAuthenticationHandler {
    DelegatedAuthenticationHandler {}
  }
}

impl std::fmt::Debug for DelegatedAuthenticationHandler {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "({})", std::any::type_name::<Self>())
  }
}

impl BidiChannelAuthenticationHandler for DelegatedAuthenticationHandler {
  fn authenticate_channel<'a>(
    &'a self,
    _channel: &'a mut (quinn::SendStream, quinn::RecvStream),
    tunnel: &'a quinn::NewConnection,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<SnocatClientIdentifier>> {
    async move {
      let peer_addr = tunnel.connection.remote_address();
      let id = SnocatClientIdentifier::new(peer_addr.to_string());
      Ok(id)
    }
      .boxed()
  }
}

impl BidiChannelAuthenticationClient for DelegatedAuthenticationHandler {
  fn authenticate_client_channel<'a>(
    &'a self,
    _channel: &'a mut (quinn::SendStream, quinn::RecvStream),
    _tunnel: &'a quinn::NewConnection,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>> {
    async move {
      Ok(())
    }
      .boxed()
  }
}
