use authentication::perform_authentication;
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use futures::{
  future::{self, BoxFuture, FutureExt, TryFutureExt},
  stream, Future, Stream, StreamExt, TryStreamExt,
};
use std::{any::Any, sync::Arc};
use tracing::Instrument;
use triggered::Listener;
use tunnel::{TunnelError, TunnelName};

use crate::common::{
  authentication::{self, AuthenticationHandler},
  protocol::{
    negotiation::{self, NegotiationError, NegotiationService},
    request_handler::{RequestClientHandler, RequestHandlingError},
    traits::{ServiceRegistry, TunnelNamingError, TunnelRegistrationError, TunnelRegistry},
    tunnel::{
      self, id::TunnelIDGenerator, ArcTunnel, ArcTunnelPair, BoxedTunnelPair, TunnelId,
      TunnelIncomingType,
    },
    Client, Request, Response, RouteAddress, Router, RoutingError, ServiceError,
  },
};
use crate::common::{
  authentication::{AuthenticationError, AuthenticationHandlingError, RemoteAuthenticationError},
  protocol::traits::SerializedTunnelRegistry,
};
use crate::{
  common::protocol::ClientError,
  util::tunnel_stream::{TunnelStream, WrappedStream},
};

pub struct ModularDaemon {
  service_registry: Arc<dyn ServiceRegistry + Send + Sync + 'static>,
  tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
  router: Arc<dyn Router + Send + Sync + 'static>,
  request_handler: Arc<RequestClientHandler>,
  authentication_handler: Arc<dyn AuthenticationHandler + Send + Sync + 'static>,
  tunnel_id_generator: Arc<dyn TunnelIDGenerator + Send + Sync + 'static>,
}

impl ModularDaemon {
  pub fn requests<'a>(&'a self) -> &Arc<RequestClientHandler> {
    &self.request_handler
  }

  fn authenticate_tunnel<'a>(
    self: &Arc<Self>,
    (tunnel, mut incoming): tunnel::ArcTunnelPair<'a>,
    shutdown: &Listener,
  ) -> impl Future<
    Output = Result<Option<(tunnel::TunnelName, tunnel::ArcTunnelPair<'a>)>, anyhow::Error>,
  > + 'a {
    let shutdown = shutdown.clone();
    let authentication_handler = Arc::clone(&self.authentication_handler);

    async move {
      let result = perform_authentication(
        authentication_handler.as_ref(),
        tunnel.as_ref(),
        &mut incoming,
        &shutdown,
      )
      .await;
      match result {
        Err(AuthenticationError::Handling(AuthenticationHandlingError::FatalApplicationError(
          fatal_error,
        ))) => {
          tracing::error!(reason=?fatal_error, "Authentication encountered fatal error!");
          anyhow::Context::context(
            Err(fatal_error),
            "Fatal error encountered while handling authentication",
          )
        }
        Err(AuthenticationError::Handling(handling_error)) => {
          // Non-fatal handling errors are passed to tracing and close the tunnel
          tracing::warn!(
            reason = (&handling_error as &dyn std::error::Error),
            "Tunnel closed due to authentication handling failure"
          );
          Ok(None)
        }
        Err(AuthenticationError::Remote(remote_error)) => {
          tracing::debug!(
            reason = (&remote_error as &dyn std::error::Error),
            "Tunnel closed due to remote authentication failure"
          );
          Ok(None)
        }
        Ok(tunnel_name) => Ok(Some((tunnel_name, (tunnel, incoming)))),
      }
    }
  }
}

