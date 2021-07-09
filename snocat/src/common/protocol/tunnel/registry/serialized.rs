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

use super::{TunnelNamingError, TunnelRecord, TunnelRegistrationError, TunnelRegistry};

/// A TunnelRegistry wrapper that ensures that mutations are performed sequentially,
/// using a RwLock to serialize all write operations while allowing lookups to be concurrent.
///
/// Use this when your registry would otherwise perform or evaluate requests out-of-order,
/// as a means of avoiding updates occurring before registrations complete or similar.
///
/// TODO: A more performant method would be a key-based locking mechanism on TunnelID
pub struct SerializedTunnelRegistry<TInner: ?Sized> {
  inner: Arc<tokio::sync::RwLock<Arc<TInner>>>,
}

impl<TTunnelRegistry> SerializedTunnelRegistry<TTunnelRegistry>
where
  TTunnelRegistry: ?Sized,
{
  pub fn new(inner: Arc<TTunnelRegistry>) -> Self {
    Self {
      inner: Arc::new(tokio::sync::RwLock::new(inner)),
    }
  }
}

impl<TTunnelRegistry, TTunnel> TunnelRegistry<TTunnel> for SerializedTunnelRegistry<TTunnelRegistry>
where
  TTunnel: Tunnel + Send + Sync + Unpin + 'static,
  TTunnelRegistry: TunnelRegistry<TTunnel> + Send + Sync + ?Sized,
{
  type Metadata = TTunnelRegistry::Metadata;

  type Error = TTunnelRegistry::Error;

  fn lookup_by_id(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.read().await;
      lock.lookup_by_id(tunnel_id).await
    }
    .boxed()
  }

  fn lookup_by_name(
    &self,
    tunnel_name: TunnelName,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.read().await;
      lock.lookup_by_name(tunnel_name).await
    }
    .boxed()
  }

  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    tunnel: Arc<TTunnel>,
  ) -> BoxFuture<Result<Self::Metadata, TunnelRegistrationError<Self::Error>>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.write().await;
      lock.register_tunnel(tunnel_id, tunnel).await
    }
    .boxed()
  }

  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: Option<TunnelName>,
  ) -> BoxFuture<Result<(), TunnelNamingError<Self::Error>>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.write().await;
      lock.name_tunnel(tunnel_id, name).await
    }
    .boxed()
  }

  fn deregister_tunnel(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.write().await;
      lock.deregister_tunnel(tunnel_id).await
    }
    .boxed()
  }
}
