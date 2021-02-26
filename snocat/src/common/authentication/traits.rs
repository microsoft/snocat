// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#[warn(unused_imports)]
use crate::server::deferred::SnocatClientIdentifier;
use crate::util::tunnel_stream::{QuinnTunnelStream, TunnelStream};
use anyhow::{Error as AnyErr, Result};
use futures::future::BoxFuture;
use std::marker::Unpin;
use tokio::stream::StreamExt;
use triggered::Listener;

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
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<SnocatClientIdentifier>>;
}

pub trait AuthenticationClient: std::fmt::Debug + Send + Sync {
  fn authenticate_client<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>>;
}

impl<T: AuthenticationHandler + ?Sized> AuthenticationHandler for Box<T> {
  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a Listener,
  ) -> BoxFuture<'a, Result<SnocatClientIdentifier, anyhow::Error>> {
    self
      .as_ref()
      .authenticate(channel, tunnel_info, shutdown_notifier)
  }
}

impl<T: AuthenticationClient + ?Sized> AuthenticationClient for Box<T> {
  fn authenticate_client<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a Listener,
  ) -> BoxFuture<'a, Result<(), anyhow::Error>> {
    self
      .as_ref()
      .authenticate_client(channel, tunnel_info, shutdown_notifier)
  }
}

pub async fn perform_authentication<'a>(
  handler: &'a impl AuthenticationHandler,
  tunnel: &'a mut quinn::NewConnection,
  shutdown_notifier: &'a triggered::Listener,
) -> Result<SnocatClientIdentifier> {
  let auth_channel = tunnel.connection.open_bi().await?;
  let tunnel_info = TunnelInfo::from_new_connection(&tunnel);
  handler
    .authenticate(
      Box::new(QuinnTunnelStream::new(auth_channel)),
      tunnel_info,
      shutdown_notifier,
    )
    .await
}

pub async fn perform_client_authentication<'a>(
  client: &'a impl AuthenticationClient,
  tunnel: &'a mut quinn::NewConnection,
  shutdown_notifier: &'a triggered::Listener,
) -> Result<()> {
  let auth_channel = tunnel
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
  let res = client
    .authenticate_client(
      Box::new(QuinnTunnelStream::new(auth_channel)),
      tunnel_info,
      shutdown_notifier,
    )
    .await;
  tracing::debug!("Authenticated with result {:?}", &res);
  res
}
