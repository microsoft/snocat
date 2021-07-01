// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![forbid(unused_imports, dead_code)]
use std::{ops::Deref, sync::Arc};

use futures::{
  future::{self, BoxFuture},
  FutureExt, StreamExt, TryFutureExt, TryStreamExt,
};
use tokio_util::sync::CancellationToken;

use crate::{
  common::protocol::tunnel::{
    Sided, Tunnel, TunnelAddressInfo, TunnelDownlink, TunnelError, TunnelIncoming,
    TunnelIncomingType, TunnelSide, TunnelUplink,
  },
  util::{dropkick::Dropkick, tunnel_stream::WrappedStream},
};

use super::{TunnelControl, TunnelControlPerChannel, TunnelMonitoring, TunnelMonitoringPerChannel};

pub struct QuinnTunnel<S: quinn::crypto::Session> {
  connection: quinn::generic::Connection<S>,
  side: TunnelSide,
  incoming: Arc<tokio::sync::Mutex<TunnelIncoming>>,

  incoming_closed: Arc<Dropkick<CancellationToken>>,
  outgoing_closed: Arc<Dropkick<CancellationToken>>,
}

impl<S: quinn::crypto::Session> QuinnTunnel<S> {
  pub fn into_inner(
    self,
  ) -> (
    quinn::generic::Connection<S>,
    TunnelSide,
    Arc<tokio::sync::Mutex<TunnelIncoming>>,
  ) {
    (self.connection, self.side, self.incoming)
  }
}

impl<S> TunnelControl for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn close<'a>(&'a self) -> BoxFuture<'a, Result<(), TunnelError>> {
    self.incoming_closed.cancel();
    self.outgoing_closed.cancel();
    future::ready(Ok(())).boxed()
  }
}

impl<S> TunnelControlPerChannel for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn close_uplink<'a>(&'a self) -> BoxFuture<'a, Result<(), TunnelError>> {
    self.outgoing_closed.cancel();
    future::ready(Ok(())).boxed()
  }

  fn close_downlink<'a>(&'a self) -> BoxFuture<'a, Result<(), TunnelError>> {
    self.incoming_closed.cancel();
    future::ready(Ok(())).boxed()
  }
}

impl<S> TunnelMonitoring for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn is_closed(&self) -> bool {
    self.outgoing_closed.is_cancelled() && self.incoming_closed.is_cancelled()
  }

  fn on_closed(&'_ self) -> BoxFuture<'static, Result<(), TunnelError>> {
    let in_close = self.incoming_closed.deref().deref().clone();
    let out_close = self.outgoing_closed.deref().deref().clone();
    async move {
      future::join(in_close.cancelled(), out_close.cancelled())
        .map(|_| Ok(()))
        .await
    }
    .boxed()
  }
}

impl<S> TunnelMonitoringPerChannel for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn is_closed_uplink(&self) -> bool {
    self.outgoing_closed.is_cancelled()
  }

  fn on_closed_uplink(&'_ self) -> BoxFuture<'static, Result<(), TunnelError>> {
    let out_close = self.outgoing_closed.clone();
    async move { out_close.cancelled().map(|_| Ok(())).await }.boxed()
  }

  fn is_closed_downlink(&self) -> bool {
    self.incoming_closed.is_cancelled()
  }

  fn on_closed_downlink(&'_ self) -> BoxFuture<'static, Result<(), TunnelError>> {
    let in_close = self.incoming_closed.deref().deref().clone();
    async move { in_close.cancelled().map(|_| Ok(())).await }.boxed()
  }
}

impl<S> Sided for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn side(&self) -> TunnelSide {
    self.side
  }
}

