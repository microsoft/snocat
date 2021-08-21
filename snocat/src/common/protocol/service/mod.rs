// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! High-level protocol services with bidirectional streaming communications
use downcast_rs::{impl_downcast, Downcast, DowncastSync};
use futures::{
  future::{AndThen, BoxFuture, LocalBoxFuture, Then},
  Future, FutureExt, TryFutureExt,
};
use std::{any::Any, fmt::Debug, marker::PhantomData, process::Output};

use crate::util::tunnel_stream::TunnelStream;

use super::{negotiation::NegotiationError, RouteAddress};

// Client

#[derive(thiserror::Error, Debug)]
#[error(bound = std::fmt::Debug)]
pub enum ClientError<ApplicationError> {
  #[error("Invalid address provided to client")]
  InvalidAddress,
  #[error("Address refused by client")]
  Refused,
  #[error("Unexpected end of stream with remote")]
  UnexpectedEnd,
  #[error("Illegal response from remote")]
  IllegalResponse(ApplicationError),
}

impl<ApplicationError> ClientError<ApplicationError> {
  pub fn map_err<F, TErr>(self, f: F) -> ClientError<F::Output>
  where
    F: FnOnce(ApplicationError) -> TErr,
  {
    match self {
      ClientError::InvalidAddress => ClientError::InvalidAddress,
      ClientError::Refused => ClientError::Refused,
      ClientError::UnexpectedEnd => ClientError::UnexpectedEnd,
      ClientError::IllegalResponse(user_error) => ClientError::IllegalResponse(f(user_error)),
    }
  }
}

pub trait ProtocolInfo {
  fn protocol_name() -> &'static str
  where
    Self: Sized;
}

pub trait RouteAddressBuilder: ProtocolInfo {
  type Params: Send;
  type BuildError: Send;

  fn build_addr(args: Self::Params) -> Result<RouteAddress, Self::BuildError>
  where
    Self: Sized;
}

pub type ClientResult<'result, TClient, TStream> = Result<
  <TClient as Client<'result, TStream>>::Response,
  ClientError<<TClient as Client<'result, TStream>>::Error>,
>;
pub type ResultTypeOf<'result, TClient, TStream> =
  <<TClient as Client<'result, TStream>>::Future as Future>::Output;

pub trait Client<'result, TStream>: ProtocolInfo {
  type Response: Send + 'result;
  type Error: Send + 'result;
  type Future: Future<Output = Result<Self::Response, ClientError<Self::Error>>>;

  fn handle(self, addr: RouteAddress, stream: TStream) -> Self::Future;
}

pub trait BoxedClient<'client, 'result>:
  Client<'result, Box<dyn TunnelStream + Send + 'client>>
{
  fn handle(
    self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'result>,
  ) -> Self::Future;
}

pub trait AddressBuilderClient<'a, TStream>: Client<'a, TStream> + RouteAddressBuilder {}

// std::any::Any requires a 'static lifetime, so that is implied here
pub type BoxClientResponse = Box<dyn Any + Send + 'static>;
pub type BoxClientError = Box<dyn Any + Send + 'static>;
pub type BoxClientFuture<'client> =
  BoxFuture<'client, Result<BoxClientResponse, ClientError<BoxClientError>>>;
pub type BoxClient<'client> = Box<
  dyn BoxedClient<
      'client,
      'static,
      Response = BoxClientResponse,
      Error = BoxClientError,
      Future = BoxClientFuture<'client>,
    > + Send
    + 'client,
>;

pub struct BoxDispatchClient<'c, TStream, TInnerClient> {
  client: TInnerClient,
  client_lifetime: PhantomData<&'c ()>,
  stream: PhantomData<TStream>,
}

impl<'c, TStream, TInnerClient> BoxDispatchClient<'c, TStream, TInnerClient> {
  pub fn new(client: TInnerClient) -> Self {
    Self {
      client,
      client_lifetime: PhantomData,
      stream: PhantomData,
    }
  }
}

impl<'c, TStream, TInnerClient: 'c> ProtocolInfo for BoxDispatchClient<'c, TStream, TInnerClient>
where
  TInnerClient: ProtocolInfo,
{
  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    TInnerClient::protocol_name()
  }
}

