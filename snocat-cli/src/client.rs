// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use crate::services::{demand_proxy::DemandProxyClient, PresetServiceRegistry};
use anyhow::{Context as AnyhowContext, Result};
use futures::{future::*, *};
use snocat::{
  common::{
    authentication::{AuthenticationAttributes, SimpleAckAuthenticationHandler},
    daemon::{
      ModularDaemon, PeerTracker, PeersView, RecordConstructor, RecordConstructorArgs,
      RecordConstructorResult,
    },
    protocol::{
      negotiation::NegotiationClient,
      proxy_tcp::{DnsTarget, TcpStreamService},
      service::{Client, Request, Router, RouterResult, RoutingError},
      tunnel::{
        id::MonotonicAtomicGenerator, registry::memory::InMemoryTunnelRegistry, TunnelId,
        TunnelName, TunnelSide, TunnelUplink,
      },
    },
    tunnel_source::DynamicConnectionSet,
  },
  util::tunnel_stream::WrappedStream,
};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ClientArgs {
  pub authority_cert: Option<PathBuf>,
  pub driver_host: std::net::SocketAddr,
  pub driver_san: String,
  pub proxy_target_host: std::net::SocketAddr,
}

pub struct SnocatClientRouter {
  peers: Arc<PeersView>,
}

impl SnocatClientRouter {
  pub fn new(peers: PeersView) -> Self {
    Self {
      peers: peers.into(),
    }
  }
}

#[derive(thiserror::Error, Debug)]
pub enum RouterError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientLocalAddress {
  MostRecentConnection,
}

impl Router for SnocatClientRouter {
  type Error = RouterError;
  type Stream = WrappedStream;
  type LocalAddress = ClientLocalAddress;

