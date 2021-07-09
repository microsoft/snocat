// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use crate::util::tunnel_stream::{TunnelStream, WrappedStream};
use downcast_rs::{impl_downcast, Downcast, DowncastSync};
use futures::future::{BoxFuture, FutureExt};
use std::{
  any::Any,
  backtrace::Backtrace,
  collections::BTreeMap,
  fmt::Debug,
  sync::{Arc, Weak},
};

use super::tunnel::{registry::TunnelRegistry, Tunnel, TunnelId, TunnelName};
use crate::common::protocol::tunnel::TunnelError;

pub type RouteAddress = String;

pub struct Request {
  pub address: RouteAddress,
  pub protocol_client: Box<dyn DynamicResponseClient + Send + Sync + 'static>,
}

impl Debug for Request {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Request")
      .field("address", &self.address)
      .finish_non_exhaustive()
  }
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
    TProtocolClient: Client + Send + Sync + 'static,
  {
    Self {
      address,
      protocol_client: Box::new(protocol_client),
    }
  }
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum RoutingError {
  #[error("No matching tunnel could be found")]
  NoMatchingTunnel,
  #[error("The tunnel failed to provide a link")]
  LinkOpenFailure(TunnelError),
}

/// Routers are responsible for taking an address and forwarding it to
/// the appropriate tunnel. When forwarding, the router can alter the
/// address to remove any routing-specific information before it is
/// handed to the Request's protocol::Client.
pub trait Router<TTunnel, TTunnelRegistry>: Downcast + DowncastSync
where
  TTunnelRegistry: TunnelRegistry<TTunnel> + Send + Sync,
{
  fn route(
    &self,
    //TODO: Consider taking only a [RouteAddress] here, except if other request metadata is desired
    request: &Request,
    tunnel_registry: Arc<TTunnelRegistry>,
  ) -> BoxFuture<Result<(RouteAddress, Box<dyn TunnelStream + Send + Sync + 'static>), RoutingError>>;
}
impl_downcast!(sync Router<TTunnel, TTunnelRegistry> where TTunnel: Tunnel + Send + Sync, TTunnelRegistry: TunnelRegistry<TTunnel> + Send + Sync);

#[derive(thiserror::Error, Debug)]
pub enum ClientError {
  #[error("Invalid address provided to client")]
  InvalidAddress,
  #[error("Address refused by client")]
  Refused,
  #[error("Unexpected end of stream with remote")]
  UnexpectedEnd,
  #[error("Illegal response from remote")]
  IllegalResponse(Option<Backtrace>),
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
    self: Box<Self>,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Response, ClientError>>;
}

impl<TResponse, TClient> DynamicResponseClient for TClient
where
  TClient: Client<Response = TResponse> + Send + 'static,
  TResponse: Any + Send + 'static,
  Self: Sized,
{
  fn handle_dynamic(
    self: Box<Self>,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Response, ClientError>> {
    Client::handle(*self, addr, tunnel)
      .map(|result| result.map(|inner| Response::new(Box::new(inner))))
      .boxed()
  }
}

#[derive(thiserror::Error, Debug)]
pub enum ServiceError {
  #[error("Address refused by client")]
  Refused,
  #[error("Unexpected end of stream with remote")]
  UnexpectedEnd,
  #[error("Illegal response from remote")]
  IllegalResponse,
  #[error("Invalid address provided by remote client")]
  AddressError,
  #[error("An internal dependency failed")]
  DependencyFailure,
  #[error("An internal dependency failed with a backtrace")]
  BacktraceDependencyFailure(Backtrace),
  #[error(transparent)]
  InternalFailure(#[from] anyhow::Error),
}

pub trait Service {
  fn accepts(&self, addr: &RouteAddress, tunnel_id: &TunnelId) -> bool;
  // fn protocol_id() -> String where Self: Sized;

  fn handle<'a>(
    &'a self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'static>,
    tunnel_id: TunnelId,
  ) -> BoxFuture<'a, Result<(), ServiceError>>;
}

pub trait ServiceRegistry {
  fn find_service(
    self: Arc<Self>,
    addr: &RouteAddress,
    tunnel_id: &TunnelId,
  ) -> Option<Arc<dyn Service + Send + Sync + 'static>>;
}