impl<'c, TStream, TInnerClient: 'c> Client<'static, TStream>
  for BoxDispatchClient<'c, TStream, TInnerClient>
where
  TInnerClient: Client<'static, TStream> + Send + 'c,
  TInnerClient::Error: Any + Send + 'static,
  TInnerClient::Response: Any + Send + 'static,
  TInnerClient::Future: Send + 'c,
{
  type Response = BoxClientResponse;

  type Error = BoxClientError;

  type Future = BoxClientFuture<'c>;

  fn handle(self, addr: RouteAddress, stream: TStream) -> Self::Future {
    TInnerClient::handle(self.client, addr, stream)
      .map_ok(|ok| Box::new(ok) as Box<_>)
      .map_err(|err| err.map_err(|err| Box::new(err) as Box<_>))
      .boxed()
  }
}

impl<'c, TInnerClient: 'c> BoxedClient<'c, 'static>
  for BoxDispatchClient<'c, Box<(dyn TunnelStream + std::marker::Send + 'c)>, TInnerClient>
where
  TInnerClient: Client<'static, Box<(dyn TunnelStream + std::marker::Send + 'c)>> + Send + 'c,
  TInnerClient::Error: Any + Send + 'static,
  TInnerClient::Response: Any + Send + 'static,
  TInnerClient::Future: Send + 'c,
{
  fn handle(self, addr: RouteAddress, stream: Box<dyn TunnelStream + Send + 'c>) -> Self::Future {
    <TInnerClient as Client<_>>::handle(self.client, addr, stream)
      .map_ok(|ok| Box::new(ok) as Box<_>)
      .map_err(|err| err.map_err(|err| Box::new(err) as Box<_>))
      .boxed()
  }
}

pub struct BoxStreamDispatchClient<'c, TInnerClient> {
  client: TInnerClient,
  client_lifetime: PhantomData<&'c ()>,
}

impl<'c, TInnerClient> BoxStreamDispatchClient<'c, TInnerClient> {
  pub fn new(client: TInnerClient) -> Self {
    Self {
      client,
      client_lifetime: PhantomData,
    }
  }
}

impl<'c, TInnerClient: 'c> ProtocolInfo for BoxStreamDispatchClient<'c, TInnerClient>
where
  TInnerClient: ProtocolInfo,
{
  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    TInnerClient::protocol_name()
  }
}

impl<'c, TInnerClient: 'c> Client<'static, Box<dyn TunnelStream + Send + 'c>>
  for BoxStreamDispatchClient<'c, TInnerClient>
where
  TInnerClient: Client<'static, Box<dyn TunnelStream + Send + 'c>> + Send + 'c,
  TInnerClient::Error: Any + Send + 'static,
  TInnerClient::Response: Any + Send + 'static,
  TInnerClient::Future: Send + 'c,
{
  type Response = BoxClientResponse;

  type Error = BoxClientError;

  type Future = BoxClientFuture<'c>;

  fn handle(self, addr: RouteAddress, stream: Box<dyn TunnelStream + Send + 'c>) -> Self::Future {
    TInnerClient::handle(self.client, addr, stream)
      .map_ok(|ok| Box::new(ok) as Box<_>)
      .map_err(|err| err.map_err(|err| Box::new(err) as Box<_>))
      .boxed()
  }
}

impl<'c, TInnerClient: 'c> BoxedClient<'c, 'static> for BoxStreamDispatchClient<'c, TInnerClient>
where
  TInnerClient: Client<'static, Box<(dyn TunnelStream + std::marker::Send + 'c)>> + Send + 'c,
  TInnerClient::Error: Any + Send + 'static,
  TInnerClient::Response: Any + Send + 'static,
  TInnerClient::Future: Send + 'c,
{
  fn handle(self, addr: RouteAddress, stream: Box<dyn TunnelStream + Send + 'c>) -> Self::Future {
    <TInnerClient as Client<_>>::handle(self.client, addr, stream)
      .map_ok(|ok| Box::new(ok) as Box<_>)
      .map_err(|err| err.map_err(|err| Box::new(err) as Box<_>))
      .boxed()
  }
}

