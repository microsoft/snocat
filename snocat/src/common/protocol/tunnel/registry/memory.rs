// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use std::{fmt::Debug, sync::Arc};

use dashmap::DashMap;
use futures::{
  future::{BoxFuture, FutureExt},
  TryFutureExt,
};
use tokio::task::JoinError;

use super::super::{registry::TunnelRegistry, TunnelName};

pub struct InMemoryTunnelRegistry<R> {
  tunnels: Arc<DashMap<TunnelName, R>>,
}

#[derive(Debug, Clone, Hash)]
pub struct InMemoryTunnelRegistryIdentifier(TunnelName);

impl InMemoryTunnelRegistryIdentifier {
  fn of(tunnel_name: &TunnelName) -> Self {
    Self(tunnel_name.clone())
  }
}

type Ident = InMemoryTunnelRegistryIdentifier;

#[derive(thiserror::Error, Debug)]
pub enum InMemoryTunnelRegistryError {
  #[error("Registry task failed to rejoin to async pool")]
  JoinError(
    #[from]
    #[backtrace]
    JoinError,
  ),
}

impl<R> TunnelRegistry for InMemoryTunnelRegistry<R>
where
  R: Send + Sync + Debug + Clone + 'static,
{
  type Identifier = InMemoryTunnelRegistryIdentifier;

  type Record = R;

  type Error = InMemoryTunnelRegistryError;

  fn lookup<'a>(
    &'a self,
    tunnel_name: &'a TunnelName,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    let tunnel_name = tunnel_name.clone();
    let tunnels = self.tunnels.clone();
    tokio::task::spawn_blocking(move || {
      let item = if let Some(item) = tunnels.get(&tunnel_name) {
        item.value().clone()
      } else {
        return Ok(None);
      };
      Ok(Some(item))
    })
    .unwrap_or_else(|e| Err(InMemoryTunnelRegistryError::from(e)))
    .boxed()
  }

  fn register<'a>(
    &'a self,
    tunnel_name: TunnelName,
    record: &'a Self::Record,
  ) -> BoxFuture<'static, Result<Self::Identifier, Self::Error>> {
    let tunnels = self.tunnels.clone();
    let record = record.clone();
    tokio::task::spawn_blocking(move || {
      let identifier = Ident::of(&tunnel_name);
      tunnels.insert(tunnel_name, record);
      identifier
    })
    .map_err(InMemoryTunnelRegistryError::from)
    .boxed()
  }

  fn deregister<'a>(
    &'a self,
    tunnel_name: &'a TunnelName,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    let tunnels = self.tunnels.clone();
    let tunnel_name = tunnel_name.clone();
    tokio::task::spawn_blocking(move || tunnels.remove(&tunnel_name).map(|(_, record)| record))
      .map_err(InMemoryTunnelRegistryError::from)
      .boxed()
  }

  fn deregister_identifier<'a>(
    &'a self,
    identifier: Self::Identifier,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    Self::deregister(&self, &identifier.0)
  }
}

impl<R> InMemoryTunnelRegistry<R> {
  pub fn new() -> Self {
    Self {
      tunnels: Default::default(),
    }
  }

  pub fn len(&self) -> usize {
    self.tunnels.len()
  }
}
