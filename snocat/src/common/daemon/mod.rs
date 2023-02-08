// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use authentication::perform_authentication;
use dashmap::DashMap;
use futures::{
  future::{self, TryFutureExt},
  Future, Stream, StreamExt, TryStream, TryStreamExt,
};
use std::{
  fmt::{Debug, Display},
  hash::Hash,
  sync::{Arc, Mutex, Weak},
  time::{Instant, SystemTime},
};
use tokio::sync::broadcast::{channel as event_channel, Sender as Broadcaster};
use tracing::Instrument;

use crate::{
  common::{
    authentication::{self, AuthenticationError, AuthenticationHandler},
    protocol::{
      negotiation::{self, NegotiationError, NegotiationService},
      service::Router,
      tunnel::{
        self,
        id::{TunnelIdGenerator, TunnelIdGeneratorExt},
        registry::TunnelRegistry,
        IntoTunnel, Tunnel, TunnelDownlink, TunnelError, TunnelId, TunnelIncomingType, TunnelName,
        WithTunnelId,
      },
      RouteAddress, ServiceRegistry,
    },
  },
  util::{cancellation::CancellationListener, dropkick::Dropkick, tunnel_stream::WrappedStream},
};

use super::{
  authentication::{AuthenticationAttributes, AuthenticationHandlingError},
  protocol::tunnel::{TunnelControl, TunnelMonitoring},
};

#[derive(Clone)]
pub struct PeerRecord {
  pub id: TunnelId,
  pub name: TunnelName,
  pub registered_at: (Instant, std::time::SystemTime),
  pub attributes: Arc<AuthenticationAttributes>,
  pub tunnel: Arc<dyn Tunnel + Send + Sync + 'static>,
}

impl Debug for PeerRecord {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("PeerTunnel")
      .field("id", &self.id)
      .field("name", &self.name)
      .finish_non_exhaustive()
  }
}

impl PartialEq for PeerRecord {
  fn eq(&self, other: &Self) -> bool {
    self.id == other.id && self.name == other.name
  }
}
impl Eq for PeerRecord {}

/// [PeerTunnel] uses the ID and Name fields for hashing, skipping tunnel attributes
impl Hash for PeerRecord {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    self.id.hash(state);
    self.name.hash(state);
  }
}

#[derive(Clone)]
pub struct PeersView {
  by_name: Weak<DashMap<TunnelName, Arc<DashMap<TunnelId, Weak<PeerRecord>>>>>,
  by_id: Weak<Arc<DashMap<TunnelId, Weak<PeerRecord>>>>,
}

impl PeersView {
  #[must_use]
  pub fn new(
    peers_by_name: Weak<DashMap<TunnelName, Arc<DashMap<TunnelId, Weak<PeerRecord>>>>>,
    peers_by_id: Weak<Arc<DashMap<TunnelId, Weak<PeerRecord>>>>,
  ) -> Self {
    Self {
      by_name: peers_by_name,
      by_id: peers_by_id,
    }
  }

  pub fn get_by_id(&self, tunnel_id: &TunnelId) -> Option<Arc<PeerRecord>> {
    if let Some(peers_by_id) = self.by_id.upgrade() {
      peers_by_id.get(tunnel_id).and_then(|x| x.upgrade())
    } else {
      None
    }
  }

  pub fn get_by_name(&self, tunnel_name: &TunnelName) -> Vec<Arc<PeerRecord>> {
    if let Some(peers_by_name) = self.by_name.upgrade() {
      if let Some(subtable) = peers_by_name.get(tunnel_name) {
        subtable
          .iter()
          .flat_map(|kv| Weak::upgrade(&kv.value()))
          .collect()
      } else {
        Default::default()
      }
    } else {
      Default::default()
    }
  }

