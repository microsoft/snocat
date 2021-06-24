// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use super::traits::*;
#[warn(unused_imports)]
use crate::{
  common::protocol::tunnel::{TunnelName, TunnelSide},
  util::{cancellation::CancellationListener, tunnel_stream::TunnelStream},
};
use anyhow::{Context, Error as AnyErr, Result};
use futures::{future::BoxFuture, TryFutureExt};
use futures::{AsyncWriteExt, FutureExt, StreamExt};
use std::marker::Unpin;
use tokio_util::sync::CancellationToken;

pub struct SimpleAckAuthenticationHandler {}

impl SimpleAckAuthenticationHandler {
  pub fn new() -> SimpleAckAuthenticationHandler {
    SimpleAckAuthenticationHandler {}
  }

  fn authenticate_listen_side<'a>(
    &'a self,
    mut channel: Box<dyn TunnelStream + Send + Unpin + 'a>,
    tunnel_info: TunnelInfo,
    _shutdown_notifier: &'a CancellationListener,
  ) -> BoxFuture<'a, Result<TunnelName, AuthenticationError>> {
    async move {
      tracing::info!("Sending HELO...");
      let mut buffer = [0u8; 64];
      use std::io::Write;
      use tokio::io::AsyncReadExt;
      use tokio::io::AsyncWriteExt;
      write!(&mut buffer[..], "HELO").unwrap();
      channel
        .write_all(&buffer)
        .map_err(|_e| RemoteAuthenticationError::ProtocolViolation("Write refused".into()))
        .await?;
      buffer = [0u8; 64];
      channel
        .read_exact(&mut buffer)
        .map_err(|_e| RemoteAuthenticationError::ProtocolViolation("Read unavailable".into()))
        .await?;
      std::str::from_utf8(&buffer)
        .map_err(|_| {
          RemoteAuthenticationError::ProtocolViolation("Received string was not valid UTF8".into())
        })
        .and_then(|decoded| {
          if !decoded.starts_with("HELO/HELO\0") {
            tracing::trace!(raw = decoded, "bad_client_ack");
            Err(
              RemoteAuthenticationError::ProtocolViolation("Invalid client ack".to_string()).into(),
            )
          } else {
            tracing::trace!("client_ack");
            Ok(())
          }
        })?;
      let peer_addr = tunnel_info.addr;
      let id = TunnelName::new(peer_addr.to_string());
      Ok(id)
    }
    .boxed()
  }

  fn authenticate_connecting_side<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin + 'a>,
    tunnel_info: TunnelInfo,
    _shutdown_notifier: &'a CancellationListener,
  ) -> BoxFuture<'a, Result<TunnelName, AuthenticationError>> {
    async move {
      let (mut recv, mut send) = tokio::io::split(channel);
      use std::io::Write;
      use tokio::io::AsyncReadExt;
      use tokio::io::AsyncWriteExt;
      let mut header = [0u8; 64];
      // TODO: Actually read the contents of the header
      AsyncReadExt::read_exact(&mut recv, &mut header)
        .map_err(|_| RemoteAuthenticationError::ProtocolViolation("Read unavailable".into()))
        .await?;
      let first_zero = header.iter().position(|x| *x == 0).unwrap_or(32);
      let read_string = std::str::from_utf8(&header[0..first_zero])
        .map_err(|_| {
          RemoteAuthenticationError::ProtocolViolation("Received string was not valid UTF8".into())
        })?
        .to_string();
      tracing::debug!("Received header: {}", read_string);
      header = [0u8; 64];
      write!(&mut header[..], "{}/{}", &read_string, &read_string).unwrap();
      AsyncWriteExt::write_all(&mut send, &header)
        .map_err(|_| RemoteAuthenticationError::ProtocolViolation("Write refused".into()))
        .await?;
      let peer_addr = tunnel_info.addr;
      let id = TunnelName::new(peer_addr.to_string());
      Ok(id)
    }
    .map(Into::into)
    .boxed()
  }
}

impl std::fmt::Debug for SimpleAckAuthenticationHandler {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "({})",
      std::any::type_name::<SimpleAckAuthenticationHandler>()
    )
  }
}

impl AuthenticationHandler for SimpleAckAuthenticationHandler {
  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin + 'a>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a CancellationListener,
  ) -> BoxFuture<'a, Result<TunnelName, AuthenticationError>> {
    match tunnel_info.side {
      TunnelSide::Listen => self
        .authenticate_listen_side(channel, tunnel_info, shutdown_notifier)
        .boxed(),
      TunnelSide::Connect => self
        .authenticate_connecting_side(channel, tunnel_info, shutdown_notifier)
        .boxed(),
    }
  }
}

#[cfg(test)]
mod tests {
  use tokio_util::sync::CancellationToken;

  use super::SimpleAckAuthenticationHandler;
  use crate::{
    common::{
      authentication::{perform_authentication, AuthenticationHandler, TunnelInfo},
      protocol::tunnel::{
        duplex::{channel as duplex, EntangledTunnels},
        TunnelName,
      },
    },
    util::cancellation::CancellationListener,
  };
  use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

  #[tokio::test]
  async fn run_auth() {
    let EntangledTunnels {
      listener,
      connector,
    } = duplex();

    let never_shutdown = CancellationListener::default();
    let auth_server = SimpleAckAuthenticationHandler::new();
    let auth_client = SimpleAckAuthenticationHandler::new();

    let client_auth_task = perform_authentication(&auth_client, &connector, &never_shutdown);
    let server_auth_task = perform_authentication(&auth_server, &listener, &never_shutdown);

    let (client_res, server_res) = futures::future::join(client_auth_task, server_auth_task).await;
    assert_eq!(client_res.unwrap(), TunnelName::new("Unidentified"));
    assert_eq!(server_res.unwrap(), TunnelName::new("Unidentified"));
  }
}
