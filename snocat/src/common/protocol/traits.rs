use crate::util::tunnel_stream::{TunnelStream, WrappedStream};
use futures::future::{BoxFuture, FutureExt};
use std::{any::Any, sync::Arc};

use super::tunnel::Tunnel;

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
    lookup: Box<dyn Fn(&str) -> BoxFuture<Option<Arc<dyn Tunnel + Send + Sync + Unpin + 'static>>>>,
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
