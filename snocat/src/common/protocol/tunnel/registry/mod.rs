// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use downcast_rs::{impl_downcast, Downcast, DowncastSync};
use futures::future::BoxFuture;
use std::{
  fmt::{Debug, Display},
  hash::Hash,
};

use crate::common::protocol::tunnel::TunnelName;

pub mod cache;
pub mod memory;
// pub mod serialized;

/// An eventually-consistent mapping of [`TunnelName`]/Tunnel associations
///
/// This guarantees only eventual consistency, and that tunnel records are safe to cache; as
/// such, this function is allowed to return tunnels which are no longer present in the set
/// represented here- or which have been mutated. Such mutations are allowed to signal this
/// implementation for deletion mechanisms such as tombstone placement.
pub trait TunnelRegistry: Downcast + DowncastSync {
  /// A value returned that uniquely addresses a registered association,
  /// such that it can be cleared without complex queries or lookup.
  ///
  /// Note that Identifiers may be irreversible to their inputs.
  type Identifier: Send + Sync + Debug + Clone + Hash + 'static;

  /// An instance of a tunnel record, with any associated data
  type Record: Send + Debug;

  /// Implementation-specific errors from the registry
  type Error: Send + Debug + Display + 'static;

  /// Looks up a tunnel by name, taking the registry's chosen most-important value
  fn lookup<'a>(
    &'a self,
    tunnel_name: &'a TunnelName,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>>;

  /// Creates a registration of a tunnel name / record association within this registry's write-namespace
  /// The returned identifier allows reference to the entry created by the registry mechanism.
  ///
  /// May overwrite an existing registration, be overwritten by another,
  /// or exist simultaneously with another, depending upon the specific
  /// implementation's consistency model.
  fn register<'a>(
    &'a self,
    tunnel_name: TunnelName,
    record: &'a Self::Record,
  ) -> BoxFuture<'static, Result<Self::Identifier, Self::Error>>;

  /// Deregisters any registration of a tunnel name / record under this registry's write-namespace
  /// Note that this means that implementations will generally not allow deletion of an entry created
  /// by another origin's instances; This is purely for explicit cleanup of those it created.
  fn deregister<'a>(
    &'a self,
    tunnel_name: &'a TunnelName,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>>;

  /// Deregisters a tunnel registration by its direct, opaque identifier.
  fn deregister_identifier<'a>(
    &'a self,
    identifier: Self::Identifier,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>>;
}
impl_downcast!(sync TunnelRegistry assoc Identifier, Record, Error);

/// Provides a means of access for a value of a specified type, for use in [Attribute Registries](trait@SharedAttributeRegistry)
pub trait AttributeValue<T> {
  /// Type stored internally for the given input
  type Stored;

  fn unpack(stored: &Self::Stored) -> T;

  fn pack(input: T) -> Self::Stored;
}

/// A [`Sync`] registry of attributes retrievable [by value](trait@AttributeValue) through string keys
pub trait SharedAttributeRegistry: Downcast + DowncastSync {
  /// A value returned that uniquely addresses a registered key,
  /// such that it can be cleared without complex queries or lookup.
  ///
  /// Note that Identifiers may be irreversible to their inputs.
  type Identifier: Send + Sync + Debug + Clone + Hash;

  /// Implementation-specific errors from the registry
  type Error: Debug + Display;

  /// Looks up a key by name in the target's key-space
  ///
  /// Keys are shared between accessors, so an Identifier
  /// which can be used for deletion is returned.
  fn lookup_attr<'a, V>(
    &'a self,
    key: &'a str,
  ) -> BoxFuture<'static, Result<Option<V>, Self::Error>>;

  /// Registers an attribute to a key within the target's key-space
  ///
  /// May overwrite an existing key or be overwritten by another,
  /// depending upon the implementation's consistency model.
  fn register_attr<'a, V>(
    &'a self,
    key: &'a str,
    value: &'a V,
  ) -> BoxFuture<'static, Result<Self::Identifier, Self::Error>>;

  /// Deregisters an attribute by its key within the target's key-space
  fn deregister_attr<'a>(
    &'a self,
    key: &'a str,
  ) -> BoxFuture<'static, Result<Option<Self::Identifier>, Self::Error>>;

  /// Deregisters an attribute registration by its direct, opaque identifier.
  fn deregister_attr_identifier<'a>(
    &'a self,
    identifier: Self::Identifier,
  ) -> BoxFuture<'static, Result<Option<Self::Identifier>, Self::Error>>;
}
