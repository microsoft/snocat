// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use super::traits::*;
#[warn(unused_imports)]
use crate::server::deferred::SnocatClientIdentifier;
use crate::util::tunnel_stream::TunnelStream;
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
    _channel: Box<dyn TunnelStream + Send + Unpin + 'a>,
    tunnel_info: TunnelInfo,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<SnocatClientIdentifier>> {
    async move {
      let peer_addr = tunnel_info.remote_address();
      let id = SnocatClientIdentifier::new(peer_addr.to_string());
      Ok(id)
    }
    .boxed()
  }
}

impl AuthenticationClient for NoOpAuthenticationHandler {
  fn authenticate_client<'a>(
    &'a self,
    _channel: Box<dyn TunnelStream + Send + Unpin + 'a>,
    _tunnel: TunnelInfo,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>> {
    futures::future::ready(Ok(())).boxed()
  }
}
