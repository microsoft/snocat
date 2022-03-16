// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use authentication::perform_authentication;
use futures::{
  future::{self, TryFutureExt},
  Future, Stream, StreamExt, TryStreamExt,
};
use std::{fmt::Debug, sync::Arc};
use tokio::sync::broadcast::{channel as event_channel, Sender as Broadcaster};
use tracing::Instrument;

use crate::{
  common::{
    authentication::{self, AuthenticationError, AuthenticationHandler},
    protocol::{
      negotiation::{self, NegotiationError, NegotiationService},
      service::Router,
      tunnel::{
        self, id::TunnelIDGenerator, registry::TunnelRegistry, AssignTunnelId, Tunnel,
        TunnelDownlink, TunnelError, TunnelId, TunnelIncomingType, TunnelName, WithTunnelId,
      },
      RouteAddress, ServiceRegistry,
    },
  },
  util::{cancellation::CancellationListener, dropkick::Dropkick, tunnel_stream::WrappedStream},
};

struct RegisteredTunnelInner<TRegistry: ?Sized, TRecordIdent> {
  registry: Arc<TRegistry>,
  record_identifier: TRecordIdent,
}

impl<TRegistry: ?Sized> RegisteredTunnelInner<TRegistry, <TRegistry as TunnelRegistry>::Identifier>
where
  TRegistry: TunnelRegistry + Debug,
{
  pub fn into_deregistration_dropkick(self) -> Dropkick<Box<dyn FnOnce() + Send + Sync + 'static>> {
    Dropkick::callback(Box::new(move || {
      tokio::task::spawn(async move {
        self.deregister_identifier().await;
      });
    }))
  }

  async fn deregister_identifier(self) {
    let res = self
      .registry
      .deregister_identifier(self.record_identifier)
      .await;
    if let Err(e) = res {
      tracing::warn!(error = ?e, registry = ?self.registry, "Failed to deregister tunnel: {}", e);
    }
  }
}

struct RegisteredTunnel<TTunnel> {
  tunnel: Arc<TTunnel>,
  deregistration_callback: Arc<Dropkick<Box<dyn FnOnce() + Send + Sync + 'static>>>,
}

impl<TTunnel> Clone for RegisteredTunnel<TTunnel> {
  fn clone(&self) -> Self {
    Self {
      tunnel: Arc::clone(&self.tunnel),
      deregistration_callback: Arc::clone(&self.deregistration_callback),
    }
  }
}

impl<TTunnel> RegisteredTunnel<TTunnel> {
  pub fn new<TRegistry: ?Sized, TRecordIdent>(
    tunnel: Arc<TTunnel>,
    registry: Arc<TRegistry>,
    record_identifier: TRecordIdent,
  ) -> Self
  where
    TRegistry: TunnelRegistry<Identifier = TRecordIdent> + Debug,
  {
    let inner = RegisteredTunnelInner {
      registry,
      record_identifier,
    };
    Self {
      tunnel: tunnel,
      deregistration_callback: Arc::new(inner.into_deregistration_dropkick()),
    }
  }

  pub fn inner(&self) -> &Arc<TTunnel> {
    &self.tunnel
  }
}

impl<TTunnel> From<RegisteredTunnel<TTunnel>> for Arc<TTunnel> {
  fn from(v: RegisteredTunnel<TTunnel>) -> Self {
    v.tunnel
  }
}

impl<TTunnel> Debug for RegisteredTunnel<TTunnel>
where
  TTunnel: Debug,
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    Debug::fmt(&self.tunnel, f)
  }
}

impl<TTunnel> std::ops::Deref for RegisteredTunnel<TTunnel> {
  type Target = TTunnel;

  fn deref(&self) -> &Self::Target {
    &self.tunnel
  }
}

pub struct TunnelConnectedEvent {
  pub tunnel: Arc<dyn Tunnel + 'static>,
}