pub struct BoundClient<TClient, F, Fut, TResponse, TError> {
  client: TClient,
  f: F,
  future: PhantomData<Fut>,
  response: PhantomData<TResponse>,
  error: PhantomData<TError>,
}

impl<'client, 'result, TClient, F, ThenFut, TResponse, TError> ProtocolInfo
  for BoundClient<TClient, F, ThenFut, TResponse, TError>
where
  TClient: ProtocolInfo,
{
  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    TClient::protocol_name()
  }
}

impl<'client, 'result, TStream, TClient, F, ThenFut, TResponse, TError> Client<'result, TStream>
  for BoundClient<TClient, F, ThenFut, TResponse, TError>
where
  TClient: Client<'result, TStream> + 'client,
  TResponse: Send + 'result,
  TError: Send + 'result,
  F: FnOnce(Result<TClient::Response, ClientError<TClient::Error>>) -> ThenFut + 'result + 'client,
  ThenFut: Future<Output = Result<TResponse, ClientError<TError>>>,
{
  type Response = TResponse;

  type Error = TError;

  type Future = Then<TClient::Future, ThenFut, F>;

  fn handle(self, addr: RouteAddress, stream: TStream) -> Self::Future {
    self.client.handle(addr, stream).then(self.f)
  }
}

pub struct MappedClient<TClient, F, TResponse, TError> {
  client: TClient,
  f: F,
  response: PhantomData<TResponse>,
  error: PhantomData<TError>,
}

impl<'client, 'result, TClient, F, TResponse, TError> ProtocolInfo
  for MappedClient<TClient, F, TResponse, TError>
where
  TClient: ProtocolInfo,
{
  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    TClient::protocol_name()
  }
}

impl<'client, 'result, TStream, TClient, F, TResponse, TError> Client<'result, TStream>
  for MappedClient<TClient, F, TResponse, TError>
where
  TClient: Client<'result, TStream> + 'client,
  TResponse: Send + 'result,
  TError: Send + 'result,
  F: FnOnce(
      Result<TClient::Response, ClientError<TClient::Error>>,
    ) -> Result<TResponse, ClientError<TError>>
    + 'client,
{
  type Response = TResponse;

  type Error = TError;

  type Future = futures::future::Map<TClient::Future, F>;

  fn handle(self, addr: RouteAddress, stream: TStream) -> Self::Future {
    self.client.handle(addr, stream).map(self.f)
  }
}

pub trait ClientExt<'client, 'result, TStream>: Client<'result, TStream> {
  fn map<F, ThenFut, TResponse, TError>(self, f: F) -> MappedClient<Self, F, TResponse, TError>
  where
    Self: Sized,
    TResponse: Send + 'result,
    TError: Send + 'result,
    F: FnOnce(
        Result<Self::Response, ClientError<Self::Error>>,
      ) -> Result<TResponse, ClientError<TError>>
      + 'client,
  {
    MappedClient {
      client: self,
      f,
      error: PhantomData,
      response: PhantomData,
    }
  }

  fn then<F, ThenFut, TResponse, TError>(
    self,
    f: F,
  ) -> BoundClient<Self, F, ThenFut, TResponse, TError>
  where
    Self: Sized,
    TResponse: Send + 'result,
    TError: Send + 'result,
    F: FnOnce(Result<Self::Response, ClientError<Self::Error>>) -> ThenFut + 'result + 'client,
    ThenFut: Future<Output = Result<TResponse, ClientError<TError>>>,
  {
    BoundClient {
      client: self,
      f,
      error: PhantomData,
      response: PhantomData,
      future: PhantomData,
    }
  }
}

impl<'client, 'result, TStream, TClient: Client<'result, TStream> + 'client>
  ClientExt<'client, 'result, TStream> for TClient
{
}

// Request

#[derive(Clone)]
pub struct Request<'a, TStream, TClient> {
  pub address: RouteAddress,
  pub protocol_client: TClient,
  phantom_lifetime: PhantomData<&'a ()>,
  phantom_stream: PhantomData<TStream>,
}

