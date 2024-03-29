// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use futures::future::BoxFuture;
use futures::FutureExt;

use super::{
  AuthenticationAttributes, AuthenticationChannel, AuthenticationError, AuthenticationHandler,
  TunnelInfo,
};
use crate::{common::protocol::tunnel::TunnelName, util::cancellation::CancellationListener};

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
  type Error = std::convert::Infallible;

  fn authenticate<'a>(
    &'a self,
    _channel: &'a mut AuthenticationChannel<'a>,
    tunnel_info: TunnelInfo,
    _shutdown_notifier: &'a CancellationListener,
  ) -> BoxFuture<'a, Result<(TunnelName, AuthenticationAttributes), AuthenticationError<Self::Error>>>
  {
    async move {
      let peer_addr = tunnel_info.addr;
      let id = TunnelName::new(peer_addr.to_string());
      Ok((id, Default::default()))
    }
    .boxed()
  }
}