  fn route<'client, 'result, TProtocolClient, IntoLocalAddress: Into<Self::LocalAddress>>(
    &self,
    request: Request<'client, Self::Stream, TProtocolClient>,
    local_address: IntoLocalAddress,
  ) -> BoxFuture<'client, RouterResult<'client, 'result, Self, TProtocolClient>>
  where
    TProtocolClient: Client<'result, Self::Stream> + Send + 'client,
  {
    // Assert that if another local address type is added, we handle it appropriately
    // if we don't provide a match for it, we can't compile at all, let alone fail
    match local_address.into() {
      ClientLocalAddress::MostRecentConnection => (),
    };
    let addr = request.address.clone();
    let err_addr = request.address.clone();
    let err_not_found = move || RoutingError::RouteNotFound(err_addr.clone());
    let peers = self.peers.clone();
    async move {
      // Select the highest keyed tunnel or bail; the highest tunnel is the newest connection, in our case
      let record = peers
        .find_by_comparator(|record| record.id)
        .ok_or_else(err_not_found.clone())?;
      let link = record
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

pub async fn client_main(config: ClientArgs) -> Result<()> {
  let config = Arc::new(config);
  let mut root_authorities = rustls::RootCertStore::empty();
  if let Some(authority_cert_path) = &config.authority_cert {
    let authority_cert_pem =
      std::fs::read(authority_cert_path).context("Failed reading authority cert file")?;
    let authority_cert_chain =
      rustls_pemfile::certs(&mut std::io::Cursor::new(&authority_cert_pem))
        .context("Quinn .pem parsing of authority certificate(s) failed")?;

    // Register all authority certificates with Rustls' root store
    for authority in authority_cert_chain {
      root_authorities
        .add(&rustls::Certificate(authority))
        .context("Failed to add root authority")?;
    }
  };

  let native_root_certs =
    rustls_native_certs::load_native_certs().context("Failed loading native certs")?;
  root_authorities.add_parsable_certificates(
    &native_root_certs
      .into_iter()
      .map(|c| c.0)
      .collect::<Vec<_>>(),
  );
  let mut crypto_config = rustls::ClientConfig::builder()
    .with_safe_default_cipher_suites()
    .with_safe_default_kx_groups()
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(root_authorities)
    .with_no_client_auth();

  crypto_config.alpn_protocols = vec![crate::util::ALPN_MS_SNOCAT_1.to_vec()];
  crypto_config.key_log = Arc::new(rustls::KeyLogFile::new());

  let quinn_config = quinn::ClientConfig::new(Arc::new(crypto_config));

  let (shutdown, sigint_handler_task) = {
    let shutdown = CancellationToken::new();
    let shutdown_trigger = shutdown.clone();
    let sigint_handler_task = tokio::task::spawn(async move {
      let _ = tokio::signal::ctrl_c().await;
      shutdown_trigger.cancel();
    });
    (shutdown, sigint_handler_task)
  };

  let proxy_target = config.proxy_target_host.clone();

  let service_registry = Arc::new(PresetServiceRegistry::<anyhow::Error>::new());

  let tcp_proxy_service = TcpStreamService::new(false);
  service_registry.add_service_blocking(Arc::new(tcp_proxy_service));

  let tunnel_registry = Arc::new(InMemoryTunnelRegistry::<(
    TunnelId,
    TunnelName,
    Arc<AuthenticationAttributes>,
  )>::new());

  let peer_tracker = PeerTracker::default();
  let router = Arc::new(SnocatClientRouter::new(peer_tracker.view()));

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
    service_registry,
    tunnel_registry,
    peer_tracker,
    router,
    authentication_handler,
    tunnel_id_generator,
    record_constructor,
  ));

  let endpoint = quinn::Endpoint::client("[::]:0".parse()?)?; // Should this be IPv4 if the server is?

  let (mut add_new_connection, connections_handle) = {
    let mut current_connection_id = 0u32;
    let connections = DynamicConnectionSet::<u32, _>::new();
    let connections_handle = connections.handle();
    let add_new_connection = move |tunnel: (quinn::Connection, _)| -> u32 {
      let connection_id = current_connection_id;
      current_connection_id += 1;
      assert!(
        connections
          .attach_stream(connection_id, stream::once(future::ready(tunnel)).boxed())
          .is_none(),
        "Connection IDs must be unique"
      );
      connection_id
    };
    (add_new_connection, connections_handle)
  };

  {
    let connection = endpoint
      .connect_with(quinn_config, config.driver_host, &config.driver_san)
      .context("Connecting to server")?
      .await
      .context("Finalizing connection to server...")?;
    let addr = connection.remote_address();
    let conn_id = add_new_connection((connection, TunnelSide::Connect));
    tracing::info!(remote = ?addr, connection_id = conn_id, "connected");
  }

  tracing::debug!("Setting up stream handling...");
  let request_handler = Arc::clone(modular.router());

  let connections = modular.construct_tunnels(connections_handle.map(|(_k, v)| v));
  let daemon = modular
    .run(connections, shutdown.clone().into())
    .map_err(|_| anyhow::Error::msg("Daemon panicked and lost context"))
    .boxed();

  let tcp_watcher = {
    let shutdown = shutdown;
    tokio::task::spawn(async move {
      // TODO: Wait until a connection is completed before requesting a proxy
      // TODO: While shutdown is not requested, attempt to reconnect every second
      tokio::time::sleep(std::time::Duration::from_secs(5)).await;
      let demand_proxy = DemandProxyClient::new("".into());
      let req = Request::new(
        demand_proxy,
        DnsTarget::Dns4 {
          host: proxy_target.ip().to_string(),
          port: proxy_target.port(),
        }
        .into(),
      )?;
      let (_remote_addrs, wait_close) = request_handler
        .route(req, ClientLocalAddress::MostRecentConnection)
        .await?
        .await?;
      let res = futures::future::select(wait_close, Box::pin(shutdown.cancelled())).await;
      match res {
        Either::Left((Err(res), _listener)) => Err(res)?,
        Either::Left((Ok(()), _listener)) => (),
        Either::Right(((), _finish_listener)) => (),
      };
      Result::<(), anyhow::Error>::Ok(())
    })
    .boxed()
    // JoinHandle
    .map_err(|join_error| {
      if join_error.is_panic() {
        anyhow::Error::msg("TCP Watcher panicked and lost context")
      } else {
        anyhow::Error::context(
          anyhow::Error::msg(join_error.to_string()),
          "TCP Watcher join handle failed",
        )
      }
    })
    // Flatten Result<Result<T, E>, E> to Result<T, E>; this is why we need HKT, Rust!
    .map(|result| match result {
      Ok(Ok(ok)) => Ok(ok),
      Ok(Err(err)) => Err(err),
      Err(err) => Err(err),
    })
    .boxed()
  };

  let ((), ()) = futures::future::try_join(daemon, tcp_watcher).await?;

  sigint_handler_task.abort();
  tracing::info!("Disconnecting...");
  Ok(())
}
