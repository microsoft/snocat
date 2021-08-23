// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use std::{
  borrow::{Borrow, BorrowMut},
  net::SocketAddr,
  ops::Deref,
  pin::Pin,
  sync::Arc,
};

use crate::util::tunnel_stream::WrappedStream;
use futures::{
  future::{BoxFuture, Either},
  stream::{BoxStream, LocalBoxStream, Stream, StreamFuture, TryStreamExt},
  Future, FutureExt, StreamExt,
};
use quinn::{crypto::Session, generic::RecvStream, ApplicationClose, SendStream};
use serde::{Deserializer, Serializer};
use tokio::{
  io::{AsyncRead, AsyncWrite},
  sync::{
    broadcast,
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot, OwnedMutexGuard,
  },
};

pub mod duplex;
pub mod id;
pub mod quinn_tunnel;
pub mod registry;

pub use self::id::TunnelId;
pub type BoxedTunnel<'a> = Box<dyn Tunnel + Send + Sync + Unpin + 'a>;
pub type ArcTunnel<'a> = Arc<dyn Tunnel + Send + Sync + Unpin + 'a>;

/// A name for an Snocat tunnel, used to identify its connection in [`TunnelServerEvent`]s.
#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone)]
#[repr(transparent)]
pub struct TunnelName(Arc<String>);

impl serde::Serialize for TunnelName {
  fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&self.0)
  }
}
impl<'de> serde::de::Deserialize<'de> for TunnelName {
  fn deserialize<D>(deserializer: D) -> Result<Self, <D as Deserializer<'de>>::Error>
  where
    D: Deserializer<'de>,
  {
    let s: String = serde::Deserialize::deserialize(deserializer)?;
    Ok(TunnelName::new(s))
  }
}

impl TunnelName {
  pub fn new<T: std::convert::Into<String>>(t: T) -> TunnelName {
    TunnelName(t.into().into())
  }

  pub fn raw(&self) -> &str {
    &self.0
  }
}

impl Into<String> for TunnelName {
  fn into(self) -> String {
    self.0.as_ref().clone()
  }
}

impl std::fmt::Debug for TunnelName {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Snocat").field("Id", &self.0).finish()
  }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Hash)]
pub enum TunnelNameOrId {
  Name(TunnelName),
  Id(TunnelId),
}

impl From<&TunnelId> for TunnelNameOrId {
  fn from(id: &TunnelId) -> Self {
    TunnelNameOrId::Id(*id)
  }
}

impl From<TunnelId> for TunnelNameOrId {
  fn from(id: TunnelId) -> Self {
    TunnelNameOrId::Id(id)
  }
}

impl From<TunnelName> for TunnelNameOrId {
  fn from(name: TunnelName) -> Self {
    TunnelNameOrId::Name(name)
  }
}

impl From<&TunnelName> for TunnelNameOrId {
  fn from(name: &TunnelName) -> Self {
    TunnelNameOrId::Name(name.clone())
  }
}

impl From<TunnelNameOrId> for Option<TunnelId> {
  fn from(name_or_id: TunnelNameOrId) -> Self {
    match name_or_id {
      TunnelNameOrId::Id(id) => Some(id),
      _ => None,
    }
  }
}

impl From<TunnelNameOrId> for Option<TunnelName> {
  fn from(name_or_id: TunnelNameOrId) -> Self {
    match name_or_id {
      TunnelNameOrId::Name(name) => Some(name),
      _ => None,
    }
  }
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum TunnelError {
  #[error("Connection closed")]
  ConnectionClosed,
  #[error("Connection closed by application")]
  ApplicationClosed,
  #[error("Connection timed out")]
  TimedOut,
  #[error("Transport error encountered")]
  TransportError,
  #[error("Connection closed locally")]
  LocallyClosed,
}

#[derive(Debug, Copy, Clone)]
pub enum TunnelSide {
  Connect,
  Listen,
}

#[derive(Debug, Clone)]
pub enum TunnelAddressInfo {
  Unidentified,
  Socket(SocketAddr),
  Port(u16),
}

impl std::string::ToString for TunnelAddressInfo {
  fn to_string(&self) -> String {
    match self {
      Self::Unidentified => String::from("Unidentified"),
      Self::Socket(socket_addr) => socket_addr.to_string(),
      Self::Port(port) => port.to_string(),
    }
  }
}

pub trait TunnelMonitoring {
  /// If the tunnel is currently closed on uplink and downlink
  fn is_closed(&self) -> bool; // May need to be async for implementation practicality and to avoid blocking

