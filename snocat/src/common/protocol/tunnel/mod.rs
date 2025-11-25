// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

#![warn(unused_imports, dead_code, unused_variables)]

use std::{
  borrow::BorrowMut,
  net::SocketAddr,
  ops::{Deref, DerefMut},
  sync::Arc,
};

use futures::{future::BoxFuture, stream::BoxStream, StreamExt};
use serde::{Deserializer, Serializer};

use crate::{ext::stream::StreamExtExt, util::tunnel_stream::WrappedStream};

pub mod duplex;
pub mod id;
pub mod quinn_tunnel;
pub mod registry;

pub use self::id::TunnelId;
pub type BoxedTunnel<'a> = Box<dyn Tunnel + Send + Sync + Unpin + 'a>;
pub type ArcTunnel<'a> = Arc<dyn Tunnel + Send + Sync + Unpin + 'a>;

pub mod prelude {
  pub use super::{
    ArcTunnel, BoxedTunnel, Sided, Tunnel, TunnelActivityMonitoring, TunnelDownlink, TunnelId,
    TunnelIncoming, TunnelMonitoring, TunnelMonitoringPerChannel, TunnelUplink,
  };
}

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

#[derive(thiserror::Error, Debug, Clone)]
pub enum TunnelCloseReason {
  #[error(
    "Tunnel closed gracefully - initiator: {}",
    if *(.remote_initiated) { "remote" } else { "local" },
  )]
  GracefulExit {
    /// Marks that the remote was or was not the initiator for the exit
    remote_initiated: bool,
  },
  #[error(
    "Tunnel failed authentication - responsibility: {}",
    match .remote_responsible {
      Some(true) => "remote",
      Some(false) => "local",
      None => "unknown"
    },
  )]
  AuthenticationFailure {
    /// Marks that the remote was or was not responsible; None indicates unspecified or unknown.
    remote_responsible: Option<bool>,
  },
  #[error("Tunnel closed due to error: {0}")]
  Error(
    #[from]
    #[source]
    #[cfg_attr(feature = "backtrace", backtrace)]
    TunnelError,
  ),
  #[error("Tunnel closed due to application error: {0}")]
  ApplicationError(
    #[from]
    #[source]
    #[cfg_attr(feature = "backtrace", backtrace)]
    Arc<dyn std::error::Error + Send + Sync + 'static>,
  ),
  #[error("Tunnel closed due to application error message: {0}")]
  ApplicationErrorMessage(Arc<String>),
  #[error("Tunnel closed without indication of reason")]
  Unspecified,
}

impl TunnelCloseReason {
  /// Returns `true` if the tunnel close reason is [`Unspecified`].
  ///
  /// [`Unspecified`]: TunnelCloseReason::Unspecified
  #[must_use]
  pub fn is_unspecified(&self) -> bool {
    matches!(self, Self::Unspecified)
  }

  /// Returns `true` if the tunnel close reason is [`GracefulExit`].
  ///
  /// [`GracefulExit`]: TunnelCloseReason::GracefulExit
  #[must_use]
  pub fn is_graceful_exit(&self) -> bool {
    matches!(self, Self::GracefulExit { .. })
  }
}

pub trait TunnelMonitoring {
  /// If the tunnel is currently closed on uplink and downlink
  fn is_closed(&self) -> bool;

