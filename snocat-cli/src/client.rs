// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use futures::future::*;
use futures::*;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use snocat::util::framed::read_framed_json;
use snocat::{
  common::{
    authentication::{AuthenticationClient, SimpleAckAuthenticationHandler},
    MetaStreamHeader,
  },
  util::{self, validators::parse_socketaddr},
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
  let cert_pem = std::fs::read(&config.authority_cert).context("Failed reading cert file")?;
  let authority = quinn::CertificateChain::from_pem(&cert_pem)?;
  let authority = quinn::Certificate::from(
    authority
      .iter()
      .nth(0)
      .cloned()
      .ok_or_else(|| AnyErr::msg("No root authority"))?,
  );
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
    let mut connection = connecting.context("Finalizing connection to server...")?;
    // Authenticate tunnel
    let authenticator = SimpleAckAuthenticationHandler::new();
    let fake_shutdown_trigger = triggered::trigger();
    ::snocat::common::authentication::perform_client_authentication(
      &authenticator,
      &mut connection,
      &fake_shutdown_trigger.1,
    )
    .await?;
    // Return successfully authenticated tunnel connection
    connection
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
  tracing::trace!("Loading metadata header");
  let header: MetaStreamHeader = read_framed_json(&mut recv).await?;

  tracing::debug!("Header received for connection {}: {:#?}", id, header);
  let (_header, await_connection): (MetaStreamHeader, _) = build_proxy_connection(header).await;
  tracing::debug!("Establishing proxy for connection {}...", id);
  let connection: TcpStream = await_connection.await?;

  tracing::trace!("Proxying...");
  util::proxy_from_tcp_stream(connection, (&mut send, &mut recv)).await?;
  tracing::info!("Closed connection {}", id);

  Ok(())
}