  /// Notifies when the tunnel is closed both in uplink and downlink, and if it was due to an error
  fn on_closed(&'_ self) -> BoxFuture<'static, Result<(), TunnelError>>;
}

pub trait TunnelMonitoringPerChannel: TunnelMonitoring {
  /// If the tunnel is currently closed on its uplink
  fn is_closed_uplink(&self) -> bool; // May need to be async for implementation practicality and to avoid blocking

  /// Notifies when the uplink is closed, and if it was due to an error
  fn on_closed_uplink(&'_ self) -> BoxFuture<'static, Result<(), TunnelError>>;

  /// If the tunnel is currently closed on its downlink
  fn is_closed_downlink(&self) -> bool; // May need to be async for implementation practicality and to avoid blocking

  /// Notifies when the downlink is closed, and if it was due to an error
  fn on_closed_downlink(&'_ self) -> BoxFuture<'static, Result<(), TunnelError>>;
}

pub trait TunnelActivityMonitoring {
  /// Allows monitoring for incoming stream creation and completion.
  ///
  /// Upon creation of an incoming stream, provides the current tunnel ID and a
  /// oneshot which fulfills upon closure of that stream, or fails to fulfill
  /// due to remote closure if the tunnel is dropped prior to completion.
  fn on_new_incoming_stream<'a>(&'a self) -> BoxStream<'a, BoxFuture<'static, Result<(), ()>>>;

  /// Allows monitoring for incoming stream creation and completion.
  ///
  /// Upon push of an outgoing stream, provides the current tunnel ID and a
  /// oneshot which fulfills upon closure of that stream, or fails to fulfill
  /// due to remote closure if the tunnel is dropped prior to completion.
  fn on_new_outgoing_stream<'a>(&'a self) -> BoxStream<'a, BoxFuture<'static, Result<(), ()>>>;

  /// Gets the current number of active streams
  ///
  /// Implementation is allowed to block creation of new streams while reading.
  fn active_stream_count(&self) -> usize;

  /// Track the number of activee streams in the session, on uplink and downlink
  fn on_active_stream_count_changed<'a>(&'a self) -> BoxStream<'a, usize> {
    use tokio::sync::watch;
    // Yes- this looks redundant- but it keeps the math simple with baseline of 1
    let counter_holder = Arc::new(Arc::new(()));
    let baseline_count = 1; // strong_count is the number of counters present when true count is zero
    let update_current_count = move |notifier: &watch::Sender<usize>, counter: &Arc<()>| {
      let current_count = Arc::strong_count(&counter) - baseline_count;
      let _ = notifier.send(current_count);
    };
    let (send, recv) = watch::channel(0);
    let send = Arc::new(send);

    // Future which produces updates when subscribed to incoming stream notifications; resolves when stream ends
    let counter_incoming = {
      let send = Arc::clone(&send);
      let counter_holder = Arc::clone(&counter_holder);
      let mut source_filtered_empty = self
        .on_new_incoming_stream()
        .map(move |on_close| {
          (
            Arc::clone(&counter_holder),
            Arc::clone(counter_holder.as_ref()),
            on_close,
          )
        })
        .then(move |(activity_counter_ref, counter_handle, on_close)| {
          let send = Arc::clone(&send);
          update_current_count(send.as_ref(), &*activity_counter_ref);
          async move {
            let _close_result = on_close.await;
            drop(counter_handle);
            update_current_count(send.as_ref(), &*activity_counter_ref);
            ()
          }
        })
        .filter(|_| futures::future::ready(false))
        .boxed();

      async move { source_filtered_empty.next().await }.boxed()
    };

