use crate::util::{self, parse_socketaddr};
use anyhow::{Error as AnyErr, Result, Context as AnyhowContext};
use futures::future;
use futures::future::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  path::{Path, PathBuf},
  sync::Arc,
  task::{Context, Poll},
};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ClientArgs {
  pub authority_cert: PathBuf,
  pub driver_host: std::net::SocketAddr,
  pub driver_san: String,
  pub proxy_target_host: std::net::SocketAddr,
}

pub async fn client_arg_handling(args: &'_ clap::ArgMatches<'_>) -> Result<ClientArgs> {
  let cert_path = Path::new(args.value_of("authority").unwrap()).to_path_buf();
  Ok(ClientArgs {
    authority_cert: cert_path,
    driver_host: parse_socketaddr(args.value_of("driver").unwrap())?,
    driver_san: args.value_of("driver-san").unwrap().into(),
    proxy_target_host: parse_socketaddr(args.value_of("target").unwrap())?,
  })
}

pub async fn client_main(config: ClientArgs) -> Result<()> {
  let cert_der = std::fs::read(config.authority_cert).context("Failed reading cert file")?;
  let authority = quinn::Certificate::from_der(&cert_der)?;
  let quinn_config = {
    let mut qc = quinn::ClientConfigBuilder::default();
    qc.add_certificate_authority(authority)?;
    qc.protocols(util::ALPN_QUIC_HTTP);
    qc.build()
  };

  let (endpoint, _incoming) = {
    let mut response_endpoint = quinn::Endpoint::builder();
    response_endpoint.default_client_config(quinn_config);
    response_endpoint.bind(&"[::]:0".parse()?)? // Should this be IPv4 if the server is?
  };

  let connected: quinn::NewConnection = {
    let connecting: Result<_, _> = endpoint
      .connect(&config.driver_host, &config.driver_san)
      .context("Connecting to server")?
      .await;
    connecting.context("Finalizing connection to server...")?
  };
  println!("Connected to {:?}", &connected.connection.remote_address());

  println!("Disconnecting...");
  endpoint.wait_idle().await;
  Ok(())
}


