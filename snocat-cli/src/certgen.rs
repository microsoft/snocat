use ::snocat::util::{self, validators::parse_socketaddr};
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
  sync::Arc,
  task::{Context, Poll},
};
use tracing::instrument;

#[instrument]
pub async fn certgen_main(output_base_path: String, host_san: String) -> Result<()> {
  use std::fs;
  use std::path::PathBuf;
  let path = PathBuf::from(output_base_path);
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).context("Directory creation must succeed for certs")?;
  }
  let cert =
    rcgen::generate_simple_self_signed(vec![host_san]).context("Certificate generation failed")?;
  let public_pem = cert.serialize_pem()?;
  let private_pem = cert.serialize_private_key_pem();
  fs::write(
    path.with_file_name(path.file_name().unwrap().to_str().unwrap().to_string() + ".pub.pem"),
    &public_pem,
  )
  .context("Failed writing public key")?;
  fs::write(
    path.with_file_name(path.file_name().unwrap().to_str().unwrap().to_string() + ".priv.pem"),
    &private_pem,
  )
    .context("Failed writing private key")?;
  Ok(())
}
