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

use crate::common::protocol::tunnel::{Tunnel, TunnelError, TunnelId, TunnelName};

pub mod local;
pub mod serialized;

#[derive(PartialEq, Eq, Hash)]
pub struct TunnelRecord<TTunnel: ?Sized, TMetadata> {
  pub id: TunnelId,
  pub name: Option<TunnelName>,
  pub tunnel: Option<Arc<TTunnel>>,
  pub metadata: TMetadata,
}

impl<TTunnel: ?Sized, TMetadata> Clone for TunnelRecord<TTunnel, TMetadata>
where
  TMetadata: Clone,
{
  fn clone(&self) -> Self {
    Self {
      id: self.id,
      name: self.name.clone(),
      tunnel: self.tunnel.clone(),
      metadata: self.metadata.clone(),
    }
  }
}

impl<TTunnel: ?Sized, TMetadata> TunnelRecord<TTunnel, TMetadata> {
  pub fn map_metadata<TOutputMetadata, F: FnOnce(TMetadata) -> TOutputMetadata>(
    self,
    f: F,
  ) -> TunnelRecord<TTunnel, TOutputMetadata> {
    TunnelRecord {
      id: self.id,
      name: self.name,
      tunnel: self.tunnel,
      metadata: f(self.metadata),
    }
  }

  // I'd use Into/From but apparently this clashes because you can't assert that TMetadata != TOutputMetadata
  pub fn convert_metadata<TOutputMetadata>(self) -> TunnelRecord<TTunnel, TOutputMetadata>
  where
    TOutputMetadata: From<TMetadata>,
  {
    self.map_metadata(From::from)
  }
}

impl<TTunnel: ?Sized, TMetadata> Debug for TunnelRecord<TTunnel, TMetadata> {
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

pub trait TunnelRegistry<TTunnel: ?Sized>: Downcast + DowncastSync {
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
  /// May also be called later to allow a reconnecting tunnel to replace its old
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

/// An extension to the Registry trait which allows registration of unowned tunnels.
/// Intended to allow mirroring of another registry across processes or hosts.
pub trait TunnelRegistryMirroring<TTunnel: ?Sized>: TunnelRegistry<TTunnel> {
  /// A superset of functionality over TunnelRegistry::register_tunnel
  /// in that it also allows passing unowned tunnels / "None", and can
  /// specify the exact metadata used in the remote tunnel record.
  fn register_tunnel_mirror(
    &self,
    tunnel_id: TunnelId,
    tunnel: Option<Arc<TTunnel>>,
    metadata: Self::Metadata,
  ) -> BoxFuture<Result<Self::Metadata, TunnelRegistrationError<Self::Error>>>;
}

impl<T, TTunnel: ?Sized> TunnelRegistry<TTunnel> for Arc<T>
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

pub trait AnyError: std::any::Any + std::fmt::Display + std::fmt::Debug {}

impl<T> AnyError for T where T: std::any::Any + std::fmt::Display + std::fmt::Debug {}

/// Any/Boxed/Opaque tunnel registry wrapper which translates outputs to std::any::Any
#[repr(transparent)]
pub struct BoxedTunnelRegistryInner<TTunnelRegistry>(TTunnelRegistry);

impl<T, TTunnel: ?Sized> TunnelRegistry<TTunnel> for BoxedTunnelRegistryInner<T>
where
  T: TunnelRegistry<TTunnel> + Send + Sync + 'static,
  TTunnel: Send + Sync + 'static,
{
  type Metadata = Box<dyn std::any::Any + 'static>;
  type Error = Box<dyn AnyError + 'static>;

  fn lookup_by_id(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    self
      .0
      .lookup_by_id(tunnel_id)
      .map_ok(|ok| ok.map(|tr| tr.map_metadata(|metadata| Box::new(metadata) as _)))
      .map_err(|e| Box::new(e) as _)
      .boxed()
  }

  fn lookup_by_name(
    &self,
    tunnel_name: TunnelName,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    self
      .0
      .lookup_by_name(tunnel_name)
      .map_ok(|ok| ok.map(|tr| tr.map_metadata(|metadata| Box::new(metadata) as _)))
      .map_err(|e| Box::new(e) as _)
      .boxed()
  }

  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    tunnel: Arc<TTunnel>,
  ) -> BoxFuture<Result<Self::Metadata, TunnelRegistrationError<Self::Error>>> {
    self
      .0
      .register_tunnel(tunnel_id, tunnel)
      .map_ok(|metadata| Box::new(metadata) as _)
      .map_err(|e| match e {
        TunnelRegistrationError::IdOccupied(id) => TunnelRegistrationError::IdOccupied(id),
        TunnelRegistrationError::ApplicationError(e) => {
          TunnelRegistrationError::ApplicationError(Box::new(e) as _)
        }
      })
      .boxed()
  }

  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: Option<TunnelName>,
  ) -> BoxFuture<Result<(), TunnelNamingError<Self::Error>>> {
    self
      .0
      .name_tunnel(tunnel_id, name)
      .map_err(|e| match e {
        TunnelNamingError::TunnelNotRegistered(id) => TunnelNamingError::TunnelNotRegistered(id),
        TunnelNamingError::ApplicationError(e) => {
          TunnelNamingError::ApplicationError(Box::new(e) as _)
        }
      })
      .boxed()
  }

  fn deregister_tunnel(
    &self,
    tunnel_id: TunnelId,
  ) -> BoxFuture<Result<Option<TunnelRecord<TTunnel, Self::Metadata>>, Self::Error>> {
    self
      .0
      .deregister_tunnel(tunnel_id)
      .map_ok(|ok| ok.map(|tr| tr.map_metadata(|metadata| Box::new(metadata) as _)))
      .map_err(|e| Box::new(e) as _)
      .boxed()
  }
}

#[repr(transparent)]
pub struct BoxedTunnelRegistry<TTunnel: ?Sized>(
  Box<
    dyn TunnelRegistry<
        TTunnel,
        Metadata = Box<dyn std::any::Any + 'static>,
        Error = Box<dyn AnyError + 'static>,
      > + 'static,
  >,
);

impl<TTunnel: ?Sized> BoxedTunnelRegistry<TTunnel> {
  pub fn into_box<T>(inner: T) -> Self
  where
    TTunnel: Send + Sync + 'static,
    T: TunnelRegistry<TTunnel> + Send + Sync + 'static,
    T::Error: AnyError,
    T::Metadata: std::any::Any,
  {
    Self(Box::new(BoxedTunnelRegistryInner(inner)) as Box<_>)
  }
}
