// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use crate::util::tunnel_stream::{TunnelStream, WrappedStream};
use downcast_rs::{impl_downcast, Downcast, DowncastSync};
use futures::future::{BoxFuture, FutureExt};
use std::{
  any::Any,
  backtrace::Backtrace,
  collections::BTreeMap,
  fmt::{Debug, Display},
  sync::{Arc, Weak},
};

use crate::common::protocol::tunnel::{Tunnel, TunnelError, TunnelId, TunnelName};

pub mod local;
pub mod serialized;

#[derive(Clone)]
pub struct TunnelRecord<TTunnel, TMetadata> {
  pub id: TunnelId,
  pub name: Option<TunnelName>,
  pub tunnel: Arc<TTunnel>,
  pub metadata: TMetadata,
}

impl<TTunnel, TMetadata> Debug for TunnelRecord<TTunnel, TMetadata> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct(stringify!(TunnelRecord))
      .field("id", &self.id)
      .field("name", &self.name)
      .finish_non_exhaustive()
  }
}

#[derive(thiserror::Error, Debug)]
pub enum TunnelRegistrationError<ApplicationError: Debug + Display> {
  #[error("Tunnel ID was already occupied")]
  IdOccupied(TunnelId),
  #[error("Application error in tunnel registration")]
  ApplicationError(ApplicationError),
}

impl<ApplicationError: Debug + Display> From<ApplicationError>
  for TunnelRegistrationError<ApplicationError>
{
  fn from(e: ApplicationError) -> Self {
    TunnelRegistrationError::ApplicationError(e)
  }
}

#[derive(thiserror::Error, Debug)]
pub enum TunnelNamingError<ApplicationError: Debug + Display> {
  #[error("The tunnel to be named was not found")]
  TunnelNotRegistered(TunnelId),
  #[error("Application error in tunnel naming")]
  ApplicationError(ApplicationError),
}

impl<ApplicationError: Debug + Display> From<ApplicationError>
  for TunnelNamingError<ApplicationError>
{
  fn from(e: ApplicationError) -> Self {
    TunnelNamingError::ApplicationError(e)
  }
}

pub trait TunnelRegistry<TTunnel>: Downcast + DowncastSync {
  /// Metadata attached to each registered tunnel, uniquely per tunnel
  ///
  /// Created at registration time, and readable by all record holders. Not guaranteed to be
  /// the same instance for all readers. Any edit to this object via internal mutability is not
  /// guaranteed to propagate back, except when the implementor is notified via a side channel.
  type Metadata;

  // Requires std::fmt::Debug constraint thanks to https://github.com/dtolnay/thiserror/issues/79
  type Error: Debug + Display;

  /// Looks up a tunnel by ID
  ///
  /// Note that if a TunnelRecord contains None for the tunnel reference, that does not mean that
  /// the same will hold true for other readers; The tunnel may be scoped to within that context.
  fn lookup_by_id(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>>;

  /// Looks up a tunnel by Name
  ///
  /// Note that if a TunnelRecord contains None for the tunnel reference, that does not mean that
  /// the same will hold true for other readers; The tunnel may be scoped to within that context.
  fn lookup_by_name(
    &self,
    tunnel_name: TunnelName,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>>;

  /// Called prior to authentication, a tunnel is not yet trusted and has no name,
  /// but the ID is guaranteed to remain stable throughout its lifetime.
  ///
  /// Upon disconnection, [Self::deregister_tunnel] will be called with the given [TunnelId].
  ///
  /// Returns Some(Metadata) if a tunnel was created with that ID, otherwise it had a conflict.
  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    tunnel: Arc<TTunnel>,
  ) -> BoxFuture<Result<Self::Metadata, TunnelRegistrationError<Self::Error>>>;

  /// Called after authentication, when a tunnel is given an official designation
  /// May also be called later to allow a reconnecting tunnel to replaace its old
  /// record until that record is removed.
  ///
  /// Returns Some if a tunnel by that ID existed and was renamed.
  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: Option<TunnelName>,
  ) -> BoxFuture<Result<(), TunnelNamingError<Self::Error>>>;

  /// Called to remove a tunnel from the registry after it is disconnected.
  /// Does not immediately destroy the Tunnel; previous consumers can hold
  /// an Arc containing the Tunnel instance, which will extend its lifetime.
  ///
  /// Completion of deregistration implies that lookups may no longer
  /// produce a tunnel record, but this may occur after a delay.
  fn deregister_tunnel(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>>;
}
impl_downcast!(sync TunnelRegistry<TTunnel> assoc Metadata, Error);

impl<T, TTunnel> TunnelRegistry<TTunnel> for Arc<T>
where
  T: TunnelRegistry<TTunnel> + Send + Sync + 'static,
  TTunnel: Send + Sync + 'static,
{
  type Metadata = <T as TunnelRegistry<TTunnel>>::Metadata;

  type Error = <T as TunnelRegistry<TTunnel>>::Error;

  fn lookup_by_id(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<'_, Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    self.as_ref().lookup_by_id(tunnel_id)
  }

  fn lookup_by_name(
    &self,
    tunnel_name: TunnelName,
  ) -> BoxFuture<'_, Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    self.as_ref().lookup_by_name(tunnel_name)
  }

  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    tunnel: Arc<TTunnel>,
  ) -> BoxFuture<'_, Result<Self::Metadata, TunnelRegistrationError<Self::Error>>> {
    self.as_ref().register_tunnel(tunnel_id, tunnel)
  }

  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: Option<TunnelName>,
  ) -> BoxFuture<'_, Result<(), TunnelNamingError<Self::Error>>> {
    self.as_ref().name_tunnel(tunnel_id, name)
  }

  fn deregister_tunnel(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<'_, Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    self.as_ref().deregister_tunnel(tunnel_id)
  }
}

// TODO: Any/Boxed/Opaque tunnel registry wrapper which translates std::any::Any to the appropriate types