    // Future which produces updates when subscribed to outgoing stream notifications; resolves when stream ends
    let counter_outgoing = {
      let send = Arc::clone(&send);
      let counter_holder = Arc::clone(&counter_holder);
      let mut source_filtered_empty = self
        .on_new_outgoing_stream()
        .map(move |on_close| {
          (
            Arc::clone(&counter_holder),
            Arc::clone(counter_holder.as_ref()),
            on_close,
          )
        })
        .then(move |(activity_counter_ref, counter_handle, on_close)| {
          let send = Arc::clone(&send);
          update_current_count(send.as_ref(), &*activity_counter_ref);
          async move {
            let _close_result = on_close.await;
            drop(counter_handle);
            update_current_count(send.as_ref(), &*activity_counter_ref);
            ()
          }
        })
        .filter(|_| futures::future::ready(false))
        .boxed();

      async move { source_filtered_empty.next().await }.boxed()
    };

    drop(counter_holder);

    tokio_stream::wrappers::WatchStream::new(recv)
      .take_until(futures::future::join(counter_incoming, counter_outgoing))
      .boxed()
  }
}

pub trait TunnelControl {
  fn close<'a>(&'a self) -> BoxFuture<'a, Result<(), TunnelError>>;
}

pub trait TunnelControlPerChannel: TunnelControl {
  fn close_uplink<'a>(&'a self) -> BoxFuture<'a, Result<(), TunnelError>>;
  fn close_downlink<'a>(&'a self) -> BoxFuture<'a, Result<(), TunnelError>>;
}

/// Provides access to a shared data structure bound to the object
///
/// Lifetimes of the baggage and its children are bound to that of the parent
pub trait Baggage {
  type Bag<'a>;

  fn bag<'a>(&'a self) -> Self::Bag<'a>;
}

pub trait WithTunnelId {
  fn id(&self) -> &TunnelId;
}

impl<T: std::ops::Deref> WithTunnelId for T
where
  T::Target: WithTunnelId,
{
  fn id(&self) -> &TunnelId {
    self.deref().id()
  }
}

pub trait Sided {
  fn side(&self) -> TunnelSide;
}

impl<T: std::ops::Deref> Sided for T
where
  T::Target: Sided,
{
  fn side(&self) -> TunnelSide {
    self.deref().side()
  }
}

pub trait TunnelUplink: WithTunnelId + Sided {
  fn addr(&self) -> TunnelAddressInfo {
    TunnelAddressInfo::Unidentified
  }

  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream, TunnelError>>;
}

impl<T> TunnelUplink for T
where
  T: Deref + Send + Sync + Unpin,
  <T as Deref>::Target: TunnelUplink + Sided,
{
  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream, TunnelError>> {
    self.deref().open_link()
  }
}

pub trait TunnelDownlink: WithTunnelId + Sided {
  fn as_stream<'a>(&'a mut self) -> BoxStream<'a, Result<TunnelIncomingType, TunnelError>>;
}

impl<TDownlink: std::ops::Deref + std::ops::DerefMut> TunnelDownlink for TDownlink
where
  TDownlink::Target: TunnelDownlink,
{
  fn as_stream<'a>(&'a mut self) -> BoxStream<'a, Result<TunnelIncomingType, TunnelError>> {
    self.deref_mut().as_stream()
  }
}

pub trait Tunnel: WithTunnelId + TunnelUplink + Send + Sync + Unpin {
  fn downlink<'a>(&'a self) -> BoxFuture<'a, Option<Box<dyn TunnelDownlink + Send + Unpin>>>;
}

