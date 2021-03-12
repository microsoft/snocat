use crate::util::tunnel_stream::{TunnelStream, WrappedStream};
use futures::future::{BoxFuture, FutureExt};
use std::{any::Any, collections::BTreeMap, fmt::Debug, sync::Arc};

use super::tunnel::{Tunnel, TunnelId, TunnelName};

pub type RouteAddress = String;

pub struct Request {
  address: RouteAddress,
  protocol_client: Box<dyn DynamicResponseClient + Send + 'static>,
}

pub struct Response {
  content: Box<dyn Any>,
}

impl Response {
  pub fn new(content: Box<dyn Any>) -> Self {
    Self { content }
  }
  pub fn content(&self) -> &Box<dyn Any> {
    &self.content
  }
  pub fn into_inner(self) -> Box<dyn Any> {
    self.content
  }
}

impl Request {
  pub fn new<TProtocolClient>(address: RouteAddress, protocol_client: TProtocolClient) -> Self
  where
    TProtocolClient: Client + Send + 'static,
  {
    Self {
      address,
      protocol_client: Box::new(protocol_client),
    }
  }
}

#[derive(Clone)]
pub struct TunnelRecord {
  id: TunnelId,
  name: Option<TunnelName>,
  tunnel: Arc<dyn Tunnel + Send + Sync + Unpin + 'static>,
}

impl Debug for TunnelRecord {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "TunnelRecord {{ id: {} }}", self.id)
  }
}

#[derive(Debug, Clone)]
pub enum TunnelRegistrationError {
  IdOccupied(TunnelId),
  NameOccupied(TunnelName),
}

#[derive(Debug, Clone)]
pub enum TunnelNamingError {
  NameOccupied(TunnelName),
  TunnelNotRegistered(TunnelId),
}

pub trait TunnelRegistry {
  fn lookup_by_id(&self, tunnel_id: TunnelId) -> BoxFuture<Option<TunnelRecord>>;
  fn lookup_by_name(&self, tunnel_name: TunnelName) -> BoxFuture<Option<TunnelRecord>>;

  /// Called prior to authentication, a tunnel is not yet trusted and has no name,
  /// but the ID is guaranteed to remain stable throughout its lifetime.
  ///
  /// Upon disconnection, [Self::drop_tunnel] will be called with the given [TunnelId].
  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: Option<TunnelName>,
    tunnel: Arc<dyn Tunnel + Send + Sync + Unpin + 'static>,
  ) -> BoxFuture<Result<(), TunnelRegistrationError>>;

  /// Called after authentication, when a tunnel is given an official designation
  /// May also be called later to allow a reconnecting tunnel to replaace its old
  /// record until that record is removed.
  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: TunnelName,
  ) -> BoxFuture<Result<(), TunnelNamingError>>;

  /// Called to remove a tunnel from the registry after it is disconnected.
  /// Does not immediately destroy the Tunnel; previous consumers can hold
  /// an Arc containing the Tunnel instance, which will extend its lifetime.
  fn deregister_tunnel(&self, tunnel_id: TunnelId) -> BoxFuture<Result<TunnelRecord, ()>>;
}

pub struct InMemoryTunnelRegistry {
  tunnels: Arc<tokio::sync::Mutex<BTreeMap<TunnelId, TunnelRecord>>>,
}

impl InMemoryTunnelRegistry {
  pub fn new() -> Self {
    Self {
      tunnels: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
    }
  }
}

