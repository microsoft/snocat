// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use super::traits::*;
#[warn(unused_imports)]
use crate::server::deferred::SnocatClientIdentifier;
use crate::util::tunnel_stream::TunnelStream;
use anyhow::{Context, Error as AnyErr, Result};
use futures::future::BoxFuture;
use futures::{AsyncWriteExt, FutureExt};
use std::marker::Unpin;
use tokio::stream::StreamExt;

pub struct SimpleAckAuthenticationHandler {}

impl SimpleAckAuthenticationHandler {
  pub fn new() -> SimpleAckAuthenticationHandler {
    SimpleAckAuthenticationHandler {}
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
    mut channel: Box<dyn TunnelStream + Send + Unpin + 'a>,
    tunnel_info: TunnelInfo,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<SnocatClientIdentifier>> {
    async move {
      tracing::info!("Sending HELO...");
      let mut buffer = [0u8; 64];
      use std::io::Write;
      use tokio::io::AsyncReadExt;
      use tokio::io::AsyncWriteExt;
      write!(&mut buffer[..], "HELO").unwrap();
      channel.write_all(&buffer).await?;
      buffer = [0u8; 64];
      channel.read_exact(&mut buffer).await?;
      let read_string = std::str::from_utf8(&buffer).unwrap();
      if !read_string.starts_with("HELO/HELO\0") {
        tracing::trace!(raw = read_string, "bad_client_ack");
        return Err(AnyErr::msg("Invalid client ack"));
      }
      tracing::trace!("client_ack");
      let peer_addr = tunnel_info.remote_address();
      let id = SnocatClientIdentifier::new(peer_addr.to_string());
      Ok(id)
    }
    .boxed()
  }
}

impl AuthenticationClient for SimpleAckAuthenticationHandler {
  fn authenticate_client<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin + 'a>,
    _tunnel_info: TunnelInfo,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>> {
    async move {
      let (mut recv, mut send) = tokio::io::split(channel);
      use std::io::Write;
      use tokio::io::AsyncReadExt;
      use tokio::io::AsyncWriteExt;
      let mut header = [0u8; 64];
      AsyncReadExt::read_exact(&mut recv, &mut header).await?; // TODO: Actually read the header
      let first_zero = header.iter().position(|x| *x == 0).unwrap_or(32);
      let read_string = std::str::from_utf8(&header[0..first_zero])
        .unwrap()
        .to_string();
      tracing::debug!("Received header: {}", read_string);
      header = [0u8; 64];
      write!(&mut header[..], "{}/{}", &read_string, &read_string).unwrap();
      AsyncWriteExt::write_all(&mut send, &header).await?;
      Ok(())
    }
    .boxed()
  }
}

#[cfg(test)]
mod tests {
  use crate::common::authentication::{
    perform_authentication, perform_client_authentication, AuthenticationClient,
    AuthenticationHandler, SimpleAckAuthenticationHandler, TunnelInfo,
  };
  use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
  use tokio::io::{duplex, DuplexStream};

  #[tokio::test]
  async fn run_auth() {
    // Use a small buffer size to ensure we don't have a minimum that causes blocking
    let localhost = Ipv6Addr::LOCALHOST;
    let (client_port, server_port) = (40000, 40001);
    let (client, server) = duplex(64);
    let auth_server = SimpleAckAuthenticationHandler::new();
    let auth_client = SimpleAckAuthenticationHandler::new();
    let shutdown_listener = triggered::trigger().1; // Fake "never" listener with a dropped sender
    let client_auth_task = auth_client.authenticate_client(
      Box::new(client),
      TunnelInfo {
        remote_address: std::net::SocketAddr::new(localhost.into(), client_port),
      },
      &shutdown_listener,
    );
    let server_auth_task = auth_server.authenticate(
      Box::new(server),
      TunnelInfo {
        remote_address: std::net::SocketAddr::new(localhost.into(), server_port),
      },
      &shutdown_listener,
    );

    let (client_res, server_res) = futures::future::join(client_auth_task, server_auth_task).await;
    client_res.unwrap();
    let produced_id = server_res.unwrap();
    assert_eq!(format!("{:?}", produced_id), "(snocat ([::1]:40001))");
  }
}
