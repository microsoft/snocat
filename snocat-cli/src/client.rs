// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use futures::{future::*, *};
use snocat::common::protocol::proxy_tcp::TcpStreamService;
use snocat::{
  common::protocol::traits::{InMemoryTunnelRegistry, ServiceRegistry, TunnelRegistry},
  common::protocol::tunnel::id::MonotonicAtomicGenerator,
  common::protocol::tunnel::BoxedTunnelPair,
  common::protocol::{Request, RouteAddress, Router, RoutingError, Service},
  common::tunnel_source::DynamicConnectionSet,
  common::{
    authentication::SimpleAckAuthenticationHandler,
    protocol::tunnel::{from_quinn_endpoint, Tunnel, TunnelSide},
  },
  server::modular::ModularDaemon,
  util,
  util::tunnel_stream::TunnelStream,
};
use std::sync::Weak;
use std::{path::PathBuf, sync::Arc};
use triggered::trigger;

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ClientArgs {
  pub authority_cert: PathBuf,
  pub driver_host: std::net::SocketAddr,
  pub driver_san: String,
  pub proxy_target_host: std::net::SocketAddr,
}

struct PresetServiceRegistry {
  pub services: Vec<Arc<dyn Service + Send + Sync>>,
}

impl PresetServiceRegistry {
  pub fn new() -> Self {
    Self {
      services: Vec::new(),
    }
  }
}

impl ServiceRegistry for PresetServiceRegistry {
  fn find_service(
    self: std::sync::Arc<Self>,
    addr: &RouteAddress,
  ) -> Option<std::sync::Arc<dyn Service + Send + Sync>> {
    self
      .services
      .iter()
      .find(|s| s.accepts(addr))
      .map(Arc::clone)
  }
}

pub struct SnocatClientRouter {
  typed_tunnel_registry: Weak<InMemoryTunnelRegistry>,
}

impl SnocatClientRouter {
  pub fn new(tunnel_registry: Weak<InMemoryTunnelRegistry>) -> Self {
    Self {
      typed_tunnel_registry: tunnel_registry,
    }
  }
}

impl Router for SnocatClientRouter {
  fn route(
    &self,
    request: &Request,
    // We don't need this one because we keep our own reference around for an unboxed variant
    // this allows us to access methods specific to our tunnel type's implementation
    _tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync>,
  ) -> BoxFuture<'_, Result<(RouteAddress, Box<dyn TunnelStream + Send + Sync>), RoutingError>> {
    let addr = request.address.clone();
    async move {
      let tunnel_registry = self
        .typed_tunnel_registry
        .upgrade()
        .ok_or(RoutingError::NoMatchingTunnel)?;
      // Select the highest keyed tunnel or bail; the highest tunnel is the newest connection, in our case
      let highest_keyed_tunnel_id = tunnel_registry
        .max_key()
        .await
        .ok_or(RoutingError::NoMatchingTunnel)?;
      // Find the tunnel if it's still around when we finish looking it up, or bail
      let tunnel = tunnel_registry
        .lookup_by_id(highest_keyed_tunnel_id)
        .await
        .ok_or(RoutingError::NoMatchingTunnel)?;
      let link = tunnel
        .tunnel
        .open_link()
        .await
        .map_err(RoutingError::LinkOpenFailure)?;
      let boxed_link: Box<dyn TunnelStream + Send + Sync + 'static> = Box::new(link);
      Ok((addr, boxed_link))
    }
    .boxed()
  }
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

  let (shutdown_listener, sigint_handler_task) = {
    let (shutdown_trigger, shutdown_listener) = trigger();
    let sigint_handler_task = tokio::task::spawn(async move {
      let _ = tokio::signal::ctrl_c().await;
      shutdown_trigger.trigger();
    });
    (shutdown_listener, sigint_handler_task)
  };

  let mut service_registry = PresetServiceRegistry::new();

  let tcp_proxy_service = TcpStreamService::new(false);
  service_registry.services.push(Arc::new(tcp_proxy_service));

  let tunnel_registry = Arc::new(InMemoryTunnelRegistry::new());

  let router = Arc::new(SnocatClientRouter::new(Arc::downgrade(&tunnel_registry)));

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

  let modular = Arc::new(ModularDaemon::new(
    Arc::new(service_registry),
    tunnel_registry,
    router,
    authentication_handler,
    tunnel_id_generator,
  ));

  let (endpoint, _incoming) = {
    let mut response_endpoint = quinn::Endpoint::builder();
    response_endpoint.default_client_config(quinn_config);
    response_endpoint.bind(&"[::]:0".parse()?)? // Should this be IPv4 if the server is?
  };

  let (mut add_new_connection, connections_handle) = {
    let mut current_connection_id = 0u32;
    let connections = DynamicConnectionSet::<u32>::new();
    let connections_handle = connections.handle();
    let add_new_connection = move |pair: BoxedTunnelPair<'static>| -> u32 {
      let connection_id = current_connection_id;
      current_connection_id += 1;
      connections
        .attach_stream(connection_id, stream::once(future::ready(pair)).boxed())
        .expect_none("Connection IDs must be unique");
      connection_id
    };
    (add_new_connection, connections_handle)
  };

  {
    let connecting: Result<_, _> = endpoint
      .connect(&config.driver_host, &config.driver_san)
      .context("Connecting to server")?
      .await;
    let connection = connecting.context("Finalizing connection to server...")?;
    let (tunnel, incoming) = from_quinn_endpoint(connection, TunnelSide::Connect);
    let addr = tunnel.addr();
    let conn_id = add_new_connection((Box::new(tunnel), incoming));
    tracing::info!(remote = ?addr, connection_id = conn_id, "connected");
  }

  tracing::debug!("Setting up stream handling...");
  modular
    .run(connections_handle.map(|(_k, v)| v), shutdown_listener)
    .await
    .expect("Modular runtime panicked and lost context");
  sigint_handler_task.abort();
  tracing::info!("Disconnecting...");
  Ok(())
}