  /// Notifies when the tunnel is closed both in uplink and downlink, and if it was due to an error
  fn on_closed(&'_ self) -> BoxFuture<'static, Arc<TunnelCloseReason>>;

  /// Notifies when authentication has completed, or when the tunnel has been closed
  fn on_authenticated(&'_ self) -> BoxFuture<'static, Result<TunnelName, Arc<TunnelCloseReason>>>;
}

pub trait TunnelMonitoringPerChannel: TunnelMonitoring {
  /// If the tunnel is currently closed on its uplink
  fn is_closed_uplink(&self) -> bool; // May need to be async for implementation practicality and to avoid blocking

  /// Notifies when the uplink is closed, and if it was due to an error
  fn on_closed_uplink(&'_ self) -> BoxFuture<'static, Arc<TunnelCloseReason>>;

  /// If the tunnel is currently closed on its downlink
  fn is_closed_downlink(&self) -> bool; // May need to be async for implementation practicality and to avoid blocking

  /// Notifies when the downlink is closed, and if it was due to an error
  fn on_closed_downlink(&'_ self) -> BoxFuture<'static, Arc<TunnelCloseReason>>;
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
    let (send, recv) = watch::channel(0usize);
    // Future which produces updates when subscribed to incoming stream notifications; resolves when stream ends
    let incoming = self.on_new_incoming_stream().boxed();
    // Future which produces updates when subscribed to outgoing stream notifications; resolves when stream ends
    let outgoing = self.on_new_outgoing_stream().boxed();
    // `StreamExtExt::try_for_each_concurrent_monitored` cannot (currently) share
    // counters, so we combine the streams to create a shared count instead.
    // Merge both streams, and map them with Ok(item) to make them a TryStream, to fit try_for_each* signatures
    let combined = futures::stream::select(incoming, outgoing)
      .map(|item| Result::<_, ()>::Ok(item))
      .boxed();
    // The callback is simply a way to ignore failures, since we actually don't care about the streams' success/failure state
    let sender = combined.try_for_each_concurrent_monitored(
      None,
      send,
      |f: BoxFuture<'_, Result<(), ()>>| async move {
        tokio::task::spawn(f).await.ok();
        Ok(())
      },
    );

    tokio_stream::wrappers::WatchStream::new(recv)
      .take_until(sender) // Run the updater that pushes to `send` as long as `recv` is watched
      .boxed()
  }
}

pub trait TunnelControl {
  /// Fails if the tunnel was already marked as closed with a specified reason- returning the reason for that closure,
  fn close<'a>(
    &'a self,
    reason: TunnelCloseReason,
  ) -> BoxFuture<'a, Result<Arc<TunnelCloseReason>, Arc<TunnelCloseReason>>>;

  /// Marks the tunnel as authenticated; Fails if the tunnel was already closed or marked as authenticated.
  fn report_authentication_success<'a>(
    &self,
    tunnel_name: TunnelName,
  ) -> BoxFuture<'a, Result<(), Option<Arc<TunnelCloseReason>>>>;
}

impl<T> TunnelControl for T
where
  T: Deref + Send + Sync + Unpin,
  <T as Deref>::Target: TunnelControl,
{
  fn close<'a>(
    &'a self,
    reason: TunnelCloseReason,
  ) -> BoxFuture<'a, Result<Arc<TunnelCloseReason>, Arc<TunnelCloseReason>>> {
    self.deref().close(reason)
  }

  fn report_authentication_success<'a>(
    &self,
    tunnel_name: TunnelName,
  ) -> BoxFuture<'a, Result<(), Option<Arc<TunnelCloseReason>>>> {
    self.deref().report_authentication_success(tunnel_name)
  }
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
  fn addr(&self) -> TunnelAddressInfo {
    self.deref().addr()
  }

  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream, TunnelError>> {
    self.deref().open_link()
  }
}

pub trait TunnelDownlink: WithTunnelId + Sided {
  fn as_stream<'a>(&'a mut self) -> BoxStream<'a, Result<TunnelIncomingType, TunnelError>>;
}

impl<TDownlink> TunnelDownlink for TDownlink
where
  TDownlink: Deref + DerefMut,
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

/// Shows that a type may be converted into a [Tunnel] when given a [TunnelId].
///
/// Compliant implementations must use the provided ID, which must remain
/// stable throughout the lifetime of the resulting tunnel instance.
pub trait IntoTunnel {
  type Tunnel: WithTunnelId;
  fn into_tunnel(self, tunnel_id: TunnelId) -> Self::Tunnel;
}