impl TunnelRegistry for InMemoryTunnelRegistry {
  fn lookup_by_id(&self, tunnel_id: TunnelId) -> BoxFuture<Option<TunnelRecord>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let tunnels = tunnels.lock().await;
      let tunnel = tunnels.get(&tunnel_id);
      tunnel.cloned()
    }
    .boxed()
  }

  fn lookup_by_name(&self, tunnel_name: TunnelName) -> BoxFuture<Option<TunnelRecord>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let tunnels = tunnels.lock().await;
      // Note: Inefficient total enumeration, replace with hash lookup
      let tunnel = tunnels
        .iter()
        .find(|(_id, record)| record.name.as_ref() == Some(&tunnel_name))
        .map(|(_id, record)| record.clone());
      tunnel
    }
    .boxed()
  }

  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: Option<TunnelName>,
    tunnel: Arc<dyn Tunnel + Send + Sync + Unpin + 'static>,
  ) -> BoxFuture<Result<(), TunnelRegistrationError>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let mut tunnels = tunnels.lock().await;
      if tunnels.contains_key(&tunnel_id) {
        return Err(TunnelRegistrationError::IdOccupied(tunnel_id));
      }
      // Note: Inefficient total enumeration, replace with hash lookup
      if name.is_some() && tunnels.iter().any(|(_id, record)| record.name == name) {
        return Err(TunnelRegistrationError::NameOccupied(name.unwrap()));
      }
      tunnels
        .insert(
          tunnel_id,
          TunnelRecord {
            id: tunnel_id,
            name: name,
            tunnel,
          },
        )
        .expect_none("TunnelId overlap despite locked map where contains_key returned false");
      Ok(())
    }
    .boxed()
  }

  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: TunnelName,
  ) -> BoxFuture<Result<(), TunnelNamingError>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let tunnels = tunnels.lock().await;
      {
        let tunnel = match tunnels.get(&tunnel_id) {
          // Event may have been processed after the tunnel
          // was deregistered, or before it was registered.
          None => return Err(TunnelNamingError::TunnelNotRegistered(tunnel_id)),
          Some(t) => t,
        };

        // If any tunnel other than this one currently has the given name, bail
        // Note: Inefficient total enumeration, replace with hash lookup
        if tunnels
          .iter()
          .any(|(id, record)| record.name.as_ref() == Some(&name) && id != &tunnel.id)
        {
          return Err(TunnelNamingError::NameOccupied(name));
        }
      }

      let mut tunnels = tunnels;
      tunnels.get_mut(&tunnel_id);
      let tunnel = tunnels
        .get_mut(&tunnel_id)
        .expect("We were just holding this, and still have the lock");

      tunnel.name = Some(name);

      Ok(())
    }
    .boxed()
  }

  fn deregister_tunnel(&self, tunnel_id: TunnelId) -> BoxFuture<Result<TunnelRecord, ()>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let mut tunnels = tunnels.lock().await;
      tunnels.remove(&tunnel_id).ok_or(())
    }
    .boxed()
  }
}

#[derive(Debug, Clone)]
pub enum RoutingError {
  NoMatchingTunnel,
}

/// Routers are responsible for taking an address and forwarding it to
/// the appropriate tunnel. When forwarding, the router can alter the
/// address to remove any routing-specific information before it is
/// handed to the Request's protocol::Client.
pub trait Router {
  fn route(
    &self,
    request: &Request,
    tunnel_registry: &dyn TunnelRegistry,
    lookup: Box<dyn Fn(&str) -> BoxFuture<Option<TunnelRecord>>>,
  ) -> BoxFuture<Result<(RouteAddress, Box<dyn TunnelStream + Send + 'static>), RoutingError>>;
}

#[derive(Debug, Clone)]
pub enum ClientError {
  InvalidAddress,
  Refused,
  UnexpectedEnd,
  IllegalResponse,
}

pub trait Client {
  type Response: Send + 'static;

  fn handle(
    self,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Self::Response, ClientError>>;
}

pub trait DynamicResponseClient: Send {
  fn handle_dynamic(
    self,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Response, ClientError>>;
}

impl<TResponse, TClient> DynamicResponseClient for TClient
where
  TClient: Client<Response = TResponse> + Send + 'static,
  TResponse: Any + Send + 'static,
{
  fn handle_dynamic(
    self,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Response, ClientError>> {
    Client::handle(self, addr, tunnel)
      .map(|result| result.map(|inner| Response::new(Box::new(inner))))
      .boxed()
  }
}

#[derive(Debug, Clone)]
pub enum ServiceError {
  Refused,
  UnexpectedEnd,
  IllegalResponse,
  AddressError,
  DependencyFailure,
}

pub trait Service {
  fn accepts(&self, addr: &RouteAddress) -> bool;
  // fn protocol_id() -> String where Self: Sized;

  fn handle(
    &'_ self,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<'_, Result<(), ServiceError>>;
}

pub trait ServiceRegistry {
  fn find_service(
    self: Arc<Self>,
    addr: RouteAddress,
  ) -> Arc<Box<dyn Service + Send + Sync + 'static>>;
}