impl ModularDaemon
where
  Self: 'static,
{
  pub fn new(
    service_registry: Arc<dyn ServiceRegistry + Send + Sync + 'static>,
    tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
    router: Arc<dyn Router + Send + Sync + 'static>,
    authentication_handler: Arc<dyn AuthenticationHandler + Send + Sync + 'static>,
    tunnel_id_generator: Arc<dyn TunnelIDGenerator + Send + Sync + 'static>,
  ) -> Self {
    Self {
      request_handler: Arc::new(RequestClientHandler::new(
        Arc::clone(&tunnel_registry),
        Arc::clone(&service_registry),
        Arc::clone(&router),
      )),
      service_registry,
      tunnel_registry,
      router,
      authentication_handler,
      tunnel_id_generator,
    }
  }

  /// Run the server against a tunnel_source.
  ///
  /// This can be performed concurrently against multiple sources, with a shared server instance.
  /// The implementation assumes that shutdown_request_listener will also halt the tunnel_source.
  pub fn run<TunnelSource>(
    self: Arc<Self>,
    tunnel_source: TunnelSource,
    shutdown_request_listener: Listener,
  ) -> tokio::task::JoinHandle<()>
  where
    TunnelSource: Stream<Item = BoxedTunnelPair<'static>> + Send + 'static,
  {
    let this = Arc::clone(&self);
    // Pipeline phases:
    // Attach baggage - Arcs need cloned once per incoming tunnel, if they need to access it
    // The baggage attachment phase takes the initial Arc items clones them per-stream
    // This also generates a u64 as an ID for this tunnel, using a naive interlocked/atomic counter
    let pipeline = tunnel_source
      .take_until(shutdown_request_listener.clone())
      .scan(
        (this, shutdown_request_listener),
        |(this, shutdown_request_listener), tunnel_pair| {
          let id = this.tunnel_id_generator.next();
          let tunnel_pair: ArcTunnelPair = (tunnel_pair.0.into(), tunnel_pair.1);
          future::ready(Some((
            tunnel_pair,
            id,
            this.clone(),
            shutdown_request_listener.clone(),
          )))
        },
      );

    // Tunnel Lifecycle - Sub-pipeline performed by futures on a per-tunnel basis
    // This could be done at the stream level, but Rust-Analyzer's typesystem struggles
    // to understand stream associated types at this level.
    let pipeline = pipeline.for_each_concurrent(
      None,
      |(tunnel_pair, id, server, shutdown_request_listener)| async move {
        if let Err(e) =
          Self::tunnel_lifecycle(id, tunnel_pair, server, shutdown_request_listener).await
        {
          tracing::debug!(error=?e, "tunnel lifetime exited with error");
        }
      },
    );

    // Spawn an instrumented task for the server which will return
    // when all connections shut down and the tunnel source closes
    tokio::task::spawn(pipeline.instrument(tracing::span!(tracing::Level::INFO, "modular_server")))
  }
}