impl<Params> IntoTunnel for Box<Params>
where
  Params: IntoTunnel,
{
  type Tunnel = Params::Tunnel;
  fn into_tunnel(self, tunnel_id: TunnelId) -> Self::Tunnel {
    <Params as IntoTunnel>::into_tunnel(*self, tunnel_id)
  }
}

mod transforming_tunnel_constructors {
  use ::std::{rc::Rc, sync::Arc};
  use std::ops::{Deref, DerefMut};

  use super::{IntoTunnel, TunnelId};

  /// Transforms the constructed tunnel into a box of that tunnel
  #[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
  #[repr(transparent)]
  pub struct IntoBoxedTunnel<Params>(pub Params);
  /// Transforms the constructed tunnel into an Rc of that tunnel
  #[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
  #[repr(transparent)]
  pub struct IntoRcTunnel<Params>(pub Params);
  /// Transforms the constructed tunnel into an Arc of that tunnel
  #[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
  #[repr(transparent)]
  pub struct IntoArcTunnel<Params>(pub Params);

  impl<Params> IntoBoxedTunnel<Params> {
    fn into_inner(self) -> Params {
      self.0
    }
  }

  impl<Params> Deref for IntoBoxedTunnel<Params> {
    type Target = Params;

    fn deref(&self) -> &Self::Target {
      &self.0
    }
  }

  impl<Params> DerefMut for IntoBoxedTunnel<Params> {
    fn deref_mut(&mut self) -> &mut Self::Target {
      &mut self.0
    }
  }

  impl<Params> IntoRcTunnel<Params> {
    fn into_inner(self) -> Params {
      self.0
    }
  }

  impl<Params> Deref for IntoRcTunnel<Params> {
    type Target = Params;

    fn deref(&self) -> &Self::Target {
      &self.0
    }
  }

  impl<Params> DerefMut for IntoRcTunnel<Params> {
    fn deref_mut(&mut self) -> &mut Self::Target {
      &mut self.0
    }
  }

  impl<Params> IntoArcTunnel<Params> {
    fn into_inner(self) -> Params {
      self.0
    }
  }

  impl<Params> Deref for IntoArcTunnel<Params> {
    type Target = Params;

    fn deref(&self) -> &Self::Target {
      &self.0
    }
  }

  impl<Params> DerefMut for IntoArcTunnel<Params> {
    fn deref_mut(&mut self) -> &mut Self::Target {
      &mut self.0
    }
  }

  impl<Params> IntoTunnel for IntoBoxedTunnel<Params>
  where
    Params: IntoTunnel,
  {
    type Tunnel = Box<Params::Tunnel>;
    fn into_tunnel(self, tunnel_id: TunnelId) -> Self::Tunnel {
      Box::new(<Params as IntoTunnel>::into_tunnel(
        self.into_inner(),
        tunnel_id,
      ))
    }
  }

  impl<Params> IntoTunnel for IntoRcTunnel<Params>
  where
    Params: IntoTunnel,
  {
    type Tunnel = Rc<Params::Tunnel>;
    fn into_tunnel(self, tunnel_id: TunnelId) -> Self::Tunnel {
      Rc::new(<Params as IntoTunnel>::into_tunnel(
        self.into_inner(),
        tunnel_id,
      ))
    }
  }

  impl<Params> IntoTunnel for IntoArcTunnel<Params>
  where
    Params: IntoTunnel,
  {
    type Tunnel = Arc<Params::Tunnel>;
    fn into_tunnel(self, tunnel_id: TunnelId) -> Self::Tunnel {
      Arc::new(<Params as IntoTunnel>::into_tunnel(
        self.into_inner(),
        tunnel_id,
      ))
    }
  }
}

pub use transforming_tunnel_constructors::{IntoArcTunnel, IntoBoxedTunnel, IntoRcTunnel};

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
