// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use crate::services::{demand_proxy::DemandProxyService, PresetServiceRegistry};
use anyhow::{Context as AnyhowContext, Result};
use futures::{
  future::{BoxFuture, FutureExt, TryFutureExt},
  StreamExt,
};
use quinn::{TransportConfig, VarInt};
use snocat::{
  common::{
    authentication::{AuthenticationAttributes, SimpleAckAuthenticationHandler},
    daemon::{
      ModularDaemon, PeerTracker, PeersView, RecordConstructor, RecordConstructorArgs,
      RecordConstructorResult,
    },
    protocol::{
      negotiation::NegotiationClient,
      service::{Client, Request, Router, RouterResult, RoutingError},
      tunnel::{
        id::MonotonicAtomicGenerator, registry::memory::InMemoryTunnelRegistry, TunnelId,
        TunnelName,
      },
    },
    tunnel_source::QuinnListenEndpoint,
  },
  server::PortRangeAllocator,
  util::tunnel_stream::WrappedStream,
};
use std::{
  convert::TryInto,
  net::{IpAddr, Ipv4Addr, Ipv6Addr},
  path::PathBuf,
  sync::Arc,
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

pub struct SnocatServerRouter {
  active_tunnels: Arc<PeersView>,
}

impl SnocatServerRouter {
  pub fn new(active_tunnels: PeersView) -> Self {
    Self {
      active_tunnels: active_tunnels.into(),
    }
  }
}

#[derive(thiserror::Error, Debug)]
pub enum RouterError {}

impl Router for SnocatServerRouter {
  type Error = RouterError;
  type Stream = WrappedStream;
  type LocalAddress = TunnelName;

  fn route<'client, 'result, TProtocolClient, IntoLocalAddress: Into<Self::LocalAddress>>(
    &self,
    request: Request<'client, Self::Stream, TProtocolClient>,
    local_address: IntoLocalAddress,
  ) -> BoxFuture<'client, RouterResult<'client, 'result, Self, TProtocolClient>>
  where
    TProtocolClient: Client<'result, Self::Stream> + Send + 'client,
  {
    let addr = request.address.clone();
    let local_address = local_address.into();
    let active_tunnels = self.active_tunnels.clone();
    let err_addr = request.address.clone();
    let err_not_found = move || RoutingError::RouteNotFound(err_addr.clone());
    async move {
      // Lookup the tunnel or bail if it doesn't exist anymore
      let dest_name: TunnelName = local_address.into();
      let tunnel = active_tunnels
        .find_by_name_and_comparator(&dest_name, |r| r.registered_at.0)
        .ok_or_else(err_not_found.clone())?;
      let link = tunnel
        .tunnel
        .open_link()
        .await
        .map_err(RoutingError::LinkOpenFailure)?;
      let negotiator = NegotiationClient::new();
      let link = negotiator
        .negotiate::<_, Self::Error>(addr.clone(), link)
        .await?;
      Ok(request.protocol_client.handle(addr, link))
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
  let endpoint = QuinnListenEndpoint::bind(config.quinn_bind_addr, quinn_config)?.filter_map(
    |(connecting, side)| {
      connecting.map(move |res| res.ok().map(move |connection| (connection, side)))
    },
  );

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

  let tunnel_registry = Arc::new(InMemoryTunnelRegistry::<(
    TunnelId,
    TunnelName,
    Arc<AuthenticationAttributes>,
  )>::new());

  let service_registry = Arc::new(PresetServiceRegistry::<anyhow::Error>::new());

  let peer_tracker = PeerTracker::default();
  let router = { Arc::new(SnocatServerRouter::new(peer_tracker.view())) };

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

  let record_constructor = Arc::new(
    |args: RecordConstructorArgs| -> RecordConstructorResult<_, _> {
      let attrs = Arc::new(args.attributes);
      futures::future::ready(Ok(((args.id, args.name, attrs.clone()), attrs))).boxed()
    },
  ) as Arc<dyn RecordConstructor<_, _>>;
  let modular = Arc::new(ModularDaemon::new(
    service_registry.clone(),
    tunnel_registry.clone(),
    peer_tracker.clone(),
    router,
    authentication_handler,
    tunnel_id_generator,
    record_constructor,
  ));

  {
    let demand_proxy_service = Arc::new(DemandProxyService::new(
      peer_tracker.view(),
      Arc::downgrade(modular.router()),
      PortRangeAllocator::new(config.tcp_bind_port_range),
      vec![
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
      ],
    ));
    service_registry.add_service_blocking(demand_proxy_service);
    drop(service_registry);
  }

  let endpoint = modular.construct_tunnels(endpoint);
  modular
    .run(endpoint, shutdown.into())
    .map_err(|_| anyhow::Error::msg("Modular runtime panicked and lost context"))
    .await?;

  sigint_handler_task.abort();
  let _cancelled = sigint_handler_task.await;

  Ok(())
}

fn build_quinn_config(config: &ServerArgs) -> Result<quinn::ServerConfig> {
  let cert_pem = std::fs::read(&config.cert).context("Failed reading cert file")?;
  let priv_pem = std::fs::read(&config.key).context("Failed reading private key file")?;
  let priv_key = {
    let priv_key = rustls_pemfile::pkcs8_private_keys(&mut std::io::Cursor::new(&priv_pem))
      .context("Quinn .pem parsing of private key failed")?;

    // TODO: We check at least one private key; check at most one as well
    let priv_key = priv_key
      .into_iter()
      .next()
      .context("Quinn private key .pem must contain exactly one private key")?;

    // Encapsulate the parsed key into rustls's wrapper type
    rustls::PrivateKey(priv_key)
  };
  let cert_chain: Vec<rustls::Certificate> = {
    let cert_chain = rustls_pemfile::certs(&mut std::io::Cursor::new(&cert_pem))
      .context("Quinn .pem parsing of certificates failed")?;

    // Map all certificates in the chain to rustls's wrapper type and construct a vector
    cert_chain.into_iter().map(rustls::Certificate).collect()
  };
  let mut crypto_config = rustls::ServerConfig::builder()
    .with_safe_default_cipher_suites()
    .with_safe_default_kx_groups()
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(cert_chain, priv_key)?;
  crypto_config.alpn_protocols = vec![crate::util::ALPN_MS_SNOCAT_1.to_vec()];
  crypto_config.key_log = Arc::new(rustls::KeyLogFile::new());
  let mut transport_config = TransportConfig::default();
  transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
  transport_config.receive_window(VarInt::from_u32(512 * 1024 * 1024));
  transport_config.send_window(512 * 1024 * 1024);
  transport_config.stream_receive_window(VarInt::from_u32(512 * 1024 * 1024 / 8));
  transport_config.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into().unwrap()));
  let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto_config));
  server_config.use_retry(true);
  server_config.transport = Arc::new(transport_config);
  server_config.migration(true);
  Ok(server_config)
}
