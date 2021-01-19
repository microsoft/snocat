use crate::util::{
  self,
  validators::{parse_ipaddr, parse_port_range, parse_socketaddr},
};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use async_std::sync::{Arc, Mutex};
use futures::future::*;
use futures::{
  future,
  future::FutureExt,
  pin_mut, select_biased,
  stream::{self, Stream, StreamExt},
};
use gen_z::gen_z as generate_stream;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use snocat::common::{authentication::SimpleAckAuthenticationHandler, MetaStreamHeader};
use snocat::server::{
  deferred::{
    ConcurrentDeferredTunnelServer, SnocatClientIdentifier, TunnelManager, TunnelServerEvent,
  },
  TcpTunnelManager,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  boxed::Box,
  path::{Path, PathBuf},
  pin::Pin,
  task::{Context, Poll},
};
use tracing::{info, instrument, trace};

/// Parameters used to run an Snocat server binding TCP connections
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ServerArgs {
  pub cert: PathBuf,
  pub key: PathBuf,
  pub quinn_bind_addr: std::net::SocketAddr,
  pub tcp_bind_ip: std::net::IpAddr,
  pub tcp_bind_port_range: std::ops::RangeInclusive<u16>,
}

/// Run an Snocat server that binds TCP sockets for each tunnel that connects
// TODO: move to snocat-cli
#[tracing::instrument(
skip(config),
fields(
addr=?config.tcp_bind_ip,
ports=?config.tcp_bind_port_range,
quinn=?config.quinn_bind_addr,
),
err
)]
pub async fn server_main(config: self::ServerArgs) -> Result<()> {
  let quinn_config = build_quinn_config(&config)?;
  let (_endpoint, incoming) = {
    let mut endpoint = quinn::Endpoint::builder();
    endpoint.listen(quinn_config);
    endpoint.bind(&config.quinn_bind_addr)?
  };

  let manager = TcpTunnelManager::new(
    config.tcp_bind_port_range,
    config.tcp_bind_ip,
    Box::new(SimpleAckAuthenticationHandler::new()),
  );
  let server = Box::new(ConcurrentDeferredTunnelServer::new(manager));

  use futures::stream::TryStreamExt;
  let (trigger_shutdown, shutdown_notifier) = triggered::trigger();
  let connections: stream::BoxStream<'_, quinn::NewConnection> = incoming
    .take_until(shutdown_notifier.clone())
    .map(|x| -> Result<_> { Ok(x) })
    .and_then(async move |connecting| {
      // When a new connection arrives, establish the connection formally, and pass it on
      let tunnel = connecting.await?; // Performs TLS handshake and migration
                                      // TODO: Protocol header can occur here, or as part of the later "binding" phase
                                      // It can also be built as an isomorphic middleware intercepting a TryStream of NewConnection
      Ok(tunnel)
    })
    .inspect_err(|e| {
      tracing::error!("Connection failure during stream pickup: {:#?}", e);
    })
    .filter_map(async move |x| x.ok()) // only keep the successful connections
    .boxed();

  let events = server
    .handle_incoming(connections, shutdown_notifier.clone())
    .fuse();
  {
    let signal_watcher = tokio::signal::ctrl_c()
      .into_stream()
      .take(1)
      .map(|_| {
        tracing::warn!("Shutdown triggered");
        // Tell manager to start shutting down tunnels; new adoption requests should return errors
        trigger_shutdown.trigger();
        None
      })
      .fuse();
    futures::stream::select(signal_watcher, events.map(|e| Some(e)))
      .filter_map(|x| future::ready(x))
      .for_each(async move |ev| {
        tracing::trace!(event = ?ev);
      })
      .await;
  }

  Ok(())
}

fn build_quinn_config(config: &ServerArgs) -> Result<quinn::ServerConfig> {
  let cert_pem = std::fs::read(&config.cert).context("Failed reading cert file")?;
  let priv_pem = std::fs::read(&config.key).context("Failed reading private key file")?;
  let priv_key =
    quinn::PrivateKey::from_pem(&priv_pem).context("Quinn .pem parsing of private key failed")?;
  let mut config = quinn::ServerConfigBuilder::default();
  config.use_stateless_retry(true);
  let mut transport_config = TransportConfig::default();
  transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
  transport_config.receive_window(512 * 1024 * 1024);
  transport_config.send_window(512 * 1024 * 1024);
  transport_config.stream_receive_window(512 * 1024 * 1024 / 8);
  transport_config
    .max_idle_timeout(Some(std::time::Duration::from_secs(30)))
    .unwrap();
  let mut server_config = quinn::ServerConfig::default();
  server_config.transport = Arc::new(transport_config);
  server_config.migration(true);
  let mut cfg_builder = quinn::ServerConfigBuilder::new(server_config);
  cfg_builder.protocols(util::ALPN_QUIC_HTTP);
  cfg_builder.enable_keylog();
  let cert_chain = quinn::CertificateChain::from_pem(&cert_pem)?;
  cfg_builder.certificate(cert_chain, priv_key)?;
  Ok(cfg_builder.build())
}
