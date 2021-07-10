// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use crate::{
  services::{demand_proxy::DemandProxyService, PresetServiceRegistry},
  util,
};
use anyhow::{Context as AnyhowContext, Result};
use futures::future::{BoxFuture, FutureExt, TryFutureExt};
use quinn::TransportConfig;
use snocat::{
  common::{
    authentication::SimpleAckAuthenticationHandler,
    protocol::{
      tunnel::{
        id::MonotonicAtomicGenerator,
        registry::{local::InMemoryTunnelRegistry, TunnelRegistry},
        QuinnTunnel, Tunnel,
      },
      Request, RouteAddress, Router, RoutingError,
    },
    tunnel_source::QuinnListenEndpoint,
  },
  server::{modular::ModularDaemon, PortRangeAllocator},
  util::tunnel_stream::TunnelStream,
};
use std::{
  boxed::Box,
  net::{IpAddr, Ipv4Addr, Ipv6Addr},
  path::PathBuf,
  sync::{Arc, Weak},
};
use tokio_util::sync::CancellationToken;

/// Parameters used to run an Snocat server binding TCP connections
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ServerArgs {
  pub cert: PathBuf,
  pub key: PathBuf,
  pub quinn_bind_addr: std::net::SocketAddr,
  pub tcp_bind_ip: std::net::IpAddr,
  pub tcp_bind_port_range: std::ops::RangeInclusive<u16>,
}

pub struct SnocatServerRouter<TTunnel> {
  tunnel_phantom: std::marker::PhantomData<TTunnel>,
}

impl<TTunnel> SnocatServerRouter<TTunnel> {
  pub fn new() -> Self {
    Self {
      tunnel_phantom: std::marker::PhantomData,
    }
  }
}
impl<TTunnel> Router<TTunnel, InMemoryTunnelRegistry<TTunnel>> for SnocatServerRouter<TTunnel>
where
  TTunnel: Tunnel + Send + Sync + 'static,
{
  fn route(
    &self,
    request: &Request,
    // We don't need this one because we keep our own reference around for an unboxed variant
    // this allows us to access methods specific to our tunnel type's implementation
    tunnel_registry: Arc<InMemoryTunnelRegistry<TTunnel>>,
  ) -> BoxFuture<
    '_,
    Result<
      (RouteAddress, Box<dyn TunnelStream + Send + Sync>),
      RoutingError<<InMemoryTunnelRegistry<TTunnel> as TunnelRegistry<TTunnel>>::Error>,
    >,
  > {
    let addr = request.address.clone();
    async move {
      // Select the highest keyed tunnel or bail; the highest tunnel is the newest connection, in our case
      let highest_keyed_tunnel_id = tunnel_registry
        .max_key()
        .await
        .ok_or(RoutingError::NoMatchingTunnel)?;
      // Find the tunnel if it's still around when we finish looking it up, or bail
      let tunnel = tunnel_registry
        .lookup_by_id(highest_keyed_tunnel_id)
        .await
        .map_err(Into::into)
        .and_then(|t| t.ok_or(RoutingError::NoMatchingTunnel))?;
      // .ok_or(RoutingError::NoMatchingTunnel)?;
      let tunnel_id = tunnel.id;
      let link = tunnel
        .tunnel
        .ok_or_else(|| {
          tracing::warn!(
            ?tunnel_id,
            "Attempted to route to tunnel not available in the local registry"
          );
          RoutingError::NoMatchingTunnel
        })?
        .open_link()
        .await
        .map_err(RoutingError::LinkOpenFailure)?;
      let boxed_link: Box<dyn TunnelStream + Send + Sync + 'static> = Box::new(link);
      Ok((addr, boxed_link))
    }
    .boxed()
  }
}

/// Run a Snocat server that binds TCP sockets for each tunnel that connects
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
  let endpoint = QuinnListenEndpoint::bind(config.quinn_bind_addr, quinn_config)?;

  let (shutdown, sigint_handler_task) = {
    let shutdown = CancellationToken::new();
    let shutdown_trigger = shutdown.clone();
    let sigint_handler_task = tokio::task::spawn(async move {
      let _ = tokio::signal::ctrl_c().await;
      tracing::trace!("SIGINT detected, initiating graceful shutdown");
      shutdown_trigger.cancel();
    });
    (shutdown, sigint_handler_task)
  };

  let tunnel_registry = Arc::new(InMemoryTunnelRegistry::new());

  let service_registry = Arc::new(PresetServiceRegistry::<anyhow::Error>::new());

  let router = { Arc::new(SnocatServerRouter::new()) };

  let authentication_handler = Arc::new(SimpleAckAuthenticationHandler::new());

  // Our tunnel IDs are just increments atop the unix timestamp millisecond we started the server
  // This would still likely lead to eventual collisions in a shared-ID cluster, so don't do that
  // The same goes for a local filesystem, if you were to create tunnels fast enough.
  let tunnel_id_generator = Arc::new(MonotonicAtomicGenerator::new(
    std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .expect("Must be a time since the unix epoch")
      .as_millis() as u64,
  ));

  let modular = Arc::new(ModularDaemon::<Arc<QuinnTunnel<_>>, _, _, _, _>::new(
    service_registry.clone(),
    tunnel_registry.clone(),
    router,
    authentication_handler,
    tunnel_id_generator,
  ));

  {
    let demand_proxy_service = Arc::new(DemandProxyService::new(
      Arc::downgrade(&tunnel_registry) as Weak<_>, // `as` clause triggers CoerceUnsize to make a dynamic Arc
      Arc::downgrade(modular.requests()),
      PortRangeAllocator::new(config.tcp_bind_port_range),
      vec![
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
      ],
    ));
    service_registry.add_service_blocking(demand_proxy_service);
    drop(service_registry);
  }

  modular
    .run(endpoint, shutdown)
    .map_err(|_| anyhow::Error::msg("Modular runtime panicked and lost context"))
    .await?;

  sigint_handler_task.abort();
  let _cancelled = sigint_handler_task.await;

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
  transport_config.receive_window(512 * 1024 * 1024)?;
  transport_config.send_window(512 * 1024 * 1024);
  transport_config.stream_receive_window(512 * 1024 * 1024 / 8)?;
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