impl<'a, TStream, TClient> Debug for Request<'a, TStream, TClient> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Request")
      .field("address", &self.address)
      .finish_non_exhaustive()
  }
}

impl<'client, TStream, TClient> Request<'client, TStream, TClient> {
  // By requiring that we pass the address parameters here,
  // we prevent illegal addresses from being passed. This
  // has an escape hatch by making a dishonest Client impl.
  pub fn new(
    protocol_client: TClient,
    address_parameters: <TClient as RouteAddressBuilder>::Params,
  ) -> Result<Self, <TClient as RouteAddressBuilder>::BuildError>
  where
    TClient: Client<'client, TStream> + RouteAddressBuilder + Send + 'static,
  {
    let address = TClient::build_addr(address_parameters)?;
    Ok(Self {
      address,
      protocol_client,
      phantom_lifetime: PhantomData,
      phantom_stream: PhantomData,
    })
  }
}

#[derive(thiserror::Error, Debug)]
#[error(bound = std::fmt::Debug)]
pub enum RoutingError<RouterError> {
  #[error("Route not found for request")]
  RouteNotFound(RouteAddress),
  #[error("Route found but unavailable for request")]
  RouteUnavailable(RouteAddress),
  #[error("Invalid tunnel address format")]
  InvalidAddress,
  #[error("The tunnel failed to provide a link")]
  LinkOpenFailure(#[from] super::tunnel::TunnelError),
  #[error("Protocol negotiation failed")]
  NegotiationError(NegotiationError<RouterError>),
  #[error("Routing error: {0:?}")]
  RouterError(RouterError),
}

impl<T, RouterError> From<NegotiationError<T>> for RoutingError<RouterError>
where
  T: Into<RouterError>,
{
  fn from(negotiation_error: NegotiationError<T>) -> Self {
    RoutingError::NegotiationError(match negotiation_error {
      NegotiationError::ReadError => NegotiationError::ReadError,
      NegotiationError::WriteError => NegotiationError::WriteError,
      NegotiationError::ProtocolViolation => NegotiationError::ProtocolViolation,
      NegotiationError::Refused => NegotiationError::Refused,
      NegotiationError::UnsupportedProtocolVersion => NegotiationError::UnsupportedProtocolVersion,
      NegotiationError::UnsupportedServiceVersion => NegotiationError::UnsupportedServiceVersion,
      NegotiationError::ApplicationError(e) => NegotiationError::ApplicationError(e.into()),
      NegotiationError::FatalError(e) => NegotiationError::FatalError(e.into()),
    })
  }
}

pub type RouterResult<'client, 'result, TRouter, TProtocolClient> = Result<
  <TProtocolClient as Client<'result, <TRouter as Router>::Stream>>::Future,
  RoutingError<<TRouter as Router>::Error>,
>;

/// Routers are responsible for taking an address and forwarding it to
/// the appropriate tunnel. When forwarding, the router can alter the
/// address to remove any routing-specific information before it is
/// handed to the Request's protocol::Client.
pub trait Router {
  type Error;
  type Stream;
  type LocalAddress;

  fn route<'client, 'result, TProtocolClient, IntoLocalAddress: Into<Self::LocalAddress>>(
    &self,
    request: Request<'client, Self::Stream, TProtocolClient>,
    local_address: IntoLocalAddress,
  ) -> BoxFuture<'client, Result<TProtocolClient::Future, RoutingError<Self::Error>>>
  where
    TProtocolClient: Client<'result, Self::Stream> + Send + 'client;
}

#[cfg(test)]
mod tests {
  use super::ClientExt;

  /// This is a static test- it does not need run to test its behaviour, just compiled
  fn static_test_boxed_client_is_object_safe<'a, 'result, Stream, C>(
    _unboxed: C,
  ) -> Option<
    Box<
      dyn super::BoxedClient<
          'a,
          'result,
          Future = C::Future,
          Error = C::Error,
          Response = C::Response,
        > + 'a,
    >,
  >
  where
    Stream: Send + 'a,
    C: super::Client<'result, Stream> + Send + 'a,
    C::Future: Send,
  {
    None
  }
}
