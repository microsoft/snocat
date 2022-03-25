// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use std::{
  fmt::{Debug, Display},
  hash::Hash,
  marker::PhantomData,
};

use futures::{FutureExt, TryFutureExt};

use super::TunnelRegistry;

pub struct MappedTunnelRegistry<Inner, Record, Identifier, InnerError, Error> {
  inner: Inner,
  phantom_record: PhantomData<Record>,
  phantom_identifier: PhantomData<Identifier>,
  phantom_inner_error: PhantomData<InnerError>,
  phantom_error: PhantomData<Error>,
}

impl<Inner, Record, Identifier, InnerError, Error>
  MappedTunnelRegistry<Inner, Record, Identifier, InnerError, Error>
{
  #[must_use]
  pub fn new(inner: Inner) -> Self {
    Self {
      inner,
      phantom_record: PhantomData::<Record>,
      phantom_identifier: PhantomData::<Identifier>,
      phantom_inner_error: PhantomData::<InnerError>,
      phantom_error: PhantomData::<Error>,
    }
  }
}

impl<Inner, Record, Identifier, InnerError, Error> Debug
  for MappedTunnelRegistry<Inner, Record, Identifier, InnerError, Error>
where
  Inner: Debug,
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct(std::any::type_name::<
      MappedTunnelRegistry<Inner, Record, Identifier, InnerError, Error>,
    >())
    .field("inner", &self.inner)
    .finish()
  }
}

#[derive(thiserror::Error, Debug)]
pub enum MappedTunnelRegistryError<Inner> {
  #[error(transparent)]
  Mapped(Inner),
  #[error("MappedTunnelRegistry translation could not downcast to the anticipated case's type")]
  Downcast,
}

impl<Inner, Record, Identifier, InnerError, Error> TunnelRegistry
  for MappedTunnelRegistry<Inner, Record, Identifier, InnerError, Error>
where
  Inner: TunnelRegistry,
  Record: From<<Inner as TunnelRegistry>::Record>
    + TryInto<<Inner as TunnelRegistry>::Record>
    + Clone
    + Debug
    + Send
    + Sync
    + 'static,
  Identifier: From<<Inner as TunnelRegistry>::Identifier>
    + TryInto<<Inner as TunnelRegistry>::Identifier>
    + Debug
    + Clone
    + Hash
    + Send
    + Sync
    + 'static,
  InnerError: From<<Inner as TunnelRegistry>::Error> + Display + Debug + Send + Sync + 'static,
  Error: From<MappedTunnelRegistryError<InnerError>> + Display + Debug + Send + Sync + 'static,
{
  type Identifier = Identifier;

  type Record = Record;

  type Error = Error;

  fn lookup<'a>(
    &'a self,
    tunnel_name: &'a crate::common::protocol::tunnel::TunnelName,
  ) -> futures::future::BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    self
      .inner
      .lookup(tunnel_name)
      .map_ok(|record_opt| record_opt.map(From::from))
      .map_err(|e| Error::from(MappedTunnelRegistryError::Mapped(e.into())))
      .boxed()
  }

  fn register<'a>(
    &'a self,
    tunnel_name: crate::common::protocol::tunnel::TunnelName,
    record: &'a Self::Record,
  ) -> futures::future::BoxFuture<'static, Result<Self::Identifier, Self::Error>> {
    let inner_record: <Inner as TunnelRegistry>::Record =
      if let Ok(inner_record) = Record::try_into(<Record as Clone>::clone(record)) {
        inner_record
      } else {
        return futures::future::ready(Err(MappedTunnelRegistryError::Downcast.into())).boxed();
      };
    self
      .inner
      .register(tunnel_name, &inner_record)
      .map_ok(From::from)
      .map_err(|e| Error::from(MappedTunnelRegistryError::Mapped(e.into())))
      .boxed()
  }

  fn deregister<'a>(
    &'a self,
    tunnel_name: &'a crate::common::protocol::tunnel::TunnelName,
  ) -> futures::future::BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    self
      .inner
      .deregister(tunnel_name)
      .map_ok(|record_opt| record_opt.map(From::from))
      .map_err(|e| Error::from(MappedTunnelRegistryError::Mapped(e.into())))
      .boxed()
  }

  fn deregister_identifier<'a>(
    &'a self,
    identifier: Self::Identifier,
  ) -> futures::future::BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    let inner_identifier: <Inner as TunnelRegistry>::Identifier =
      if let Ok(inner_identifier) = Identifier::try_into(identifier) {
        inner_identifier
      } else {
        return futures::future::ready(Err(MappedTunnelRegistryError::Downcast.into())).boxed();
      };
    self
      .inner
      .deregister_identifier(inner_identifier)
      .map_ok(|record_opt| record_opt.map(From::from))
      .map_err(|e| Error::from(MappedTunnelRegistryError::Mapped(e.into())))
      .boxed()
  }
}
