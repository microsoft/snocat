use crate::common::MetaStreamHeader;
use crate::util::{self, parse_socketaddr};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use futures::future::*;
use futures::*;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  boxed::Box,
  path::{Path, PathBuf},
  pin::Pin,
  sync::Arc,
  task::{Context, Poll},
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
  let config = Arc::new(config);
  let cert_der = std::fs::read(&config.authority_cert).context("Failed reading cert file")?;
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

  let quinn::NewConnection {
    connection,
    bi_streams,
    ..
  } = {
    let connecting: Result<_, _> = endpoint
      .connect(&config.driver_host, &config.driver_san)
      .context("Connecting to server")?
      .await;
    connecting.context("Finalizing connection to server...")?
  };
  println!("Connected to {:?}", &connection.remote_address());

  println!("Setting up stream handling...");
  {
    let config = Arc::clone(&config);
    let build_connection_handler = &move |stream_header: MetaStreamHeader| {
      let config = Arc::clone(&config);
      async move {
        let builder: BoxFuture<Result<TcpStream, AnyErr>> =
          TcpStream::connect((config.proxy_target_host).clone())
            .map_err(|e| e.into())
            .fuse()
            .boxed();
        (stream_header, builder)
      }
      .fuse()
      .boxed()
    };

    let mut connection_id: ConnectionId = 0;
    bi_streams
      .map_err(|e| e.into())
      .try_for_each_concurrent(None, |(send, recv)| {
        println!("New bidirectional stream received, allocating handler coroutine...");
        handle_connection(
          {
            let tmp = connection_id;
            connection_id += 1;
            tmp
          },
          (send, recv),
          build_connection_handler,
        )
      })
      .await?;
  }

  println!("Disconnecting...");
  endpoint.wait_idle().await;
  Ok(())
}

type ConnectionId = u32;

type ProxyConnectionProvider<'a, 'b> = dyn Fn(
  MetaStreamHeader,
) -> future::BoxFuture<
  'a,
  (
    MetaStreamHeader,
    future::BoxFuture<'b, anyhow::Result<TcpStream>>,
  ),
>;

async fn handle_connection(
  id: ConnectionId,
  (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
  build_proxy_connection: &ProxyConnectionProvider<'_, '_>,
) -> Result<()> {
  println!(
    "Received new connection {} from driver... Loading metadata header...",
    id
  );
  let header = MetaStreamHeader::read_from_stream(&mut recv).await?;

  println!("Header received for connection {}: {:#?}", id, header);
  let (header, await_connection): (MetaStreamHeader, _) = build_proxy_connection(header).await;
  println!("Establishing proxy for connection {}...", id);
  let connection: TcpStream = await_connection.await?;

  let (mut reader, mut writer) = connection.split();

  future::try_join(
    async_std::io::copy(&mut recv, &mut writer).fuse(),
    async_std::io::copy(&mut reader, &mut send).fuse(),
  )
  .await?;

  println!("Closed connection {}", id);
  Ok(())
}
