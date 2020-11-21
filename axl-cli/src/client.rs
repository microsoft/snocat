use crate::common::MetaStreamHeader;
use crate::util::{self, validators::parse_socketaddr};
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

pub async fn client_main(config: ClientArgs) -> Result<()> {
  let config = Arc::new(config);
  let cert_der = std::fs::read(&config.authority_cert).context("Failed reading cert file")?;
  let authority = quinn::Certificate::from_der(&cert_der)?;
  let quinn_config = {
    let mut qc = quinn::ClientConfigBuilder::default();
    qc.enable_keylog();
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
  tracing::info!(remote = ?connection.remote_address(), "connected");

  tracing::debug!("Setting up stream handling...");
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

    tracing::info!("Stream listener installed; waiting for bistreams...");
    let mut connection_id: ConnectionId = 0;
    bi_streams
      .map_err(|e| e.into())
      .try_for_each_concurrent(None, |(send, recv)| {
        tracing::info!("New bidirectional stream received, allocating handler coroutine...");
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

  tracing::info!("Disconnecting...");
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

#[tracing::instrument(skip(send, recv, build_proxy_connection), err)]
async fn handle_connection(
  id: ConnectionId,
  (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
  build_proxy_connection: &ProxyConnectionProvider<'_, '_>,
) -> Result<()> {
  tracing::info!(
    target: "new connection",
    id = ?id
  );
  // let header = MetaStreamHeader::read_from_stream(&mut recv).await?;
  tracing::trace!("Loading metadata header");
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
  send.flush().await?;

  let header = MetaStreamHeader::new();

  tracing::debug!("Header received for connection {}: {:#?}", id, header);
  let (_header, await_connection): (MetaStreamHeader, _) = build_proxy_connection(header).await;
  tracing::debug!("Establishing proxy for connection {}...", id);
  let connection: TcpStream = await_connection.await?;

  tracing::trace!("Proxying...");
  util::proxy_from_tcp_stream(connection, (&mut send, &mut recv)).await?;

  // util::proxy_stream((&mut recv, &mut send)).await; // Echo server

  tracing::info!("Closed connection {}", id);
  Ok(())
}
