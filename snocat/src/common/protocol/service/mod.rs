// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! High-level protocol services with bidirectional streaming communications
use downcast_rs::{impl_downcast, Downcast, DowncastSync};
use futures::{
  future::{AndThen, BoxFuture, LocalBoxFuture, Then},
  Future, FutureExt, TryFutureExt,
};
use std::{any::Any, fmt::Debug, marker::PhantomData, process::Output};

use super::RouteAddress;

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

pub trait RouteAddressBuilder {
  type Params: Send;

  fn build_addr(args: Self::Params) -> RouteAddress
  where
    Self: Sized;
}

pub trait AddressBuilderClient<'a>: Client<'a> + RouteAddressBuilder {}

// std::any::Any requires a 'static lifetime, so that is implied here
pub type BoxClientResponse = Box<dyn Any + Send + 'static>;
pub type BoxClientError = Box<dyn Any + Send + 'static>;
pub type BoxClientFuture<'client> =
  BoxFuture<'client, Result<BoxClientResponse, ClientError<BoxClientError>>>;
pub type BoxClient<'client, TStream> = Box<
  dyn Client<
      'static,
      Stream = TStream,
      Response = BoxClientResponse,
      Error = BoxClientError,
      Future = BoxClientFuture<'client>,
    > + Send
    + 'client,
>;

pub type BoxAddressBuilderClient<'client, TStream, TParams> = Box<
  dyn AddressBuilderClient<
      'static,
      Stream = TStream,
      Response = BoxClientResponse,
      Error = BoxClientError,
      Future = BoxClientFuture<'client>,
      Params = TParams,
    > + Send
    + 'client,
>;

pub type ResultTypeOf<'result, TClient> =
  Result<<TClient as Client<'result>>::Response, ClientError<<TClient as Client<'result>>::Error>>;

pub trait Client<'result> {
  type Response: Send + 'result;
  type Error: Send + 'result;
  type Stream: Send;
  type Future: Future<Output = Result<Self::Response, ClientError<Self::Error>>>;

  fn protocol_name() -> &'static str
  where
    Self: Sized;

  fn handle(self, addr: RouteAddress, stream: Self::Stream) -> Self::Future;
}

pub struct BoxDispatchClient<'c, TInnerClient> {
  client: TInnerClient,
  client_lifetime: PhantomData<&'c ()>,
}

impl<'c, TInnerClient> BoxDispatchClient<'c, TInnerClient> {
  pub fn new(client: TInnerClient) -> Self {
    Self {
      client,
      client_lifetime: PhantomData,
    }
  }
}

impl<'c, TInnerClient: 'c> Client<'static> for BoxDispatchClient<'c, TInnerClient>
where
  TInnerClient: Client<'static> + Send + 'c,
  TInnerClient::Error: Any + Send + 'static,
  TInnerClient::Response: Any + Send + 'static,
  TInnerClient::Future: Send + 'c,
{
  type Response = BoxClientResponse;

  type Error = BoxClientError;

  type Stream = TInnerClient::Stream;

  type Future = BoxClientFuture<'c>;

  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    TInnerClient::protocol_name()
  }

  fn handle(self, addr: RouteAddress, stream: Self::Stream) -> Self::Future {
    TInnerClient::handle(self.client, addr, stream)
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

impl<'client, 'result, TClient, F, ThenFut, TResponse, TError> Client<'result>
  for BoundClient<TClient, F, ThenFut, TResponse, TError>
where
  TClient: Client<'result> + 'client,
  TResponse: Send + 'result,
  TError: Send + 'result,
  F: FnOnce(Result<TClient::Response, ClientError<TClient::Error>>) -> ThenFut + 'result + 'client,
  ThenFut: Future<Output = Result<TResponse, ClientError<TError>>>,
{
  type Response = TResponse;

  type Error = TError;

  type Stream = TClient::Stream;

  type Future = Then<TClient::Future, ThenFut, F>;

  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    TClient::protocol_name()
  }

  fn handle(self, addr: RouteAddress, stream: Self::Stream) -> Self::Future {
    self.client.handle(addr, stream).then(self.f)
  }
}

pub struct MappedClient<TClient, F, TResponse, TError> {
  client: TClient,
  f: F,
  response: PhantomData<TResponse>,
  error: PhantomData<TError>,
}

