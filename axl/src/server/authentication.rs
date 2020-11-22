//! Types supporting authentication of AXL tunnel connections

use crate::server::deferred::AxlClientIdentifier;
use anyhow::{Error as AnyErr, Result};
use futures::future::BoxFuture;
use futures::{AsyncWriteExt, FutureExt};
use tokio::stream::StreamExt;

pub trait AuthenticationHandler: std::fmt::Debug + Send + Sync {
  fn authenticate<'a>(
    &self,
    tunnel: &'a mut quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<AxlClientIdentifier>>;
}

pub trait AuthenticationClient: std::fmt::Debug + Send + Sync {
  fn authenticate_client<'a>(
    &self,
    tunnel: &'a mut quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>>;
}

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
    &self,
    tunnel: &'a mut quinn::NewConnection,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<AxlClientIdentifier>> {
    async move {
      let peer_addr = tunnel.connection.remote_address();
      let id = AxlClientIdentifier::new(peer_addr.to_string());
      let mut auth_channel = tunnel.connection.open_bi().await?;
      tracing::info!("Sending HELO...");
      let mut buffer = [0u8; 64];
      use std::io::Write;
      write!(&mut buffer[..], "HELO").unwrap();
      auth_channel.0.write_all(&buffer).await?;
      buffer = [0u8; 64];
      auth_channel.1.read_exact(&mut buffer).await?;
      let read_string = std::str::from_utf8(&buffer).unwrap();
      if !read_string.starts_with("HELO/HELO\0") {
        tracing::trace!(raw = read_string, "bad_client_ack");
        return Err(AnyErr::msg("Invalid client ack"));
      }
      tracing::trace!("client_ack");
      let _ = auth_channel.0.close().await;
      Ok(id)
    }
    .boxed()
  }
}

impl AuthenticationClient for SimpleAckAuthenticationHandler {
  fn authenticate_client<'a>(
    &self,
    tunnel: &'a mut quinn::NewConnection,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>> {
    async move {
      let (mut send, mut recv) = tunnel
        .bi_streams
        .next()
        .await
        .map(|i| i.map_err(|e| AnyErr::new(e)))
        .unwrap_or_else(|| {
          Err(AnyErr::msg(
            "Tunnel connection closed remotely before authentication",
          ))
        })?;
      use std::io::Write;
      let mut header = [0u8; 64];
      recv.read_exact(&mut header).await?; // TODO: Actually read the header
      let first_zero = header.iter().position(|x| *x == 0).unwrap_or(32);
      let read_string = std::str::from_utf8(&header[0..first_zero])
        .unwrap()
        .to_string();
      tracing::debug!("Received header: {}", read_string);
      header = [0u8; 64];
      write!(&mut header[..], "{}/{}", &read_string, &read_string).unwrap();
      send.write_all(&header).await?;
      let _ = send.close().await;
      Ok(())
    }
    .boxed()
  }
}
