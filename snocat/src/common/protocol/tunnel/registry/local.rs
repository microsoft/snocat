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

use crate::common::protocol::tunnel::{Tunnel, TunnelError, TunnelId, TunnelName};

use super::{
  TunnelNamingError, TunnelRecord, TunnelRegistrationError, TunnelRegistry, TunnelRegistryMirroring,
};

pub struct InMemoryTunnelRegistry<TTunnel: ?Sized, TMetadata = ()> {
  tunnels: Arc<tokio::sync::Mutex<BTreeMap<TunnelId, TunnelRecord<TTunnel, TMetadata>>>>,
  metadata_constructor:
    Box<dyn (Fn(TunnelId, Option<&Arc<TTunnel>>) -> TMetadata) + Send + Sync + 'static>,
}

impl<TTunnel> InMemoryTunnelRegistry<TTunnel, ()> {
  pub fn new() -> Self {
    Self {
      tunnels: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
      metadata_constructor: Box::new(|_, _| ()),
    }
  }
}

impl<TTunnel: ?Sized, TMetadata> InMemoryTunnelRegistry<TTunnel, TMetadata>
where
  TMetadata: Clone + Send + Sync,
{
  pub fn new_with_shared_metadata(metadata: TMetadata) -> Self
  where
    TMetadata: 'static,
  {
    Self::new_with_metadata_constructor(move |_, _| metadata.clone())
  }

  pub fn new_with_metadata_constructor<F>(metadata_constructor: F) -> Self
  where
    F: (Fn(TunnelId, Option<&Arc<TTunnel>>) -> TMetadata) + Send + Sync + 'static,
  {
    Self {
      tunnels: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
      metadata_constructor: Box::new(metadata_constructor),
    }
  }
}

impl<TTunnel, TMetadata> InMemoryTunnelRegistry<TTunnel, TMetadata> {
  pub async fn keys(&self) -> Vec<TunnelId> {
    let lock = self.tunnels.lock().await;
    lock.keys().cloned().collect()
  }

  pub async fn max_key(&self) -> Option<TunnelId> {
    let lock = self.tunnels.lock().await;
    lock.keys().max().cloned()
  }
}

impl<TTunnel, TMetadata> TunnelRegistry<TTunnel> for InMemoryTunnelRegistry<TTunnel, TMetadata>
where
  TTunnel: Send + Sync + 'static,
  TMetadata: Clone + Send + Sync + 'static,
{
  type Metadata = TMetadata;

  type Error = anyhow::Error;

  fn lookup_by_id(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let tunnels = tunnels.lock().await;
      let tunnel = tunnels.get(&tunnel_id);
      Ok(tunnel.cloned())
    }
    .boxed()
  }

  fn lookup_by_name(
    &self,
    tunnel_name: TunnelName,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let tunnels = tunnels.lock().await;
      // Note: Inefficient total enumeration, replace with hash lookup
      let tunnel = tunnels
        .iter()
        .find(|(_id, record)| record.name.as_ref() == Some(&tunnel_name))
        .map(|(_id, record)| record.clone());
      Ok(tunnel)
    }
    .boxed()
  }

  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    tunnel: Arc<TTunnel>,
  ) -> BoxFuture<Result<Self::Metadata, TunnelRegistrationError<Self::Error>>> {
    self.register_tunnel_mirror(tunnel_id, Some(tunnel))
  }

  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: Option<TunnelName>,
  ) -> BoxFuture<Result<(), TunnelNamingError<Self::Error>>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let mut tunnels = tunnels.lock().await;
      {
        // Note: Inefficient total enumeration, replace with hash lookup
        for (_id, record) in tunnels.iter_mut().filter(|(id, record)| {
          record.name.is_some() && record.name.as_ref() == name.as_ref() && id != &&tunnel_id
        }) {
          record.name = None;
        }

        // Event may have been processed after the tunnel
        // was deregistered, or before it was registered.
        if let None = tunnels.get(&tunnel_id) {
          return Err(TunnelNamingError::TunnelNotRegistered(tunnel_id));
        }
      }

      tunnels.get_mut(&tunnel_id);
      let tunnel = tunnels
        .get_mut(&tunnel_id)
        .expect("We were just holding this, and still have the lock");

      tunnel.name = name;

      Ok(())
    }
    .boxed()
  }

  fn deregister_tunnel(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let mut tunnels = tunnels.lock().await;
      Ok(tunnels.remove(&tunnel_id))
    }
    .boxed()
  }
}

impl<TTunnel, TMetadata> TunnelRegistryMirroring<TTunnel>
  for InMemoryTunnelRegistry<TTunnel, TMetadata>
where
  TTunnel: Send + Sync + 'static,
  TMetadata: Clone + Send + Sync + 'static,
{
  fn register_tunnel_mirror(
    &self,
    tunnel_id: TunnelId,
    tunnel: Option<Arc<TTunnel>>,
  ) -> BoxFuture<Result<Self::Metadata, TunnelRegistrationError<Self::Error>>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let mut tunnels = tunnels.lock().await;
      if tunnels.contains_key(&tunnel_id) {
        return Err(TunnelRegistrationError::IdOccupied(tunnel_id));
      }
      let metadata = (self.metadata_constructor)(tunnel_id, tunnel.as_ref());
      assert!(
        tunnels
          .insert(
            tunnel_id,
            TunnelRecord {
              id: tunnel_id,
              name: None,
              tunnel: tunnel,
              metadata: metadata.clone(),
            },
          )
          .is_none(),
        "TunnelId overlap despite locked map where contains_key returned false"
      );
      Ok(metadata)
    }
    .boxed()
  }
}