impl<'client, 'result, TClient, F, TResponse, TError> Client<'result>
  for MappedClient<TClient, F, TResponse, TError>
where
  TClient: Client<'result> + 'client,
  TResponse: Send + 'result,
  TError: Send + 'result,
  F: FnOnce(
      Result<TClient::Response, ClientError<TClient::Error>>,
    ) -> Result<TResponse, ClientError<TError>>
    + 'client,
{
  type Response = TResponse;

  type Error = TError;

  type Stream = TClient::Stream;

  type Future = futures::future::Map<TClient::Future, F>;

  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    TClient::protocol_name()
  }

  fn handle(self, addr: RouteAddress, stream: Self::Stream) -> Self::Future {
    self.client.handle(addr, stream).map(self.f)
  }
}

mod private_client_ext {
  use super::Client;

  pub trait Sealed {}

  impl<'a, C> Sealed for C where C: ?Sized + Client<'a> {}
}

pub trait ClientExt<'client, 'result>: Client<'result> + private_client_ext::Sealed {
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

  fn boxed(self) -> BoxClient<'client, Self::Stream>
  where
    Self: Sized + Send + 'client,
    Self::Future: Send + 'client,
    Self::Stream: Send + 'client,
    'result: 'static,
  {
    let dispatcher: BoxDispatchClient<'client, _> = BoxDispatchClient::new(self);
    Box::new(dispatcher) as BoxClient<'client, _>
  }
}

impl<'client, 'result, TClient: Client<'result> + 'client> ClientExt<'client, 'result> for TClient {}

// Request

pub struct Request<'a, TClient> {
  pub address: RouteAddress,
  pub protocol_client: TClient,
  phantom_lifetime: PhantomData<&'a ()>,
}

impl<'a, TClient> Debug for Request<'a, TClient> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Request")
      .field("address", &self.address)
      .finish_non_exhaustive()
  }
}

impl<'client, TClient> Request<'client, TClient> {
  // By requiring that we pass the address parameters here,
  // we prevent illegal addresses from being passed. This
  // has an escape hatch by making a dishonest Client impl.
  pub fn new(address_parameters: TClient::Params, protocol_client: TClient) -> Self
  where
    TClient: Client<'client> + RouteAddressBuilder + Send + 'static,
  {
    let address = TClient::build_addr(address_parameters);
    Self {
      address,
      protocol_client,
      phantom_lifetime: PhantomData,
    }
  }
}

#[derive(thiserror::Error, Debug, Clone)]
#[error(bound = std::error::Error)]
pub enum RoutingError<TunnelRegistryError> {
  #[error("Invalid tunnel address format")]
  InvalidAddress,
  #[error("No matching tunnel could be found")]
  NoMatchingTunnel,
  #[error("The tunnel failed to provide a link")]
  LinkOpenFailure(super::tunnel::TunnelError),
  #[error("Tunnel registry handling failed")]
  TunnelRegistryError(TunnelRegistryError),
}

impl<TunnelRegistryError> From<TunnelRegistryError> for RoutingError<TunnelRegistryError> {
  fn from(e: TunnelRegistryError) -> Self {
    Self::TunnelRegistryError(e)
  }
}

/// Routers are responsible for taking an address and forwarding it to
/// the appropriate tunnel. When forwarding, the router can alter the
/// address to remove any routing-specific information before it is
/// handed to the Request's protocol::Client.
pub trait Router<Context> {
  type ContextError;

  fn route<'a, TProtocolClient>(
    self,
    request: Request<'a, TProtocolClient>,
    context: &Context,
  ) -> BoxFuture<
    'a,
    Result<<TProtocolClient as Client<'a>>::Future, RoutingError<Self::ContextError>>,
  >
  where
    TProtocolClient: Client<'a> + Send + 'a;
}

#[cfg(test)]
mod tests {
  /// This is a static test- it does not need run to test its behaviour, just compiled
  fn static_test_client_is_object_safe<'a, Stream, C: super::Client<'a> + 'a>(
    unboxed: C,
  ) -> Box<
    dyn super::Client<
        'a,
        Future = C::Future,
        Stream = C::Stream,
        Error = C::Error,
        Response = C::Response,
      > + 'a,
  > {
    Box::new(unboxed) as Box<_>
  }
}
