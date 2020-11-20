use crate::util::{
  self,
  framed::{read_frame_vec, write_frame},
  validators::parse_socketaddr,
};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use futures::future;
use futures::future::*;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  path::{Path, PathBuf},
  pin::Pin,
  sync::Arc,
  task::{Context, Poll},
};

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct MetaStreamHeader {}

impl MetaStreamHeader {
  pub fn new() -> MetaStreamHeader {
    MetaStreamHeader {}
  }

  pub async fn read_from_stream<T: tokio::io::AsyncRead + Unpin>(
    s: &mut T,
  ) -> Result<MetaStreamHeader> {
    let buffer = read_frame_vec(s)
      .await
      .context("Failure reading stream header")?
      .into_boxed_slice(); // Drop the ability to resize the buffer
    serde_json::from_slice::<MetaStreamHeader>(&buffer).context("Error decoding stream header json")
  }

  pub async fn write_to_stream<T: tokio::io::AsyncWrite + Unpin>(&self, s: &mut T) -> Result<()> {
    let buffer = serde_json::to_vec(&self)
      .context("Failure serializing stream header")?
      .into_boxed_slice(); // Drop the ability to resize the buffer
    write_frame(s, &buffer)
      .await
      .context("Failure writing stream header")
  }
}

#[cfg(test)]
mod tests {
  use crate::common::MetaStreamHeader;

  #[async_std::test]
  async fn stream_header_serialization_roundtrip() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let buffer: Vec<u8> = Vec::new();
    let mut cursor = std::io::Cursor::new(buffer);
    let original = MetaStreamHeader::new();
    original
      .write_to_stream(&mut cursor)
      .await
      .expect("Writing to stream must succeed");
    cursor.set_position(0);
    let deserialized = MetaStreamHeader::read_from_stream(&mut cursor)
      .await
      .expect("Reading header from stream must succeed");
    assert_eq!(original, deserialized);
  }
}
