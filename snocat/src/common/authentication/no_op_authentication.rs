use super::traits::*;
#[warn(unused_imports)]
use crate::server::deferred::SnocatClientIdentifier;
use anyhow::{Context, Error as AnyErr, Result};
use futures::future::BoxFuture;
use futures::{AsyncWriteExt, FutureExt};
use tokio::stream::StreamExt;

pub struct NoOpAuthenticationHandler {}

impl NoOpAuthenticationHandler {
  pub fn new() -> NoOpAuthenticationHandler {
    NoOpAuthenticationHandler {}
  }
}

impl std::fmt::Debug for NoOpAuthenticationHandler {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "({})",
      std::any::type_name::<NoOpAuthenticationHandler>()
    )
  }
}

impl AuthenticationHandler for NoOpAuthenticationHandler {
  fn authenticate<'a>(
    &'a self,
    tunnel: &'a mut quinn::NewConnection,
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

impl AuthenticationClient for NoOpAuthenticationHandler {
  fn authenticate_client<'a>(
    &'a self,
    _tunnel: &'a mut quinn::NewConnection,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>> {
    futures::future::ready(Ok(())).boxed()
  }
}