  pub fn find_by_name_and_predicate<P: for<'a> Fn(&'a PeerRecord) -> bool>(
    &self,
    tunnel_name: &TunnelName,
    predicate: P,
  ) -> Option<Arc<PeerRecord>> {
    if let Some(peers_by_name) = self.by_name.upgrade() {
      if let Some(subtable) = peers_by_name.get(tunnel_name) {
        subtable.iter().find_map(|kv| {
          if let Some(upgraded) = kv.upgrade() {
            if predicate(upgraded.as_ref()) {
              Some(upgraded)
            } else {
              None
            }
          } else {
            None
          }
        })
      } else {
        Default::default()
      }
    } else {
      Default::default()
    }
  }

  /// Allows fetching a tunnel by comparing its attributes with those of all others
  pub fn find_by_comparator<Ordering: std::cmp::Ord, P: for<'a> Fn(&'a PeerRecord) -> Ordering>(
    &self,
    comparator_predicate: P,
  ) -> Option<Arc<PeerRecord>> {
    if let Some(peers_by_id) = self.by_id.upgrade() {
      peers_by_id
        .iter()
        .filter_map(|kv| kv.upgrade())
        .max_by_key(|tunnel| comparator_predicate(tunnel.as_ref()))
    } else {
      Default::default()
    }
  }

  /// Allows fetching a tunnel by comparing its attributes with those of others with the same name
  pub fn find_by_name_and_comparator<
    Ordering: std::cmp::Ord,
    P: for<'a> Fn(&'a PeerRecord) -> Ordering,
  >(
    &self,
    tunnel_name: &TunnelName,
    comparator_predicate: P,
  ) -> Option<Arc<PeerRecord>> {
    if let Some(peers_by_name) = self.by_name.upgrade() {
      if let Some(subtable) = peers_by_name.get(tunnel_name) {
        subtable
          .iter()
          .filter_map(|kv| kv.upgrade())
          .max_by_key(|tunnel| comparator_predicate(tunnel.as_ref()))
      } else {
        Default::default()
      }
    } else {
      Default::default()
    }
  }

  /// Fetch a vector of all active tunnels
  pub fn all(&self) -> Vec<Arc<PeerRecord>> {
    self
      .by_id
      .upgrade()
      .iter()
      .flat_map(|by_id| by_id.iter().filter_map(|kv| kv.value().upgrade()))
      .collect()
  }
}

#[derive(Clone, Default)]
pub struct PeerTracker {
  by_name: Arc<DashMap<TunnelName, Arc<DashMap<TunnelId, Weak<PeerRecord>>>>>,
  by_id: Arc<Arc<DashMap<TunnelId, Weak<PeerRecord>>>>,
}

impl PeerTracker {
  #[must_use]
  pub fn new() -> Self {
    Self {
      by_id: Default::default(),
      by_name: Default::default(),
    }
  }

  #[must_use]
  pub fn view(&self) -> PeersView {
    PeersView {
      by_name: Arc::downgrade(&self.by_name),
      by_id: Arc::downgrade(&self.by_id),
    }
  }

  fn insert(&self, record: &Arc<PeerRecord>) {
    self.by_id.insert(record.id, Arc::downgrade(record));
    self
      .by_name
      .entry(record.name.clone())
      .or_insert_with(|| Default::default())
      .insert(record.id, Arc::downgrade(record));
  }
}

struct DeregisteringTunnelWrapper<TRegistry: ?Sized, TRecordIdent> {
  registry: Arc<TRegistry>,
  record_identifier: TRecordIdent,
  peers: PeersView,
  peer_record: Arc<PeerRecord>,
  disconnection_broadcaster: Arc<Broadcaster<TunnelDisconnectedEvent>>,
}

impl<TRegistry: ?Sized>
  DeregisteringTunnelWrapper<TRegistry, <TRegistry as TunnelRegistry>::Identifier>
