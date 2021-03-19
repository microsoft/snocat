// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use super::traits::*;
#[warn(unused_imports)]
use crate::server::deferred::SnocatClientIdentifier;
use crate::{
  common::protocol::tunnel::{TunnelName, TunnelSide},
  util::tunnel_stream::TunnelStream,
};
use anyhow::{Context, Error as AnyErr, Result};
use futures::{future::BoxFuture, TryFutureExt};
use futures::{AsyncWriteExt, FutureExt, StreamExt};
use std::marker::Unpin;

pub struct SimpleAckAuthenticationHandler {}

impl SimpleAckAuthenticationHandler {
  pub fn new() -> SimpleAckAuthenticationHandler {
    SimpleAckAuthenticationHandler {}
  }

  fn authenticate_listen_side<'a>(
    &'a self,
    mut channel: Box<dyn TunnelStream + Send + Unpin + 'a>,
    tunnel_info: TunnelInfo,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<Result<TunnelName, RemoteAuthenticationError>, AuthenticationError>> {
    async move {
      tracing::info!("Sending HELO...");
      let mut buffer = [0u8; 64];
      use std::io::Write;
      use tokio::io::AsyncReadExt;
      use tokio::io::AsyncWriteExt;
      write!(&mut buffer[..], "HELO").unwrap();
      match channel
        .write_all(&buffer)
        .map_err(|_e| RemoteAuthenticationError::ProtocolViolation("Write refused".into()))
        .await
      {
        Ok(()) => (),
        Err(e) => return Ok(Err(e)),
      };
      buffer = [0u8; 64];
      match channel
        .read_exact(&mut buffer)
        .map_err(|_e| RemoteAuthenticationError::ProtocolViolation("Read unavailable".into()))
        .await
      {
        Ok(_read_count) => (),
        Err(e) => return Ok(Err(e)),
      };
      let read_string = std::str::from_utf8(&buffer).unwrap();
      if !read_string.starts_with("HELO/HELO\0") {
        tracing::trace!(raw = read_string, "bad_client_ack");
        return Ok(Err(RemoteAuthenticationError::ProtocolViolation(
          "Invalid client ack".to_string(),
        )));
      };
      tracing::trace!("client_ack");
      let peer_addr = tunnel_info.addr;
      let id = SnocatClientIdentifier::new(peer_addr.to_string());
      Ok(Ok(id))
    }
    .boxed()
  }

  fn authenticate_connecting_side<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin + 'a>,
    tunnel_info: TunnelInfo,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<Result<TunnelName, RemoteAuthenticationError>, AuthenticationError>> {
    async move {
      let (mut recv, mut send) = tokio::io::split(channel);
      use std::io::Write;
      use tokio::io::AsyncReadExt;
      use tokio::io::AsyncWriteExt;
      let mut header = [0u8; 64];
      match AsyncReadExt::read_exact(&mut recv, &mut header)
        .map_err(|_| RemoteAuthenticationError::ProtocolViolation("Read unavailable".into()))
        .await
      {
        // TODO: Actually read the header
        Ok(_read_count) => (),
        Err(e) => return Ok(Err(e)),
      };
      let first_zero = header.iter().position(|x| *x == 0).unwrap_or(32);
      let read_string = std::str::from_utf8(&header[0..first_zero])
        .unwrap()
        .to_string();
      tracing::debug!("Received header: {}", read_string);
      header = [0u8; 64];
      write!(&mut header[..], "{}/{}", &read_string, &read_string).unwrap();
      match AsyncWriteExt::write_all(&mut send, &header)
        .map_err(|_| RemoteAuthenticationError::ProtocolViolation("Write refused".into()))
        .await
      {
        Ok(()) => (),
        Err(e) => return Ok(Err(e)),
      };
      let peer_addr = tunnel_info.addr;
      let id = SnocatClientIdentifier::new(peer_addr.to_string());
      Ok(Ok(id))
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
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<Result<TunnelName, RemoteAuthenticationError>, AuthenticationError>> {
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
  use super::SimpleAckAuthenticationHandler;
  use crate::common::{
    authentication::{perform_authentication, AuthenticationHandler, TunnelInfo},
    protocol::tunnel::{duplex, EntangledTunnels, TunnelName},
  };
  use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

  #[tokio::test]
  async fn run_auth() {
    let EntangledTunnels {
      mut listener,
      mut connector,
    } = duplex();

    let never_shutdown = triggered::trigger().1; // "never" listener due to dropped sender
    let auth_server = SimpleAckAuthenticationHandler::new();
    let auth_client = SimpleAckAuthenticationHandler::new();

    let client_auth_task = perform_authentication(
      &auth_client,
      &connector.0,
      &mut connector.1,
      &never_shutdown,
    );
    let server_auth_task =
      perform_authentication(&auth_server, &listener.0, &mut listener.1, &never_shutdown);

    let (client_res, server_res) = futures::future::join(client_auth_task, server_auth_task).await;
    assert_eq!(
      client_res.unwrap().unwrap(),
      TunnelName::new("Unidentified")
    );
    assert_eq!(
      server_res.unwrap().unwrap(),
      TunnelName::new("Unidentified")
    );
  }
}
