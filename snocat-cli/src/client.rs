// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use crate::services::{demand_proxy::DemandProxyClient, PresetServiceRegistry};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use futures::{future::*, *};
use snocat::{
  common::{
    authentication::SimpleAckAuthenticationHandler,
    protocol::{
      proxy_tcp::TcpStreamService,
      traits::{InMemoryTunnelRegistry, TunnelRegistry},
      tunnel::{
        from_quinn_endpoint, id::MonotonicAtomicGenerator, QuinnTunnel, TunnelSide, TunnelUplink,
      },
      Request, RouteAddress, Router, RoutingError,
    },
    tunnel_source::DynamicConnectionSet,
  },
  server::modular::ModularDaemon,
  util::{self, tunnel_stream::TunnelStream},
};
use std::{
  path::PathBuf,
  sync::{Arc, Weak},
};
use triggered::trigger;

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ClientArgs {
  pub authority_cert: Option<PathBuf>,
  pub driver_host: std::net::SocketAddr,
  pub driver_san: String,
  pub proxy_target_host: std::net::SocketAddr,
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
  let authority = match &config.authority_cert {
    Some(authority_cert_path) => {
      let cert_pem =
        std::fs::read(authority_cert_path).context("Failed reading authority cert file")?;
      let authority = quinn::CertificateChain::from_pem(&cert_pem)?;
      let authority = quinn::Certificate::from(
        authority
          .iter()
          .nth(0)
          .cloned()
          .ok_or_else(|| AnyErr::msg("No root authority"))?,
      );
      Some(authority)
    }
    None => None,
  };
  let quinn_config = {
    let mut qc = quinn::ClientConfigBuilder::default();
    qc.enable_keylog();
    if let Some(authority) = authority {
      qc.add_certificate_authority(authority)?;
    }
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

  let proxy_target = config.proxy_target_host.clone();

  let service_registry = Arc::new(PresetServiceRegistry::new());

  let tcp_proxy_service = TcpStreamService::new(false);
  service_registry.add_service_blocking(Arc::new(tcp_proxy_service));

  let tunnel_registry: Arc<InMemoryTunnelRegistry> = Arc::new(InMemoryTunnelRegistry::new());

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

  let modular = Arc::new(ModularDaemon::<QuinnTunnel<_>>::new(
    service_registry,
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
    let connections = DynamicConnectionSet::<u32, _>::new();
    let connections_handle = connections.handle();
    let add_new_connection = move |tunnel: QuinnTunnel<_>| -> u32 {
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
    let connecting: Result<_, _> = endpoint
      .connect(&config.driver_host, &config.driver_san)
      .context("Connecting to server")?
      .await;
    let connection = connecting.context("Finalizing connection to server...")?;
    let tunnel = from_quinn_endpoint(connection, TunnelSide::Connect);
    let addr = tunnel.addr();
    let conn_id = add_new_connection(tunnel);
    tracing::info!(remote = ?addr, connection_id = conn_id, "connected");
  }

  tracing::debug!("Setting up stream handling...");
  let request_handler = Arc::clone(modular.requests());

  let daemon = modular
    .run(
      connections_handle.map(|(_k, v)| v),
      shutdown_listener.clone(),
    )
    .map_err(|_| anyhow::Error::msg("Daemon panicked and lost context"))
    .boxed();

  let tcp_watcher = {
    let shutdown_listener = shutdown_listener;
    tokio::task::spawn(async move {
      // TODO: Wait until a connection is completed before requesting a proxy
      // TODO: While shutdown is not requested, attempt to reconnect every second
      tokio::time::sleep(std::time::Duration::from_secs(5)).await;
      let demand_proxy = DemandProxyClient {
        proxied_subject: "".into(),
      };
      let (_remote_addr, wait_close) = request_handler
        .handle(
          format!(
            "/proxyme/0.0.1/{}/{}",
            proxy_target.ip().to_string(),
            proxy_target.port()
          ),
          demand_proxy,
        )
        .await?;
      let res = futures::future::select(wait_close, shutdown_listener).await;
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