where
  TRegistry: TunnelRegistry,
{
  pub fn into_deregistration_dropkick(
    self,
  ) -> Dropkick<Arc<Mutex<Option<Box<dyn FnOnce() + Send + Sync + 'static>>>>> {
    Dropkick::new(Arc::new(Mutex::new(Some(Box::new(move || {
      tokio::task::spawn(async move {
        self.deregister_identifier().await;
      });
    })))))
  }

  async fn deregister_identifier(self) {
    // Fire disconnected event
    let _ = self
      .disconnection_broadcaster
      .send(TunnelDisconnectedEvent {
        id: self.peer_record.id,
      });
    // Deregister from local by-id and by-name tables
    // These are held behind a weakref, because if they're already gone, we have no need to clear them
    // Because these map containers perform a lot of mutexing, we run them in a blocking thread and move on.
    tokio::task::spawn_blocking({
      let peers_by_name = self.peers.by_name;
      let peers_by_id = self.peers.by_id;
      let peer_record = self.peer_record;
      move || {
        if let Some(peers_by_id) = peers_by_id.upgrade() {
          peers_by_id.remove(&peer_record.id);
        }
        if let Some(peers_by_name) = peers_by_name.upgrade() {
          // Fetch by-id sub-entry for the specified name
          let by_id = peers_by_name.get(&peer_record.name).map(|x| Arc::clone(&x));
          // Clear the given ID from the map, if it is present
          by_id.map(|peers_by_id| peers_by_id.remove(&peer_record.id));
          // Shrink by-name table by removing empty by-id sub-entries
          peers_by_name.retain(|_, peers_by_id: &mut Arc<DashMap<_, _>>| !peers_by_id.is_empty());
        }
      }
    })
    .await
    .expect("PeerTunnel clear operation failed to rejoin");
    let res = self
      .registry
      .deregister_identifier(self.record_identifier)
      .await;
    if let Err(e) = res {
      tracing::warn!(error = ?e, "Failed to deregister tunnel: {}", e);
    }
  }
}

struct WrappedTunnel<TTunnel> {
  tunnel: Arc<TTunnel>,
  drop_callback: Arc<Dropkick<Arc<Mutex<Option<Box<dyn FnOnce() + Send + Sync + 'static>>>>>>,
}

impl<TTunnel> Clone for WrappedTunnel<TTunnel> {
  fn clone(&self) -> Self {
    Self {
      tunnel: Arc::clone(&self.tunnel),
      drop_callback: Arc::clone(&self.drop_callback),
    }
  }
}

impl<TTunnel> WrappedTunnel<TTunnel> {
  pub fn new_deregistering<TRegistry: ?Sized, TRecordIdent>(
    tunnel: Arc<TTunnel>,
    registry: Arc<TRegistry>,
    record_identifier: TRecordIdent,
    peers: PeersView,
    peer_record: Arc<PeerRecord>,
    disconnection_broadcaster: Arc<Broadcaster<TunnelDisconnectedEvent>>,
  ) -> Self
  where
    TRegistry: TunnelRegistry<Identifier = TRecordIdent>,
  {
    let inner = DeregisteringTunnelWrapper {
      registry,
      record_identifier,
      peers,
      peer_record,
      disconnection_broadcaster,
    };
    Self {
      tunnel: tunnel,
      drop_callback: Arc::new(inner.into_deregistration_dropkick()),
    }
  }

  pub fn new_registration_failure_closing(tunnel: Arc<TTunnel>) -> Self
  where
    TTunnel: Send + Sync + TunnelControl + 'static,
  {
    let target = tunnel.clone();
    Self {
      tunnel,
      drop_callback: Arc::new(Dropkick::new(Arc::new(Mutex::new(Some(Box::new(
        move || {
          tokio::task::spawn(async move {
            TunnelControl::close(
              &target,
              tunnel::TunnelCloseReason::AuthenticationFailure {
                remote_responsible: None,
              },
            )
            .await
          });
        },
      )))))),
    }
  }

  /// Removes the tunnel from the wrapper and prevents the drop callback from executing
  pub fn extract_inner(self) -> Arc<TTunnel> {
    self.drop_callback.counter_take_mutex();
    self.tunnel
  }

  pub fn as_inner(&self) -> &Arc<TTunnel> {
    &self.tunnel
  }
}

impl<TTunnel> From<WrappedTunnel<TTunnel>> for Arc<TTunnel> {
  fn from(v: WrappedTunnel<TTunnel>) -> Self {
    v.tunnel
  }
}

impl<TTunnel> Debug for WrappedTunnel<TTunnel>
where
  TTunnel: Debug,
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    Debug::fmt(&self.tunnel, f)
  }
}

