use crate::util::{self, validators::parse_socketaddr};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use futures::future;
use futures::future::*;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  path::{Path, PathBuf},
  pin::Pin,
  sync::Arc,
  task::{Context, Poll},
};
use serde::{Serialize, Deserialize};

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct MetaStreamHeader {}

impl MetaStreamHeader {
  pub fn new() -> MetaStreamHeader {
    MetaStreamHeader {}
  }

  pub async fn read_from_stream(mut s: Pin<&mut dyn tokio::io::AsyncRead>) -> Result<MetaStreamHeader> {
    use tokio::io::AsyncReadExt;
    let length = s.read_u32().await.context("Failed to read stream header frame length")? as usize;
    let mut buffer = Vec::with_capacity(length);
    buffer.resize_with(length, Default::default);
    s.read_exact(buffer.as_mut_slice()).await.context("Failed reading stream header frame contents")?;
    let buffer = buffer.into_boxed_slice();
    serde_json::from_slice::<MetaStreamHeader>(&buffer)
      .context("Error decoding stream header json")
  }

  pub async fn write_to_stream(&self, mut s: Pin<&mut dyn tokio::io::AsyncWrite>) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let buffer = serde_json::to_vec(&self)
      .context("Error serializing stream header")?
      .into_boxed_slice();
    s.write_u32(buffer.len() as u32).await.context("Failed writing header frame length to stream")?;
    s.write_all(&buffer).await.context("Failed writing header to stream")
  }
}