impl Debug for TunnelConnectedEvent {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct(std::any::type_name::<TunnelConnectedEvent>())
      .field("id", self.tunnel.id())
      .finish()
  }
}

impl Clone for TunnelConnectedEvent {
  fn clone(&self) -> Self {
    Self {
      tunnel: self.tunnel.clone(),
    }
  }
}

#[derive(Clone)]
pub struct TunnelAuthenticatedEvent {
  pub tunnel: Arc<dyn Tunnel + 'static>,
  pub name: TunnelName,
}

impl Debug for TunnelAuthenticatedEvent {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct(std::any::type_name::<TunnelAuthenticatedEvent>())
      .field("id", self.tunnel.id())
      .finish()
  }
}

#[derive(Debug, Clone)]
pub struct TunnelDisconnectedEvent {
  pub id: TunnelId,
  // pub reason: Option<DisconnectReason>,
}

pub struct ModularDaemon<
  TTunnelRegistry: ?Sized,
  TServiceRegistry: ?Sized,
  TRouter: ?Sized,
  TAuthenticationHandler: ?Sized,
> {
  service_registry: Arc<TServiceRegistry>,
  tunnel_registry: Arc<TTunnelRegistry>,
  router: Arc<TRouter>,
  // request_handler: Arc<RequestClientHandler<TTunnel, TTunnelRegistry, TServiceRegistry, TRouter>>,
  authentication_handler: Arc<TAuthenticationHandler>,
  tunnel_id_generator: Arc<dyn TunnelIDGenerator + Send + Sync + 'static>,

  // event hooks
  pub tunnel_connected: Broadcaster<TunnelConnectedEvent>,
  pub tunnel_authenticated: Broadcaster<TunnelAuthenticatedEvent>,
  pub tunnel_disconnected: Broadcaster<TunnelDisconnectedEvent>,
}

impl<
    TTunnelRegistry: ?Sized,
    TServiceRegistry: ?Sized,
    TRouter: ?Sized,
    TAuthenticationHandler: ?Sized,
  > ModularDaemon<TTunnelRegistry, TServiceRegistry, TRouter, TAuthenticationHandler>
{
  pub fn router<'a>(&'a self) -> &Arc<TRouter> {
    &self.router
  }
}

impl<
    TTunnelRegistry: ?Sized,
    TServiceRegistry: ?Sized,
    TRouter: ?Sized,
    TAuthenticationHandler: ?Sized,
  > ModularDaemon<TTunnelRegistry, TServiceRegistry, TRouter, TAuthenticationHandler>