impl<TTunnel> std::ops::Deref for WrappedTunnel<TTunnel> {
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
  pub attributes: Arc<AuthenticationAttributes>,
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
  TRecordConstructor: ?Sized,
> {
  service_registry: Arc<TServiceRegistry>,
  tunnel_registry: Arc<TTunnelRegistry>,
  router: Arc<TRouter>,
  // request_handler: Arc<RequestClientHandler<TTunnel, TTunnelRegistry, TServiceRegistry, TRouter>>,
  authentication_handler: Arc<TAuthenticationHandler>,
  tunnel_id_generator: Arc<dyn TunnelIdGenerator + Send + Sync + 'static>,
  record_constructor: Arc<TRecordConstructor>,
  peers: PeerTracker,

  // event hooks
  pub tunnel_connected: Arc<Broadcaster<TunnelConnectedEvent>>,
  pub tunnel_authenticated: Arc<Broadcaster<TunnelAuthenticatedEvent>>,
  pub tunnel_disconnected: Arc<Broadcaster<TunnelDisconnectedEvent>>,
}

#[derive(thiserror::Error, Debug)]
enum TunnelLifecycleError<ApplicationError, AuthHandlingError, RegistryError> {
  #[error("Tunnel registration error")]
  RegistrationError(
    #[source]
    #[cfg_attr(feature = "backtrace", backtrace)]
    RegistryError,
  ),
  #[error("Request Processing Error: {0}")]
  RequestProcessingError(
    #[source]
    #[cfg_attr(feature = "backtrace", backtrace)]
    RequestProcessingError<ApplicationError>,
  ),
  #[error("Authentication refused to remote by either breach of protocol or invalid/inadequate credentials")]
  AuthenticationRefused,
  #[error("Authentication Handling Error: {0}")]
  AuthenticationHandlingError(
    #[source]
    #[cfg_attr(feature = "backtrace", backtrace)]
    AuthenticationHandlingError<AuthHandlingError>,
  ),
  #[error("Application error encountered in tunnel lifecycle: {0:?}")]
  ApplicationError(
    #[source]
    #[cfg_attr(feature = "backtrace", backtrace)]
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
    #[cfg_attr(feature = "backtrace", backtrace)]
    TunnelError,
  ),
  #[error("Fatal application error")]
  ApplicationError(
    #[source]
    #[cfg_attr(feature = "backtrace", backtrace)]
    ApplicationError,
  ),
}

mod record_constructor {
  use crate::common::{
    authentication::AuthenticationAttributes,
    protocol::tunnel::{ArcTunnel, TunnelId, TunnelName},
  };
  use futures::{future::BoxFuture, Future};
  use std::{marker::PhantomData, ops::Deref, sync::Arc};

  pub struct RecordConstructorArgs {
    pub id: TunnelId,
    pub name: TunnelName,
    pub attributes: AuthenticationAttributes,
    pub tunnel: ArcTunnel<'static>,
  }

  pub type RecordConstructorResult<Record, Error> =
    BoxFuture<'static, Result<(Record, Arc<AuthenticationAttributes>), Error>>;

  pub type RecordConstructorSuccess<Record> = (Record, Arc<AuthenticationAttributes>);

  pub trait RecordConstructor: private::Sealed {
    type Record;
    type Error;
    type Future: Future<Output = Result<RecordConstructorSuccess<Self::Record>, Self::Error>>;
    fn construct_record(&self, args: RecordConstructorArgs) -> Self::Future;
  }

  pub struct BoxedRecordConstructor<'fut, Record, Error> {
    inner: Box<
      dyn RecordConstructor<
          Record = Record,
          Error = Error,
          Future = BoxFuture<'fut, Result<RecordConstructorSuccess<Record>, Error>>,
        > + Send
        + Sync
        + 'fut,
    >,
  }