#[derive(thiserror::Error, Debug)]
enum TunnelLifecycleError {
  #[error(transparent)]
  RegistrationError(#[from] TunnelRegistrationError),
  #[error(transparent)]
  RegistryNamingError(#[from] TunnelNamingError),
  #[error(transparent)]
  RequestProcessingError(#[from] RequestProcessingError),
  #[error("Authentication refused to remote by either breach of protocol or invalid/inadequate credentials")]
  AuthenticationRefused,
  #[error("Fatal error encountered in tunnel lifecycle: {0:?}")]
  FatalError(anyhow::Error),
}

#[derive(thiserror::Error, Debug)]
enum RequestProcessingError {
  #[error("Protocol version mismatch")]
  UnsupportedProtocolVersion,
  #[error("Tunnel error encountered: {0}")]
  TunnelError(TunnelError),
}

impl ModularDaemon
where
  Self: 'static,
{
  fn tunnel_lifecycle(
    id: TunnelId,
    (tunnel, incoming): ArcTunnelPair<'static>,
    server: Arc<ModularDaemon>,
    shutdown: Listener,
  ) -> impl Future<Output = Result<(), TunnelLifecycleError>> + 'static {
    async move {
      // A registry mutex that prevents us from racing when calling the registry for
      // this particular tunnel entry. This should also be enforced at the registry level.
      let serialized_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static> = Arc::new(SerializedTunnelRegistry::new(Arc::clone(&server.tunnel_registry)));

      // Tunnel registration - The tunnel registry is called to imbue the tunnel with an ID
      {
        let tunnel = Arc::clone(&tunnel);
        let tunnel_registry = Arc::clone(&serialized_registry);
        Self::register_tunnel(id, tunnel, tunnel_registry)
          .instrument(tracing::span!(tracing::Level::DEBUG, "registration", ?id))
      }.await?;

      // From here on, any failure must trigger attempted deregistration of the tunnel,
      // So further phases return their result to check for failures, which then result
      // in a deregistration call.
      // Phases resume in registered_tunnel_lifecycle.
      let tunnel_registry = Arc::clone(&serialized_registry);
      match Self::registered_tunnel_lifecycle(id, (tunnel, incoming), server, shutdown, tunnel_registry).await {
        Ok(lifecycle_result) => Ok(lifecycle_result),
        Err(e) => {
          let deregistered = serialized_registry.deregister_tunnel(id).await.ok();
          match &e {
            &TunnelLifecycleError::AuthenticationRefused => tracing::debug!(err=?e, record=?deregistered, "Deregistered due to authentication refusal"),
            e => tracing::info!(err=?e, record=?deregistered, "Deregistered due to lifecycle error")
          }
          Err(e)
        }
      }
    }.instrument(tracing::span!(tracing::Level::DEBUG, "tunnel", ?id))
  }

  async fn registered_tunnel_lifecycle(
    id: TunnelId,
    (tunnel, incoming): ArcTunnelPair<'static>,
    server: Arc<ModularDaemon>,
    shutdown: Listener,
    serialized_tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
  ) -> Result<(), TunnelLifecycleError> {
    // Authenticate connections - Each connection will be piped into the authenticator,
    // which has the option of declining the connection, and may save additional metadata.
    let tunnel_authentication = {
      let server = Arc::clone(&server);
      server
        .authenticate_tunnel((tunnel, incoming), &shutdown)
        .instrument(tracing::span!(tracing::Level::DEBUG, "authentication", ?id))
        .map_err(TunnelLifecycleError::FatalError)
    };

    let (tunnel_name, tunnel, incoming) = match tunnel_authentication.await? {
      Some((tunnel_name, (tunnel, incoming))) => (tunnel_name, tunnel, incoming),
      None => {
        let _ = serialized_tunnel_registry.deregister_tunnel(id).await;
        return Ok(());
      }
    };

    // Tunnel naming - The tunnel registry is notified of the authenticator-provided tunnel name
    {
      let tunnel_registry = Arc::clone(&serialized_tunnel_registry);
      Self::name_tunnel(id, tunnel_name, tunnel_registry).instrument(tracing::span!(
        tracing::Level::DEBUG,
        "naming",
        ?id
      ))
    }
    .await?;

    // Process incoming requests until the incoming channel is closed.
    {
      let service_registry = Arc::clone(&server.service_registry);
      Self::handle_incoming_requests(id, (tunnel, incoming), service_registry, shutdown).instrument(
        tracing::span!(tracing::Level::DEBUG, "request_handling", ?id),
      )
    }
    .await?;

    // Deregister closed tunnels after graceful exit
    let _record = serialized_tunnel_registry.deregister_tunnel(id).await;

    Ok(())
  }

  // Process incoming requests until the incoming channel is closed.
  // Await a tunnel closure request from the host, or for the tunnel to close on its own.
  // A tunnel has "closed on its own" if incoming closes *or* outgoing requests fail with
  // a notification that the outgoing channel has been closed.
  //
  // The request handler for this side should be configured to send a close request for
  // the tunnel with the given ID when it sees a request fail due to tunnel closure.
  // TODO: configure request handler (?) to do that using a std::sync::Weak<ModularDaemon>.
  async fn handle_incoming_requests(
    id: TunnelId,
    (_tunnel, incoming): ArcTunnelPair<'static>,
    service_registry: Arc<dyn ServiceRegistry + Send + Sync + 'static>,
    shutdown: Listener,
  ) -> Result<(), RequestProcessingError> {
    let negotiator = Arc::new(NegotiationService::new(service_registry));

    incoming
      .streams()
      // Stop accepting new requests after a graceful shutdown is requested
      .take_until(shutdown.clone())
      .map_err(|e: TunnelError| RequestProcessingError::TunnelError(e))
      .scan((negotiator, shutdown), |(negotiator, shutdown), link| {
        let res = link.map(|content| (Arc::clone(&*negotiator), shutdown.clone(), content));
        future::ready(Some(res))
      })
      .try_for_each_concurrent(None, |(negotiator, shutdown, link)| {
        Self::handle_incoming_request(id, link, negotiator, shutdown)
      })
      .await?;

    Ok(())
  }

  async fn handle_incoming_request<Services>(
    id: TunnelId,
    link: TunnelIncomingType,
    negotiator: Arc<NegotiationService<Services>>,
    shutdown: Listener,
  ) -> Result<(), RequestProcessingError>
  where
    Services: ServiceRegistry + Send + Sync + ?Sized + 'static,
  {
    match link {
      tunnel::TunnelIncomingType::BiStream(link) => {
        Self::handle_incoming_request_bistream(id, link, negotiator, shutdown).await
      }
    }
  }

  async fn handle_incoming_request_bistream<Services>(
    tunnel_id: TunnelId,
    link: WrappedStream,
    negotiator: Arc<NegotiationService<Services>>,
    shutdown: Listener, // TODO: Respond to shutdown listener requests
  ) -> Result<(), RequestProcessingError>
  where
    Services: ServiceRegistry + Send + Sync + ?Sized + 'static,
  {
    match negotiator.negotiate(link, tunnel_id).await {
      // Tunnels established on an invalid negotiation protocol are useless; consider this fatal
      Err(NegotiationError::UnsupportedProtocolVersion) => {
        Err(RequestProcessingError::UnsupportedProtocolVersion)
      }
      // Protocol violations are not considered fatal, as they do not affect other links
      // They do still destroy the current link, however.
      Err(NegotiationError::ProtocolViolation) => Ok(()),
      Err(NegotiationError::ReadError) => Ok(()),
      Err(NegotiationError::WriteError) => Ok(()),
      // Generic refusal for when a service doesn't accept a route for whatever reason
      Err(NegotiationError::Refused) => {
        tracing::debug!("Refused remote protocol request");
        Ok(())
      }
      // Lack of support for a service is just a more specific refusal
      Err(NegotiationError::UnsupportedServiceVersion) => {
        tracing::debug!("Refused request due to unsupported service version");
        Ok(())
      }
      Ok((link, route_addr, service)) => {
        if shutdown.is_triggered() {
          // Drop services post-negotiation if the connection is awaiting
          // shutdown, instead of handing them to the service to be performed.
          return Ok(());
        }
        let route_addr: RouteAddress = route_addr;
        let service: negotiation::ArcService = service;
        match service
          .handle(route_addr.clone(), Box::new(link), tunnel_id)
          .await
        {
          // TODO: Figure out which of these should be considered fatal to the tunnel, if any
          Err(e) => {
            tracing::debug!(
              address = route_addr.as_str(),
              error = ?e,
              "Protocol Service responded with non-fatal error"
            );
            Ok(())
          }
          Ok(()) => {
            tracing::trace!(
              address = route_addr.as_str(),
              "Protocol Service reported success"
            );
            Ok(())
          }
        }
      }
    }
  }

  async fn register_tunnel(
    id: TunnelId,
    tunnel: ArcTunnel<'static>,
    tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
  ) -> Result<(), TunnelRegistrationError> {
    tunnel_registry
      .register_tunnel(id, None, tunnel)
      .map_err(|e| match e {
        TunnelRegistrationError::IdOccupied(id) => {
          tracing::error!(?id, "ID occupied; dropping tunnel");
          TunnelRegistrationError::IdOccupied(id)
        }
        TunnelRegistrationError::NameOccupied(name) => {
          // This error indicates that the tunnel registry is reporting names incorrectly, or
          // holding entries from prior launches beyond the lifetime of the server that created them
          tracing::error!(
            "Name reported as occupied, but we haven't named this tunnel yet; dropping tunnel"
          );
          TunnelRegistrationError::NameOccupied(name)
        }
      })
      .await
  }

  async fn name_tunnel(
    id: TunnelId,
    tunnel_name: TunnelName,
    tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
  ) -> Result<(), TunnelNamingError> {
    tunnel_registry
      .name_tunnel(id, tunnel_name)
      .map_err(|e| match e {
        // If a tunnel registry wishes to keep a tunnel alive past a naming clash, it
        // must rename the existing tunnel then name the new one, and report Ok here.
        TunnelNamingError::NameOccupied(name) => {
          tracing::error!(?id, "Name reports as occupied; dropping tunnel");
          TunnelNamingError::NameOccupied(name)
        }
        TunnelNamingError::TunnelNotRegistered(id) => {
          // This indicates out-of-order processing on per-tunnel events in the registry
          // To solve this, the tunnel registry task complete event processing in-order
          // for events produced by a given tunnel's lifetime. The simplest way is to
          // serialize all registry changes using a tokio::task with an ordered channel.
          tracing::error!("Tunnel reported as not registered from naming task");
          TunnelNamingError::TunnelNotRegistered(id)
        }
      })
      .await
  }
}
