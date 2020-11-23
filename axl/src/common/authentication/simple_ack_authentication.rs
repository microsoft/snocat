#[warn(unused_imports)]
use crate::server::deferred::AxlClientIdentifier;
use anyhow::{Context, Error as AnyErr, Result};
use futures::future::BoxFuture;
use futures::{AsyncWriteExt, FutureExt};
use tokio::stream::StreamExt;
use super::traits::*;

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

impl BidiChannelAuthenticationHandler for SimpleAckAuthenticationHandler {
  fn authenticate_channel<'a>(
    &'a self,
    channel: &'a mut (quinn::SendStream, quinn::RecvStream),
    tunnel: &'a quinn::NewConnection,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<AxlClientIdentifier>> {
    async move {
      tracing::info!("Sending HELO...");
      let mut buffer = [0u8; 64];
      use std::io::Write;
      write!(&mut buffer[..], "HELO").unwrap();
      channel.0.write_all(&buffer).await?;
      buffer = [0u8; 64];
      channel.1.read_exact(&mut buffer).await?;
      let read_string = std::str::from_utf8(&buffer).unwrap();
      if !read_string.starts_with("HELO/HELO\0") {
        tracing::trace!(raw = read_string, "bad_client_ack");
        return Err(AnyErr::msg("Invalid client ack"));
      }
      tracing::trace!("client_ack");
      let peer_addr = tunnel.connection.remote_address();
      let id = AxlClientIdentifier::new(peer_addr.to_string());
      Ok(id)
    }
      .boxed()
  }
}

impl BidiChannelAuthenticationClient for SimpleAckAuthenticationHandler {
  fn authenticate_client_channel<'a>(
    &'a self,
    channel: &'a mut (quinn::SendStream, quinn::RecvStream),
    _tunnel: &'a quinn::NewConnection,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, Result<()>> {
    async move {
      let (send, recv) = channel;
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
      Ok(())
    }
      .boxed()
  }
}