  impl<'fut, Record, Error> BoxedRecordConstructor<'fut, Record, Error> {
    pub fn new<R>(inner: R) -> Self
    where
      R: RecordConstructor<Record = Record, Error = Error> + Send + Sync + 'fut,
      <R as RecordConstructor>::Future: Send + 'fut,
    {
      let wrapped: BoxingRecordConstructor<'fut, R> = BoxingRecordConstructor {
        inner: inner,
        phantom_fut: PhantomData,
      };
      Self {
        inner: Box::new(wrapped),
      }
    }
  }

  pub struct ArcRecordConstructor<'fut, Record, Error> {
    inner: Arc<
      dyn RecordConstructor<
          Record = Record,
          Error = Error,
          Future = BoxFuture<'fut, Result<RecordConstructorSuccess<Record>, Error>>,
        > + Send
        + Sync
        + 'fut,
    >,
  }

  impl<'fut, Record, Error> ArcRecordConstructor<'fut, Record, Error> {
    pub fn new<R>(inner: R) -> Self
    where
      R: RecordConstructor<Record = Record, Error = Error> + Send + Sync + 'fut,
      <R as RecordConstructor>::Future: Send + 'fut,
    {
      let wrapped: BoxingRecordConstructor<'fut, R> = BoxingRecordConstructor {
        inner: inner,
        phantom_fut: PhantomData,
      };
      Self {
        inner: Arc::new(wrapped),
      }
    }
  }

  // TODO: Lifetimes are welded to static on these; consider blog "The Better Alternative to Lifetime GATs" for possible solution
  impl<Record, Error> RecordConstructor for BoxedRecordConstructor<'static, Record, Error>
  where
    // No clue why this one is required; I miss Haskell and Scala
    dyn RecordConstructor<
        Record = Record,
        Error = Error,
        Future = BoxFuture<'static, Result<RecordConstructorSuccess<Record>, Error>>,
      > + Send
      + Sync
      + 'static: RecordConstructor,
  {
    type Record = Record;
    type Error = Error;
    type Future = BoxFuture<'static, Result<RecordConstructorSuccess<Self::Record>, Self::Error>>;
    fn construct_record(&self, args: RecordConstructorArgs) -> Self::Future {
      self.inner.as_ref().construct_record(args)
    }
  }

  impl<Record, Error> RecordConstructor for ArcRecordConstructor<'static, Record, Error>
  where
    dyn RecordConstructor<
        Record = Record,
        Error = Error,
        Future = BoxFuture<'static, Result<RecordConstructorSuccess<Record>, Error>>,
      > + Send
      + Sync
      + 'static: RecordConstructor,
  {
    type Record = Record;
    type Error = Error;
    type Future = BoxFuture<'static, Result<RecordConstructorSuccess<Self::Record>, Self::Error>>;
    fn construct_record(&self, args: RecordConstructorArgs) -> Self::Future {
      self.inner.as_ref().construct_record(args)
    }
  }

  struct BoxingRecordConstructor<'fut, Wrapped> {
    inner: Wrapped,
    phantom_fut: PhantomData<&'fut ()>,
  }

  impl<'fut, Wrapped> RecordConstructor for BoxingRecordConstructor<'fut, Wrapped>
  where
    Wrapped: RecordConstructor + Send + Sync,
    <Wrapped as RecordConstructor>::Future: Send + 'fut,
  {
    type Record = <Wrapped as RecordConstructor>::Record;
    type Error = <Wrapped as RecordConstructor>::Error;
    type Future = BoxFuture<'fut, Result<RecordConstructorSuccess<Self::Record>, Self::Error>>;
    fn construct_record(&self, args: RecordConstructorArgs) -> Self::Future {
      use futures::future::FutureExt;
      self.inner.construct_record(args).boxed()
    }
  }

  impl<Record, Error, F, Fut> RecordConstructor for F
  where
    F: Fn(RecordConstructorArgs) -> Fut,
    Fut: Future<Output = Result<RecordConstructorSuccess<Record>, Error>>,
  {
    type Record = Record;
    type Error = Error;
    type Future = Fut;
    fn construct_record(&self, args: RecordConstructorArgs) -> Self::Future {
      (self)(args)
    }
  }

  impl<T> RecordConstructor for Arc<T>
  where
    T: RecordConstructor,
  {
    type Record = T::Record;
    type Error = T::Error;
    type Future = T::Future;
    fn construct_record(&self, args: RecordConstructorArgs) -> Self::Future {
      <Arc<T> as Deref>::deref(self).construct_record(args)
    }
  }

  mod private {
    use super::RecordConstructor;
    pub trait Sealed {}

    impl<R: ?Sized + RecordConstructor> Sealed for R {}
  }
}

pub use record_constructor::{
  ArcRecordConstructor, BoxedRecordConstructor, RecordConstructor, RecordConstructorArgs,
  RecordConstructorResult, RecordConstructorSuccess,
};

