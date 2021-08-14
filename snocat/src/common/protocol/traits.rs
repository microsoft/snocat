// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use crate::util::tunnel_stream::{TunnelStream, WrappedStream};
use downcast_rs::{impl_downcast, Downcast, DowncastSync};
use futures::{
  future::{BoxFuture, FutureExt},
  TryFutureExt,
};
use std::{
  any::Any,
  backtrace::Backtrace,
  collections::BTreeMap,
  fmt::{Debug, Display},
  sync::{Arc, Weak},
};

use super::{
  tunnel::{registry::TunnelRegistry, Tunnel, TunnelId, TunnelName},
  RouteAddress,
};
use crate::common::protocol::tunnel::TunnelError;

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
pub enum RoutingError<TunnelRegistryError: std::fmt::Debug + std::fmt::Display> {
  #[error("Invalid tunnel address format")]
  InvalidAddress,
  #[error("No matching tunnel could be found")]
  NoMatchingTunnel,
  #[error("The tunnel failed to provide a link")]
  LinkOpenFailure(TunnelError),
  #[error("Tunnel registry handling failed")]
  TunnelRegistryError(TunnelRegistryError),
}

impl<TunnelRegistryError: std::fmt::Debug + std::fmt::Display> From<TunnelRegistryError>
  for RoutingError<TunnelRegistryError>
{
  fn from(e: TunnelRegistryError) -> Self {
    Self::TunnelRegistryError(e)
  }
}

/// Routers are responsible for taking an address and forwarding it to
/// the appropriate tunnel. When forwarding, the router can alter the
/// address to remove any routing-specific information before it is
/// handed to the Request's protocol::Client.
pub trait Router<TTunnel, TTunnelRegistry: ?Sized>: Downcast + DowncastSync
where
  TTunnelRegistry: TunnelRegistry<TTunnel> + Send + Sync,
{
  fn route(
    &self,
    //TODO: Consider taking only a [RouteAddress] here, except if other request metadata is desired
    request: &Request,
    tunnel_registry: Arc<TTunnelRegistry>,
  ) -> BoxFuture<
    Result<
      (RouteAddress, Box<dyn TunnelStream + Send + Sync + 'static>),
      RoutingError<TTunnelRegistry::Error>,
    >,
  >;
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
pub enum ServiceError<InternalError: std::fmt::Debug + std::fmt::Display> {
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
  #[error("Internal service error")]
  InternalError(InternalError),
  #[error(transparent)]
  InternalFailure(anyhow::Error),
}

impl<InternalError: std::fmt::Debug + std::fmt::Display> ServiceError<InternalError> {
  pub fn map_internal<TNewError, F>(self, f: F) -> ServiceError<TNewError>
  where
    TNewError: std::fmt::Debug + std::fmt::Display,
    F: Fn(InternalError) -> TNewError,
  {
    match self {
      ServiceError::Refused => ServiceError::Refused,
      ServiceError::UnexpectedEnd => ServiceError::UnexpectedEnd,
      ServiceError::IllegalResponse => ServiceError::IllegalResponse,
      ServiceError::AddressError => ServiceError::AddressError,
      ServiceError::DependencyFailure => ServiceError::DependencyFailure,
      ServiceError::BacktraceDependencyFailure(e) => ServiceError::BacktraceDependencyFailure(e),
      ServiceError::InternalError(e) => ServiceError::InternalError(f(e)),
      ServiceError::InternalFailure(e) => ServiceError::InternalFailure(e),
    }
  }

  pub fn err_into<TNewError>(self) -> ServiceError<TNewError>
  where
    TNewError: From<InternalError> + std::fmt::Debug + std::fmt::Display,
  {
    self.map_internal(From::from)
  }
}

impl<InternalError: std::fmt::Debug + std::fmt::Display> From<InternalError>
  for ServiceError<InternalError>
{
  fn from(e: InternalError) -> Self {
    Self::InternalError(e)
  }
}

pub trait Service {
  type Error: std::fmt::Debug + std::fmt::Display;

  fn accepts(&self, addr: &RouteAddress, tunnel_id: &TunnelId) -> bool;
  // fn protocol_id() -> String where Self: Sized;

  fn handle<'a>(
    &'a self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'static>,
    tunnel_id: TunnelId,
  ) -> BoxFuture<'a, Result<(), ServiceError<Self::Error>>>;
}

pub trait MappedService<TIntoError: std::fmt::Debug + std::fmt::Display> {
  fn accepts_mapped(&self, addr: &RouteAddress, tunnel_id: &TunnelId) -> bool;
  // fn protocol_id() -> String where Self: Sized;

  fn handle_mapped<'a>(
    &'a self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'static>,
    tunnel_id: TunnelId,
  ) -> BoxFuture<'a, Result<(), ServiceError<TIntoError>>>;
}

impl<TInnerService, TIntoError> MappedService<TIntoError> for TInnerService
where
  TInnerService: Service,
  TIntoError: From<<Self as Service>::Error> + std::fmt::Debug + std::fmt::Display,
{
  fn accepts_mapped(&self, addr: &RouteAddress, tunnel_id: &TunnelId) -> bool {
    Service::accepts(self, addr, tunnel_id)
  }

  fn handle_mapped<'a>(
    &'a self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'static>,
    tunnel_id: TunnelId,
  ) -> BoxFuture<'a, Result<(), ServiceError<TIntoError>>> {
    Service::handle(self, addr, stream, tunnel_id)
      .map_err(|e| ServiceError::err_into(e))
      .boxed()
  }
}

pub trait ServiceRegistry {
  type Error: std::fmt::Debug + std::fmt::Display;

  fn find_service(
    self: Arc<Self>,
    addr: &RouteAddress,
    tunnel_id: &TunnelId,
  ) -> Option<Arc<dyn MappedService<Self::Error> + Send + Sync + 'static>>;
}
