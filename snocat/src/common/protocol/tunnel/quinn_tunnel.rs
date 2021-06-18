// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![forbid(unused_imports, dead_code)]
use std::sync::Arc;

use futures::{
  future::{self, BoxFuture},
  stream, FutureExt, StreamExt, TryStreamExt,
};
use tokio::sync::watch;

use crate::{
  common::protocol::tunnel::{
    Sided, Tunnel, TunnelAddressInfo, TunnelDownlink, TunnelError, TunnelIncoming,
    TunnelIncomingType, TunnelSide, TunnelUplink,
  },
  util::tunnel_stream::WrappedStream,
};

use super::{TunnelControl, TunnelMonitoring};

pub struct QuinnTunnel<S: quinn::crypto::Session> {
  connection: quinn::generic::Connection<S>,
  side: TunnelSide,
  incoming: Arc<tokio::sync::Mutex<TunnelIncoming>>,

  closed: (watch::Sender<bool>, watch::Receiver<bool>),
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
    let closed = *self.closed.0.borrow();
    future::ready(if !closed {
      self
        .closed
        .0
        .send(true)
        .map_err(|_| TunnelError::ConnectionClosed)
    } else {
      Ok(())
    })
    .boxed()
  }
}

impl<S> TunnelMonitoring for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn is_closed(&self) -> bool {
    *self.closed.0.borrow()
  }

  fn on_closed(&'_ self) -> BoxFuture<'static, Result<(), TunnelError>> {
    let mut closed = self.closed.1.clone();
    async move {
      let is_closed = *closed.borrow();
      if !is_closed {
        drop(closed.changed().await);
      }
      Ok(())
    }
    .boxed()
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
    if self.is_closed() {
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
  // TODO: make incoming streams exit when close() is called
  let stream_tunnels = bi_streams
    .map_ok(|(send, recv)| {
      TunnelIncomingType::BiStream(WrappedStream::Boxed(Box::new(recv), Box::new(send)))
    })
    .map_err(Into::into)
    .boxed();
  let mut tunnel = QuinnTunnel {
    connection,
    side,
    incoming: Arc::new(tokio::sync::Mutex::new(TunnelIncoming {
      inner: stream_tunnels,
      side,
    })),
    closed: watch::channel(false),
  };
  // Close stream of incoming streams when we close locally
  // Getting the closed event before creating the object is hard.
  // Swap it while we still own it exclusively, using a placeholder pending() stream temporarily
  {
    let closed_future = tunnel.on_closed();
    let incoming = Arc::get_mut(&mut tunnel.incoming)
      .expect("Must have sole ownership still")
      .get_mut();
    let box_of_nothing = stream::pending().boxed();
    let incoming_streams = std::mem::replace(&mut incoming.inner, box_of_nothing);
    incoming.inner = incoming_streams.take_until(closed_future).boxed();
  }
  tunnel
}