where
  TAuthenticationHandler: AuthenticationHandler + 'static,
  TAuthenticationHandler::Error: std::fmt::Debug,
{
  fn authenticate_tunnel<'a, TTunnel: Tunnel + 'a>(
    self: &Arc<Self>,
    tunnel: &'a TTunnel,
    shutdown: CancellationListener,
  ) -> impl Future<Output = Result<Option<tunnel::TunnelName>, anyhow::Error>> + 'a {
    let authentication_handler = Arc::clone(&self.authentication_handler);

    async move {
      let result =
        perform_authentication(authentication_handler.as_ref(), tunnel, &shutdown.into()).await;
      match result {
        Err(AuthenticationError::Handling(handling_error)) => {
          // Non-fatal handling errors are passed to tracing and close the tunnel
          tracing::warn!(
            reason = ?&handling_error,
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
        Ok(tunnel_name) => Ok(Some(tunnel_name)),
      }
    }
  }
}

#[derive(thiserror::Error, Debug)]
enum TunnelLifecycleError<ApplicationError, RegistryError> {
  #[error("Tunnel registration error")]
  RegistrationError(
    #[source]
    #[backtrace]
    RegistryError,
  ),
  #[error("{0}")]
  RequestProcessingError(
    #[source]
    #[backtrace]
    RequestProcessingError<ApplicationError>,
  ),
  #[error("Authentication refused to remote by either breach of protocol or invalid/inadequate credentials")]
  AuthenticationRefused,
  #[error("Application error encountered in tunnel lifecycle: {0:?}")]
  ApplicationError(
    #[source]
    #[backtrace]
    ApplicationError,
  ),
}

#[derive(thiserror::Error, Debug)]
enum RequestProcessingError<ApplicationError> {
  #[error("Protocol version mismatch")]
  UnsupportedProtocolVersion,
  #[error("Tunnel error encountered: {0}")]
  TunnelError(
    #[source]
    #[backtrace]
    TunnelError,
  ),
  #[error("Fatal application error")]
  ApplicationError(
    #[source]
    #[backtrace]
    ApplicationError,
  ),
}

impl<
    RegistryError: std::fmt::Debug + std::fmt::Display,
    ApplicationError: std::fmt::Debug + std::fmt::Display,
  > From<RequestProcessingError<ApplicationError>>
  for TunnelLifecycleError<ApplicationError, RegistryError>
{
  fn from(e: RequestProcessingError<ApplicationError>) -> Self {
    match e {
      RequestProcessingError::ApplicationError(fatal_error) => {
        TunnelLifecycleError::ApplicationError(fatal_error)
      }
      non_fatal => TunnelLifecycleError::RequestProcessingError(non_fatal),
    }
  }
}

impl<
    TTunnelRegistry: ?Sized,
    TServiceRegistry: ?Sized,
    TRouter: ?Sized,
    TAuthenticationHandler: ?Sized,
  > ModularDaemon<TTunnelRegistry, TServiceRegistry, TRouter, TAuthenticationHandler>
where
  Self: 'static,
  TTunnelRegistry: TunnelRegistry + Send + Sync + 'static,
  TTunnelRegistry::Record: 'static,
  TServiceRegistry: ServiceRegistry + Send + Sync + 'static,
  TRouter: Router + Send + Sync + 'static,
  TAuthenticationHandler: AuthenticationHandler + 'static,
  TAuthenticationHandler::Error: std::fmt::Debug,
{
  pub fn new(
    service_registry: Arc<TServiceRegistry>,
    tunnel_registry: Arc<TTunnelRegistry>,
    router: Arc<TRouter>,
    authentication_handler: Arc<TAuthenticationHandler>,
    tunnel_id_generator: Arc<dyn TunnelIDGenerator + Send + Sync + 'static>,
  ) -> Self {
    Self {
      service_registry,
      tunnel_registry,
      router,
      authentication_handler,
      tunnel_id_generator,

      // For event handlers, we simply drop the receive sides,
      // as new ones can be made with Sender::subscribe(&self)
      tunnel_connected: event_channel(32).0,
      tunnel_authenticated: event_channel(32).0,
      tunnel_disconnected: event_channel(32).0,
    }
  }

  /// Convert a source of tunnel progenitors into tunnels by assigning IDs from the daemon's ID generator
  pub fn assign_tunnel_ids<TTunnel, TunnelSource, TIntoTunnel>(
    &self,
    tunnel_source: TunnelSource,
  ) -> impl Stream<Item = TTunnel> + Send + 'static
  where
    TTunnel: Tunnel + 'static,
    TunnelSource: Stream<Item = TIntoTunnel> + Send + 'static,
    TIntoTunnel: AssignTunnelId<TTunnel> + 'static,
  {
    let tunnel_id_generator = self.tunnel_id_generator.clone();
    tunnel_source.map(move |pretunnel| pretunnel.assign_tunnel_id(tunnel_id_generator.next()))
  }

  /// Run the server against a tunnel_source.
  ///
  /// This can be performed concurrently against multiple sources, with a shared server instance.
  /// The implementation assumes that shutdown_request_listener will also halt the tunnel_source.
  pub fn run<TTunnel, TunnelSource>(
    self: Arc<Self>,
    tunnels: TunnelSource,
    shutdown_request_listener: CancellationListener,
  ) -> tokio::task::JoinHandle<()>
  where
    TTunnel: Tunnel + 'static,
    TunnelSource: Stream<Item = TTunnel> + Send + 'static,
  {
    // Stop accepting new Tunnels when asked to shutdown
    let tunnels = tunnels.take_until({
      let shutdown_request_listener = shutdown_request_listener.clone();
      async move { shutdown_request_listener.cancelled().await }
    });

    // Tunnel Lifecycle - Sub-pipeline performed by futures on a per-tunnel basis
    let lifecycle = tunnels.for_each_concurrent(None, move |tunnel| {
      let this = self.clone();
      let shutdown_request_listener = shutdown_request_listener.clone();
      async move {
        let tunnel_id = *tunnel.id();
        let tunnel = Arc::new(tunnel);
        match this
          .clone()
          .tunnel_lifecycle(tunnel, shutdown_request_listener)
          .await
        {
          Err(TunnelLifecycleError::AuthenticationRefused) => {
            tracing::info!(id=?tunnel_id, "Tunnel lifetime aborted due to authentication refusal");
          }
          Err(e) => {
            tracing::info!(id=?tunnel_id, error=?e, "Tunnel lifetime aborted with error {}", e);
          }
          Ok(()) => {}
        }
        this.fire_tunnel_disconnected(TunnelDisconnectedEvent { id: tunnel_id })
      }
    });

    // Spawn an instrumented task for the server which will return
    // when all connections shut down and the tunnel source closes
    tokio::task::spawn(lifecycle.instrument(tracing::span!(tracing::Level::INFO, "modular_server")))
  }
}

impl<
    TTunnelRegistry: ?Sized,
    TServiceRegistry: ?Sized,
    TRouter: ?Sized,
    TAuthenticationHandler: ?Sized,
  > ModularDaemon<TTunnelRegistry, TServiceRegistry, TRouter, TAuthenticationHandler>
where
  TTunnelRegistry: TunnelRegistry + Send + Sync + 'static,
  TTunnelRegistry::Record: 'static,
  TServiceRegistry: ServiceRegistry + Send + Sync + 'static,
  TRouter: Router + Send + Sync + 'static,
  TAuthenticationHandler: AuthenticationHandler + 'static,
  TAuthenticationHandler::Error: std::fmt::Debug,
{
  // Sends tunnel_connected event when a tunnel begins being processed by the daemon pipeline
  fn fire_tunnel_connected(&self, ev: TunnelConnectedEvent) {
    // Send; Ignore errors produced when no receivers exist to read the event
    let _ = self.tunnel_connected.send(ev);
  }

  // Sends tunnel_authenticated event when a tunnel has received its name and has been successfully registered
  fn fire_tunnel_authenticated(&self, ev: TunnelAuthenticatedEvent) {
    // Send; Ignore errors produced when no receivers exist to read the event
    let _ = self.tunnel_authenticated.send(ev);
  }

  // Sends tunnel_disconnected when a tunnel has disconnected
  fn fire_tunnel_disconnected(&self, ev: TunnelDisconnectedEvent) {
    // Send; Ignore errors produced when no receivers exist to read the event
    let _ = self.tunnel_disconnected.send(ev);
  }

  #[tracing::instrument(err, skip(self, tunnel, shutdown), fields(id=?tunnel.id()))]
  async fn tunnel_lifecycle<TTunnel>(
    self: Arc<Self>,
    tunnel: Arc<TTunnel>,
    shutdown: CancellationListener,
  ) -> Result<(), TunnelLifecycleError<anyhow::Error, TTunnelRegistry::Error>>
  where
    TTunnel: Tunnel + 'static,
  {
    // Send tunnel_connected event once the tunnel is successfully registered to its ID
    self.fire_tunnel_connected(TunnelConnectedEvent {
      tunnel: tunnel.clone() as Arc<_>,
    });

    // Authenticate connections - Each connection will be piped into the authenticator,
    // which has the option of declining the connection, and may save additional metadata.
    let tunnel_name = self
      .authenticate_tunnel(tunnel.as_ref(), shutdown.clone())
      .instrument(tracing::debug_span!("authentication"))
      .map_err(TunnelLifecycleError::ApplicationError)
      .await
      .and_then(|s| match s {
        Some(identity) => Ok(identity),
        None => Err(TunnelLifecycleError::AuthenticationRefused),
      })?;

    // Tunnel naming - The tunnel registry is notified of the authenticator-provided tunnel name
    let tunnel: RegisteredTunnel<TTunnel> = {
      Self::name_tunnel(tunnel, tunnel_name.clone(), self.tunnel_registry.clone())
        .instrument(tracing::debug_span!("naming"))
    }
    .await
    .map_err(TunnelLifecycleError::RegistrationError)?;

    // Send tunnel_authenticated event for the newly-named tunnel, once the registry is aware of it
    self.fire_tunnel_authenticated(TunnelAuthenticatedEvent {
      tunnel: tunnel.inner().clone() as Arc<_>,
      name: tunnel_name.clone(),
    });

    // Phases resume in registered_tunnel_lifecycle.
    self
      .clone()
      .registered_tunnel_lifecycle(tunnel, tunnel_name, shutdown)
      .await?;
    Ok(())
  }

  #[tracing::instrument(err, skip(self, tunnel, shutdown), fields(name=?tunnel_name, id=?tunnel.id()))]
  async fn registered_tunnel_lifecycle<TTunnel>(
    self: Arc<Self>,
    tunnel: RegisteredTunnel<TTunnel>,
    tunnel_name: TunnelName,
    shutdown: CancellationListener,
  ) -> Result<(), TunnelLifecycleError<anyhow::Error, TTunnelRegistry::Error>>
  where
    TTunnel: Tunnel + 'static,
  {
    // Process incoming requests until the incoming channel is closed.
    {
      let service_registry = Arc::clone(&self.service_registry);
      let incoming =
        tunnel
          .downlink()
          .await
          .ok_or(TunnelLifecycleError::RequestProcessingError(
            RequestProcessingError::TunnelError(TunnelError::ConnectionClosed),
          ))?;
      Self::handle_incoming_requests(tunnel, incoming, service_registry, shutdown)
        .instrument(tracing::debug_span!("request_handling"))
    }
    .await?;

    // The tunnel is dropped by this point, and will be automatically deregistered on a background task
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
  async fn handle_incoming_requests<TTunnel, TTunnelDownlink>(
    tunnel: TTunnel,
    mut incoming: TTunnelDownlink,
    service_registry: Arc<TServiceRegistry>,
    shutdown: CancellationListener,
  ) -> Result<(), RequestProcessingError<anyhow::Error>>
  where
    TTunnel: Tunnel + Clone + 'static,
    TTunnelDownlink: TunnelDownlink + Send + Unpin + 'static,
  {
    let negotiator = Arc::new(NegotiationService::new(service_registry));

    incoming
      .as_stream()
      // Stop accepting new requests after a graceful shutdown is requested
      .take_until(shutdown.clone().cancelled())
      .map_err(|e: TunnelError| RequestProcessingError::TunnelError(e))
      .scan((negotiator, shutdown), |(negotiator, shutdown), link| {
        let res = link.map(|content| (Arc::clone(&*negotiator), shutdown.clone(), content));
        future::ready(Some(res))
      })
      .try_for_each_concurrent(None, move |(negotiator, shutdown, link)| {
        Self::handle_incoming_request(tunnel.clone(), link, negotiator, shutdown)
      })
      .await?;

    Ok(())
  }

  async fn handle_incoming_request<TTunnel, Services>(
    tunnel: TTunnel,
    link: TunnelIncomingType,
    negotiator: Arc<NegotiationService<Services>>,
    shutdown: CancellationListener,
  ) -> Result<(), RequestProcessingError<anyhow::Error>>
  where
    TTunnel: Tunnel + Clone + 'static,
    Services: ServiceRegistry + Send + Sync + ?Sized + 'static,
  {
    match link {
      tunnel::TunnelIncomingType::BiStream(link) => {
        Self::handle_incoming_request_bistream(tunnel, link, negotiator, shutdown).await
      }
    }
  }

  async fn handle_incoming_request_bistream<TTunnel, Services>(
    tunnel: TTunnel,
    link: WrappedStream,
    negotiator: Arc<NegotiationService<Services>>,
    shutdown: CancellationListener, // TODO: Respond to shutdown listener requests
  ) -> Result<(), RequestProcessingError<anyhow::Error>>
  where
    TTunnel: Tunnel + Clone + 'static,
    Services: ServiceRegistry + Send + Sync + ?Sized + 'static,
  {
    match negotiator.negotiate(link, tunnel.clone()).await {
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
      Err(NegotiationError::ApplicationError(e)) => {
        tracing::warn!(err=?e, "Refused request due to application error in negotiation");
        Ok(())
      }
      Err(NegotiationError::FatalError(e)) => {
        tracing::error!(err=?e, "Refused request due to fatal application error in negotiation");
        Err(RequestProcessingError::ApplicationError(
          NegotiationError::FatalError(e).into(),
        ))
      }
      Ok((link, route_addr, service)) => {
        if shutdown.is_cancelled() {
          // Drop services post-negotiation if the connection is awaiting
          // shutdown, instead of handing them to the service to be performed.
          return Ok(());
        }
        let route_addr: RouteAddress = route_addr;
        let service: negotiation::ArcService<_> = service;
        match service
          .handle_mapped(route_addr.clone(), Box::new(link), Arc::new(tunnel) as _)
          .await
        {
          // TODO: Figure out which of these should be considered fatal to the tunnel, if any
          Err(e) => {
            tracing::debug!(
              address = ?route_addr,
              error = ?e,
              "Protocol Service responded with non-fatal error"
            );
            Ok(())
          }
          Ok(()) => {
            tracing::trace!(
              address = ?route_addr,
              "Protocol Service reported success"
            );
            Ok(())
          }
        }
      }
    }
  }

  async fn name_tunnel<TTunnel>(
    tunnel: Arc<TTunnel>,
    tunnel_name: TunnelName,
    tunnel_registry: Arc<TTunnelRegistry>,
  ) -> Result<RegisteredTunnel<TTunnel>, TTunnelRegistry::Error>
  where
    TTunnel: Tunnel + 'static,
  {
    // let naming = async move {
    //   tunnel_registry
    //     .register()
    //     .name_tunnel(id, Some(tunnel_name))
    //     .map_err(|e| match e {
    //       TunnelNamingError::TunnelNotRegistered(id) => {
    //         // This indicates out-of-order processing on per-tunnel events in the registry
    //         // To solve this, the tunnel registry task complete event processing in-order
    //         // for events produced by a given tunnel's lifetime. The simplest way is to
    //         // serialize all registry changes using a tokio::task with an ordered channel.
    //         tracing::error!("Tunnel reported as not registered from naming task");
    //         TunnelNamingError::TunnelNotRegistered(id)
    //       }
    //       TunnelNamingError::ApplicationError(e) => {
    //         tracing::error!(err=?e, "ApplicationError in tunnel naming");
    //         TunnelNamingError::ApplicationError(e)
    //       }
    //     })
    //     .await
    // };
    // tokio::spawn(naming)
    //   .await
    //   .map_err(|e| {
    //     if e.is_panic() {
    //       std::panic::resume_unwind(e.into_panic());
    //     } else {
    //       panic!("Naming task cancelled");
    //     }
    //   })
    //   .unwrap()
    todo!()
  }
}