impl<
    ApplicationError: std::fmt::Debug + std::fmt::Display,
    AuthHandlingError: std::fmt::Debug + std::fmt::Display,
    RegistryError: std::fmt::Debug + std::fmt::Display,
  > From<RequestProcessingError<ApplicationError>>
  for TunnelLifecycleError<ApplicationError, AuthHandlingError, RegistryError>
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

// Implementations with no dependencies on content
impl<
    TTunnelRegistry: ?Sized,
    TServiceRegistry: ?Sized,
    TRouter: ?Sized,
    TAuthenticationHandler: ?Sized,
    TRecordConstructor: ?Sized,
  >
  ModularDaemon<
    TTunnelRegistry,
    TServiceRegistry,
    TRouter,
    TAuthenticationHandler,
    TRecordConstructor,
  >
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
    TRecordConstructor: ?Sized,
  >
  ModularDaemon<
    TTunnelRegistry,
    TServiceRegistry,
    TRouter,
    TAuthenticationHandler,
    TRecordConstructor,
  >
where
  Self: 'static,
  TTunnelRegistry: TunnelRegistry + Send + Sync + 'static,
  TTunnelRegistry::Record: Send + Sync + 'static,
  TTunnelRegistry::Error: Debug + Display + Send + 'static,
  TServiceRegistry: ServiceRegistry + Send + Sync + 'static,
  TRouter: Router + Send + Sync + 'static,
  TAuthenticationHandler: AuthenticationHandler + 'static,
  TAuthenticationHandler::Error: Debug + Display + Send + 'static,
  TRecordConstructor: RecordConstructor<Record = TTunnelRegistry::Record, Error = TTunnelRegistry::Error>
    + Send
    + Sync
    + 'static,
  <TRecordConstructor as RecordConstructor>::Future: Send,
{
  pub fn new(
    service_registry: Arc<TServiceRegistry>,
    tunnel_registry: Arc<TTunnelRegistry>,
    peer_tracker: PeerTracker,
    router: Arc<TRouter>,
    authentication_handler: Arc<TAuthenticationHandler>,
    tunnel_id_generator: Arc<dyn TunnelIdGenerator + Send + Sync + 'static>,
    record_constructor: Arc<TRecordConstructor>,
  ) -> Self {
    let s = Self {
      service_registry,
      tunnel_registry,
      router,
      authentication_handler,
      tunnel_id_generator,
      record_constructor,
      peers: peer_tracker,

      // For event handlers, we simply drop the receive sides,
      // as new ones can be made with Sender::subscribe(&self)
      tunnel_connected: Arc::new(event_channel(32).0),
      tunnel_authenticated: Arc::new(event_channel(32).0),
      tunnel_disconnected: Arc::new(event_channel(32).0),
    };
    s
  }

  pub fn peers(&self) -> PeersView {
    PeersView {
      by_name: Arc::downgrade(&self.peers.by_name),
      by_id: Arc::downgrade(&self.peers.by_id),
    }
  }

  /// Convert a source of tunnel progenitors into tunnels by assigning IDs from the
  /// daemon's ID generator, stopping and returning the first error that is is provided.
  pub fn try_construct_tunnels<TunnelSource>(
    &self,
    tunnel_source: TunnelSource,
  ) -> impl TryStream<
    Ok = <<TunnelSource as TryStream>::Ok as IntoTunnel>::Tunnel,
    Error = TunnelSource::Error,
  >
  where
    TunnelSource: TryStream,
    <TunnelSource as TryStream>::Ok: IntoTunnel,
  {
    self
      .tunnel_id_generator
      .clone()
      .try_construct_tunnels(tunnel_source)
  }

  /// Convert a source of tunnel progenitors into tunnels by assigning IDs from the daemon's ID generator
  pub fn construct_tunnels<TunnelSource>(
    &self,
    tunnel_source: TunnelSource,
  ) -> impl Stream<Item = <<TunnelSource as Stream>::Item as IntoTunnel>::Tunnel>
  where
    TunnelSource: Stream,
    <TunnelSource as Stream>::Item: IntoTunnel,
  {
    self
      .tunnel_id_generator
      .clone()
      .construct_tunnels(tunnel_source)
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
    TTunnel: Tunnel + TunnelControl + TunnelMonitoring + 'static,
    TunnelSource: Stream<Item = TTunnel> + Send + 'static,
  {
    // Stop accepting new Tunnels when asked to shutdown
    let tunnels = tunnels.take_until({
      let shutdown_request_listener = shutdown_request_listener.clone();
      async move { shutdown_request_listener.cancelled().await }
    });

    // Tunnel Lifecycle - Sub-pipeline performed by futures on a per-tunnel basis
    let lifecycle = tunnels.for_each_concurrent(None, move |tunnel: TTunnel| {
      let this = self.clone();
      let shutdown_request_listener = shutdown_request_listener.clone();
      async move {
        let tunnel_id = *tunnel.id();
        let tunnel: Arc<TTunnel> = Arc::new(tunnel);
        let close_handle: Weak<TTunnel> = Arc::downgrade(&tunnel);
        match this
          .clone()
          .tunnel_lifecycle(tunnel, shutdown_request_listener)
          .await
        {
          Err(TunnelLifecycleError::AuthenticationRefused) => {
            tracing::info!(id=?tunnel_id, "Tunnel lifetime aborted due to authentication refusal");
            if let Some(t) = close_handle.upgrade() {
              // TODO: Determine if remote or local is responsible for the refusal
              tokio::task::spawn(async move {
                t.close(tunnel::TunnelCloseReason::AuthenticationFailure {
                  remote_responsible: None,
                })
                .await
              });
            }
          }
          Err(e) => {
            tracing::info!(id=?tunnel_id, error=?e, "Tunnel lifetime aborted with error {}", e);
            if let Some(t) = close_handle.upgrade() {
              let error_message = e.to_string();
              tokio::task::spawn(async move {
                t.close(tunnel::TunnelCloseReason::ApplicationErrorMessage(
                  Arc::new(error_message) as Arc<_>,
                ))
                .await
              });
            }
          }
          Ok(()) => {
            if let Some(t) = close_handle.upgrade() {
              tokio::task::spawn(async move {
                t.close(tunnel::TunnelCloseReason::GracefulExit {
                  remote_initiated: false,
                })
                .await
              });
            }
          }
        }
      }
    });

    // Spawn an instrumented task for the server which will return
    // when all connections shut down and the tunnel source closes
    tokio::task::spawn(lifecycle.instrument(tracing::span!(tracing::Level::INFO, "modular_server")))
  }

  fn authenticate_tunnel<'a, ApplicationError, TTunnel: Tunnel + Clone + 'static>(
    self: &Arc<Self>,
    tunnel: TTunnel,
    shutdown: CancellationListener,
  ) -> impl Future<
    Output = Result<
      (tunnel::TunnelName, AuthenticationAttributes),
      TunnelLifecycleError<ApplicationError, TAuthenticationHandler::Error, TTunnelRegistry::Error>,
    >,
  > + 'static {
    let authentication_handler = Arc::clone(&self.authentication_handler);
    let tunnel = tunnel.clone();
    async move {
      let result: Result<(_, _), AuthenticationError<_>> = tokio::task::spawn(async move {
        perform_authentication(authentication_handler.as_ref(), &tunnel, &shutdown.into()).await
      })
      .unwrap_or_else(|e| {
        Err(AuthenticationError::Handling(
          AuthenticationHandlingError::JoinError(e),
        ))
      })
      .await;
      match result {
        Err(AuthenticationError::Handling(handling_error)) => {
          // Non-fatal handling errors are passed to tracing and close the tunnel
          // TODO: Feed this upward as a tunnel lifecycle failure
          tracing::warn!(
            reason = ?&handling_error,
            "Tunnel closed due to authentication handling failure"
          );
          Err(TunnelLifecycleError::AuthenticationHandlingError(
            handling_error.err_into(),
          ))
        }
        Err(AuthenticationError::Remote(remote_error)) => {
          tracing::debug!(
            reason = (&remote_error as &dyn std::error::Error),
            "Tunnel closed due to remote authentication failure"
          );
          Err(TunnelLifecycleError::AuthenticationRefused)
        }
        Ok(tunnel) => Ok(tunnel),
      }
    }
  }

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

  #[tracing::instrument(err, skip(self, tunnel, shutdown), fields(id=?tunnel.id()))]
  async fn tunnel_lifecycle<TTunnel>(
    self: Arc<Self>,
    tunnel: Arc<TTunnel>,
    shutdown: CancellationListener,
  ) -> Result<
    (),
    TunnelLifecycleError<anyhow::Error, TAuthenticationHandler::Error, TTunnelRegistry::Error>,
  >
  where
    TTunnel: Tunnel + TunnelControl + 'static,
  {
    let tunnel = WrappedTunnel::new_registration_failure_closing(tunnel);
    // Send tunnel_connected event once the tunnel is successfully registered to its ID
    self.fire_tunnel_connected(TunnelConnectedEvent {
      tunnel: tunnel.tunnel.clone() as Arc<_>,
    });

    // Authenticate connections - Each connection will be piped into the authenticator,
    // which has the option of declining the connection, and may save additional metadata.
    let (tunnel_name, tunnel_attrs) = {
      let res = self
        .authenticate_tunnel(tunnel.clone(), shutdown.clone())
        .instrument(tracing::debug_span!("authentication"))
        .await;
      res
    }?;

    // Tunnel naming - The tunnel registry is notified of the authenticator-provided tunnel name
    let tunnel: WrappedTunnel<TTunnel> = {
      self
        .register_tunnel(
          tunnel,
          tunnel_name.clone(),
          self.tunnel_registry.clone(),
          tunnel_attrs,
          self.record_constructor.clone(),
        )
        .instrument(tracing::debug_span!("naming"))
    }
    .await
    .map_err(TunnelLifecycleError::RegistrationError)?;

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
    tunnel: WrappedTunnel<TTunnel>,
    tunnel_name: TunnelName,
    shutdown: CancellationListener,
  ) -> Result<
    (),
    TunnelLifecycleError<anyhow::Error, TAuthenticationHandler::Error, TTunnelRegistry::Error>,
  >
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
          .handle(route_addr.clone(), Box::new(link), Arc::new(tunnel) as _)
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

  async fn register_tunnel<TTunnel>(
    &self,
    tunnel: WrappedTunnel<TTunnel>,
    tunnel_name: TunnelName,
    tunnel_registry: Arc<TTunnelRegistry>,
    attributes: AuthenticationAttributes,
    record_constructor: Arc<TRecordConstructor>,
  ) -> Result<WrappedTunnel<TTunnel>, TTunnelRegistry::Error>
  where
    TTunnel: Tunnel + TunnelControl + 'static,
  {
    let registered_at = (Instant::now(), SystemTime::now());
    let (record, attributes) = record_constructor
      .construct_record(RecordConstructorArgs {
        id: tunnel.id().clone(),
        name: tunnel_name.clone(),
        attributes: attributes,
        tunnel: tunnel.as_inner().clone() as Arc<_>,
      })
      .await?;
    let identifier = tunnel_registry
      .register(tunnel_name.clone(), &record)
      .await?;

    let tunnel_id = *tunnel.id();
    let peer_record = Arc::new(PeerRecord {
      id: tunnel_id,
      name: tunnel_name.clone(),
      registered_at,
      attributes: Arc::clone(&attributes),
      tunnel: tunnel.as_inner().clone() as Arc<_>,
    });
    self.peers.insert(&peer_record);

    // Inform the tunnel of its new registration status, once the registry is aware of it
    if TunnelControl::report_authentication_success(&tunnel, tunnel_name.clone())
      .await
      .is_ok()
    {
      // Send Daemon-level tunnel_authenticated event for the newly-named tunnel,
      // but only if the tunnel was not reported as closed during the authentication event firing.
      self.fire_tunnel_authenticated(TunnelAuthenticatedEvent {
        tunnel: tunnel.as_inner().clone() as Arc<_>,
        name: tunnel_name,
        attributes: attributes,
      });
    }

    Ok(WrappedTunnel::new_deregistering(
      tunnel.extract_inner().clone(),
      tunnel_registry,
      identifier,
      self.peers(),
      peer_record,
      self.tunnel_disconnected.clone(),
    ))
  }
}
