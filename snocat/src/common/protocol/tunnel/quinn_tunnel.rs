// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![forbid(unused_imports, dead_code)]
use std::sync::Arc;

use futures::{future::BoxFuture, FutureExt, StreamExt, TryStreamExt};

use crate::{
  common::protocol::tunnel::{
    Sided, Tunnel, TunnelAddressInfo, TunnelDownlink, TunnelError, TunnelIncoming,
    TunnelIncomingType, TunnelSide, TunnelUplink,
  },
  util::tunnel_stream::WrappedStream,
};

pub struct QuinnTunnel<S: quinn::crypto::Session> {
  connection: quinn::generic::Connection<S>,
  side: TunnelSide,
  incoming: Arc<tokio::sync::Mutex<TunnelIncoming>>,
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
  let stream_tunnels = bi_streams
    .map_ok(|(send, recv)| {
      TunnelIncomingType::BiStream(WrappedStream::Boxed(Box::new(recv), Box::new(send)))
    })
    .map_err(Into::into)
    .boxed();
  QuinnTunnel {
    connection,
    side,
    incoming: Arc::new(tokio::sync::Mutex::new(TunnelIncoming {
      inner: stream_tunnels,
      side,
    })),
  }
}