impl<T> Tunnel for T
where
  T: Deref + Send + Sync + Unpin,
  <T as Deref>::Target: Tunnel + TunnelUplink + Sided,
{
  fn downlink<'a>(&'a self) -> BoxFuture<'a, Option<Box<dyn TunnelDownlink + Send + Unpin>>> {
    self.deref().downlink()
  }
}

/// Creates a tunnel with the provided ID
///
/// Compliant implementations must use the provided ID, and must remain
/// stable throughout the lifetime of the resulting tunnel instance
pub trait AssignTunnelId<TTunnel>
where
  TTunnel: WithTunnelId,
{
  fn assign_tunnel_id(self, tunnel_id: TunnelId) -> TTunnel;
}

impl<TTunnel, TAssignTunnelId: AssignTunnelId<TTunnel>> AssignTunnelId<Box<TTunnel>>
  for TAssignTunnelId
where
  TTunnel: WithTunnelId,
{
  fn assign_tunnel_id(self, tunnel_id: TunnelId) -> Box<TTunnel> {
    Box::new(TAssignTunnelId::assign_tunnel_id(self, tunnel_id))
  }
}

impl<TTunnel, TAssignTunnelId: AssignTunnelId<TTunnel>> AssignTunnelId<Arc<TTunnel>>
  for TAssignTunnelId
where
  TTunnel: WithTunnelId,
{
  fn assign_tunnel_id(self, tunnel_id: TunnelId) -> Arc<TTunnel> {
    Arc::new(TAssignTunnelId::assign_tunnel_id(self, tunnel_id))
  }
}

// /// Creates a tunnel with the provided ID
// ///
// /// Compliant implementations must use the provided ID, and must remain
// /// stable throughout the lifetime of the resulting tunnel instance
// pub trait FromTunnelComponents<TComponents> : Tunnel + WithTunnelId {
//   fn assign_tunnel_id(tunnel_id: TunnelId, components: TComponents) -> Self;
// }

// /// Any tunnel type which can be infallibly converted from the result of
// /// another tunnel assignment method on the same inputs is an assignment method.
// impl<TComponents, T: FromTunnelComponents<TComponents>, TOut> FromTunnelComponents<TComponents> for TOut
// where
//   T: Into<TOut>,
// {
//   fn assign_tunnel_id(tunnel_id: TunnelId, components: TComponents) -> TOut {
//     let inner = T::assign_tunnel_id(components, tunnel_id);
//     inner.into()
//   }
// }

pub enum TunnelIncomingType {
  BiStream(WrappedStream),
}

pub struct TunnelIncoming {
  id: TunnelId,
  inner: BoxStream<'static, Result<TunnelIncomingType, TunnelError>>,
  side: TunnelSide,
}

impl std::fmt::Debug for TunnelIncoming {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TunnelIncoming")
      .field("id", &self.id)
      .field("side", &self.side)
      .finish_non_exhaustive()
  }
}

impl TunnelIncoming {
  pub fn id(&self) -> &TunnelId {
    &self.id
  }

  pub fn side(&self) -> TunnelSide {
    self.side
  }

  pub fn streams(self) -> BoxStream<'static, Result<TunnelIncomingType, TunnelError>> {
    self.inner
  }

  pub fn streams_ref<'a>(&'a mut self) -> BoxStream<'a, Result<TunnelIncomingType, TunnelError>> {
    self.inner.borrow_mut().boxed()
  }
}

impl WithTunnelId for TunnelIncoming {
  fn id(&self) -> &TunnelId {
    &self.id
  }
}

impl Sided for TunnelIncoming {
  fn side(&self) -> TunnelSide {
    self.side
  }
}

impl TunnelDownlink for TunnelIncoming {
  fn as_stream<'a>(&'a mut self) -> BoxStream<'a, Result<TunnelIncomingType, TunnelError>> {
    self.inner.borrow_mut().boxed()
  }
}

#[cfg(test)]
mod tests {}
