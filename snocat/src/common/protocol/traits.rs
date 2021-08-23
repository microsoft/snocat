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

#[derive(thiserror::Error, Debug)]
#[error(bound = std::fmt::Debug)]
pub enum ServiceError<InternalError> {
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

impl<InternalError> ServiceError<InternalError> {
  pub fn map_internal<TNewError, F>(self, f: F) -> ServiceError<TNewError>
  where
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
    TNewError: From<InternalError>,
  {
    self.map_internal(From::from)
  }
}

pub trait Service {
  type Error;

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
