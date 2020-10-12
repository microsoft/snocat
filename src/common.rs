use crate::util::{self, parse_socketaddr};
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

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct MetaStreamHeader {}

impl MetaStreamHeader {
  pub fn new() -> MetaStreamHeader {
    MetaStreamHeader {}
  }

  pub async fn read_from_stream(_s: &mut quinn::RecvStream) -> Result<MetaStreamHeader> {
    unimplemented!()
  }

  pub async fn write_to_stream(_s: &mut quinn::SendStream) -> Result<()> {
    unimplemented!()
  }
}