impl<S> TunnelUplink for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream, TunnelError>> {
    if self.is_closed_uplink() {
      return future::ready(Err(TunnelError::ConnectionClosed)).boxed();
    }
    // TODO: make streams exit when close() is called
    self
      .connection
      .open_bi()
      .map(|result| match result {
        Ok((send, recv)) => Ok(WrappedStream::Boxed(Box::new(recv), Box::new(send))),
        Err(e) => Err(e.into()),
      })
      .inspect_err({
        // Clone the dropkick arc to ensure that it is not marked closed
        // until the [Tunnel], its downlink, and all its uplinks are dropped.
        let canceller = self.outgoing_closed.clone();
        move |_tunnel_error| {
          // TODO: set closed reason, once a place exists to set such a thing
          canceller.cancel();
        }
      })
      .boxed()
  }

  fn addr(&self) -> TunnelAddressInfo {
    TunnelAddressInfo::Socket(self.connection.remote_address())
  }
}

impl<S> Tunnel for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn downlink<'a>(&'a self) -> BoxFuture<'a, Option<Box<dyn TunnelDownlink + Send + Unpin>>> {
    if self.is_closed_downlink() {
      return future::ready(None).boxed();
    }
    // [TunnelIncoming] is constructed upon opening the tunnel
    // Logic to cut it upon and after downlink closure is handled at time of construction
    self
      .incoming
      .clone()
      .lock_owned()
      .map(|x| Some(Box::new(x) as Box<_>))
      .boxed()
  }
}

impl From<quinn::ConnectionError> for TunnelError {
  fn from(connection_error: quinn::ConnectionError) -> Self {
    match connection_error {
      quinn::ConnectionError::VersionMismatch => Self::TransportError,
      quinn::ConnectionError::TransportError(_) => Self::TransportError,
      quinn::ConnectionError::ConnectionClosed(_) => Self::ConnectionClosed,
      quinn::ConnectionError::ApplicationClosed(_) => Self::ApplicationClosed,
      quinn::ConnectionError::Reset => Self::TransportError,
      quinn::ConnectionError::TimedOut => Self::TimedOut,
      quinn::ConnectionError::LocallyClosed => Self::LocallyClosed,
    }
  }
}

pub fn from_quinn_endpoint<S>(
  new_connection: quinn::generic::NewConnection<S>,
  side: TunnelSide,
) -> QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  let quinn::generic::NewConnection {
    connection,
    bi_streams,
    ..
  } = new_connection;
  // Incoming Cancellation is used for is_closed_downlink later
  // We need to use it earlier to prep the incoming stream.
  let incoming_cancellation: Arc<Dropkick<CancellationToken>> =
    Arc::new(CancellationToken::new().into());
  let stream_tunnels = bi_streams
    .map_ok(|(send, recv)| {
      // TODO: make incoming streams exit when close() is called
      TunnelIncomingType::BiStream(WrappedStream::Boxed(Box::new(recv), Box::new(send)))
    })
    .map_err(Into::into)
    // Only take new streams until incoming is cancelled
    .take_until({
      // Copy a cancellation token instance which is used to cut the incoming channel
      // We only need one clone of it because downlinks are exclusively held via lock
      // We clone the dropkick arc to ensure that it is not marked closed
      // until the [Tunnel], its downlink, and all its uplinks are dropped.
      let incoming_cancellation = incoming_cancellation.clone();
      // Run in a separate task to ensure that we drop the arc on cancellation even if
      // nobody awaits the downlink's cancellation event.
      tokio::task::spawn({
        async move {
          incoming_cancellation.cancelled().await;
          drop(incoming_cancellation);
        }
      })
    })
    .inspect_err({
      let incoming_cancellation = incoming_cancellation.deref().deref().clone();
      move |_tunnel_error| {
        // TODO: set closed reason, once a place exists to set such a thing
        incoming_cancellation.cancel();
      }
    })
    .fuse()
    .boxed();
  QuinnTunnel {
    connection,
    side,
    incoming: Arc::new(tokio::sync::Mutex::new(TunnelIncoming {
      inner: stream_tunnels,
      side,
    })),
    incoming_closed: incoming_cancellation,
    outgoing_closed: Arc::new(CancellationToken::new().into()),
  }
}
