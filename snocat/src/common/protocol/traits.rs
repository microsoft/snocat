// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use crate::util::tunnel_stream::TunnelStream;
use futures::{
  future::{BoxFuture, FutureExt},
  TryFutureExt,
};
use std::{backtrace::Backtrace, fmt::Debug, marker::PhantomData, sync::Arc};

use super::{tunnel::ArcTunnel, RouteAddress};

#[derive(thiserror::Error, Debug)]
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

  fn accepts(&self, addr: &RouteAddress, tunnel: &ArcTunnel) -> bool;
  // fn protocol_id() -> String where Self: Sized;

  fn handle<'a>(
    &'a self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'static>,
    tunnel: ArcTunnel,
  ) -> BoxFuture<'a, Result<(), ServiceError<Self::Error>>>;
}

pub struct MappedService<S, E> {
  inner_service: S,
  error_phantom: PhantomData<E>,
}

impl<S, E> MappedService<S, E> {
  pub fn new(inner_service: S) -> Self {
    Self {
      inner_service,
      error_phantom: PhantomData::<E>,
    }
  }

  pub fn into_inner(self) -> S {
    self.inner_service
  }
}

impl<S, E> Service for MappedService<S, E>
where
  S: Service,
  E: From<<S as Service>::Error>,
{
  type Error = E;

  fn accepts(&self, addr: &RouteAddress, tunnel: &ArcTunnel) -> bool {
    Service::accepts(&self.inner_service, addr, tunnel)
  }

  fn handle<'a>(
    &'a self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'static>,
    tunnel: ArcTunnel,
  ) -> BoxFuture<'a, Result<(), ServiceError<Self::Error>>> {
    Service::handle(self, addr, stream, tunnel)
      .map_err(|e| ServiceError::err_into(e))
      .boxed()
  }
}

// TODO: Adopt a proc-macro crate such as [blanket](crates.io/crates/blanket) to generate ref type impls
macro_rules! impl_service_ref_type {
  // (Generic, Type-using-Generic, Param-Ident, Block-using-Param-Ident)
  ($tpe_generic: ident, $tpe: ty, $this: ident, $dereference: block) => {
    impl<$tpe_generic> Service for $tpe
    where
      $tpe_generic: Service,
    {
      type Error = <$tpe_generic as Service>::Error;

      fn accepts(&self, addr: &RouteAddress, tunnel: &ArcTunnel) -> bool {
        let dereferenced: &S = {
          let $this: &Self = self;
          $dereference
        };
        Service::accepts(dereferenced, addr, tunnel)
      }

      fn handle<'a>(
        &'a self,
        addr: RouteAddress,
        stream: Box<dyn TunnelStream + Send + 'static>,
        tunnel: ArcTunnel,
      ) -> BoxFuture<'a, Result<(), ServiceError<Self::Error>>> {
        let dereferenced: &S = {
          let $this: &Self = self;
          $dereference
        };
        Service::handle(dereferenced, addr, stream, tunnel)
      }
    }
  };
  // (Generic, Type-using-Generic) for items with Deref implementations
  ($tpe_generic: ident, $tpe: ty) => {
    impl_service_ref_type!($tpe_generic, $tpe, x, {
      let inner_ref: &$tpe_generic = std::ops::Deref::deref(x);
      inner_ref
    });
  };
}

impl_service_ref_type!(S, &S, s, { *s });
impl_service_ref_type!(S, &mut S, s, { *s });
impl_service_ref_type!(S, Box<S>);
impl_service_ref_type!(S, std::rc::Rc<S>);
impl_service_ref_type!(S, Arc<S>);

pub trait ServiceRegistry {
  type Error: std::fmt::Debug + std::fmt::Display;

  fn find_service(
    self: Arc<Self>,
    addr: &RouteAddress,
    tunnel: &ArcTunnel,
  ) -> Option<Arc<dyn Service<Error = Self::Error> + Send + Sync + 'static>>;
}
