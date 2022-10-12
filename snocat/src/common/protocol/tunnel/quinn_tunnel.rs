// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![deny(unused_imports, dead_code)]
use std::sync::Arc;

use arc_swap::ArcSwap;
use futures::{
  future::{self, BoxFuture},
  FutureExt, StreamExt, TryFutureExt, TryStreamExt,
};
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tokio_util::sync::CancellationToken;

use crate::{
  common::protocol::tunnel::{
    Sided, Tunnel, TunnelAddressInfo, TunnelDownlink, TunnelError, TunnelIncoming,
    TunnelIncomingType, TunnelSide, TunnelUplink,
  },
  ext::future::FutureExtExt,
  util::{cancellation::CancellationListener, dropkick::Dropkick, tunnel_stream::WrappedStream},
};

use super::{
  AssignTunnelId, Baggage, TunnelCloseReason, TunnelControl, TunnelId, TunnelMonitoring,
  TunnelMonitoringPerChannel, TunnelName, WithTunnelId,
};

pub struct QuinnTunnel<B = ()> {
  id: TunnelId,
  connection: quinn::Connection,
  side: TunnelSide,
  incoming: Arc<tokio::sync::Mutex<TunnelIncoming>>,

  closed: Arc<Dropkick<CancellationToken>>,
  incoming_closed: Arc<Dropkick<CancellationToken>>,
  outgoing_closed: Arc<Dropkick<CancellationToken>>,
  authenticated: Arc<tokio::sync::RwLock<Option<TunnelName>>>,
  authenticated_notifier: Arc<watch::Sender<Option<TunnelName>>>,
  close_reason: Arc<ArcSwap<TunnelCloseReason>>,

  baggage: Arc<B>,
}

impl<Baggage> std::fmt::Debug for QuinnTunnel<Baggage> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("QuinnTunnel")
      .field("id", &self.id)
      .field("side", &self.side)
      .field("closed", &self.incoming_closed)
      .field("incoming_closed", &self.incoming_closed)
      .field("outgoing_closed", &self.outgoing_closed)
      .finish_non_exhaustive()
  }
}

impl<B> QuinnTunnel<B> {
  pub fn into_inner(
    self,
  ) -> (
    TunnelId,
    quinn::Connection,
    TunnelSide,
    Arc<tokio::sync::Mutex<TunnelIncoming>>,
  ) {
    (self.id, self.connection, self.side, self.incoming)
  }

  pub fn from_quinn_connection_with_baggage(
    id: TunnelId,
    new_connection: quinn::NewConnection,
    side: TunnelSide,
    baggage: B,
  ) -> QuinnTunnel<B> {
    let quinn::NewConnection {
      connection,
      bi_streams,
      ..
    } = new_connection;

    let overall_cancellation: Arc<Dropkick<CancellationToken>> =
      Arc::new(CancellationToken::new().into());
    // Single-stream cancellations are derived from the full-cancellation token,
    // and are used for is_closed_downlink / is_closed_uplink later.
    //
    // Additionally, a task is created which joins the two of them
    // to close the common canceller when both are completed.
    let incoming_cancellation: Arc<Dropkick<CancellationToken>> =
      Arc::new(overall_cancellation.child_token().into());
    let outgoing_cancellation: Arc<Dropkick<CancellationToken>> =
      Arc::new(overall_cancellation.child_token().into());
    {
      let incoming_cancellation = CancellationListener::from(&**incoming_cancellation);
      let outgoing_cancellation = CancellationListener::from(&**outgoing_cancellation);
      let overall_cancellation = overall_cancellation.clone();
      // Automatically close the common canceller if both channels are closed.
      tokio::task::spawn(async move {
        future::join(
          incoming_cancellation.cancelled(),
          outgoing_cancellation.cancelled(),
        )
        .await;
        tokio::task::yield_now().await;
        if !overall_cancellation.is_cancelled() {
          overall_cancellation.cancel();
        }
      });
    }
    let close_reason = Arc::new(ArcSwap::new(Arc::new(TunnelCloseReason::Unspecified)));
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
        // until the [Tunnel], its downlink, and all of its uplinks are dropped.
        let incoming_cancellation = incoming_cancellation.clone();
        // Run in a separate task to ensure that we drop the arc on cancellation even if
        // nobody awaits the downlink's cancellation event.
        async move {
          // Cut the channel when the cancellation token is invoked
          incoming_cancellation.cancelled().await;
        }
      })
      .inspect_err({
        let incoming_cancellation = CancellationToken::clone(&incoming_cancellation);
        let close_reason_store = Arc::clone(&close_reason);
        move |_tunnel_error| {
          let close_reason = TunnelCloseReason::Error(TunnelError::ConnectionClosed);
          {
            let close_reason_store = &close_reason_store;
            close_reason_store.store(Arc::new(close_reason));
          };
          if !incoming_cancellation.is_cancelled() {
            incoming_cancellation.cancel();
          }
        }
      })
      .fuse()
      .boxed();
    QuinnTunnel {
      connection,
      id,
      side,
      incoming: Arc::new(tokio::sync::Mutex::new(TunnelIncoming {
        inner: stream_tunnels,
        id,
        side,
      })),
      close_reason,
      authenticated: Default::default(),
      authenticated_notifier: Arc::new(watch::channel(None).0),
      outgoing_closed: Arc::new(overall_cancellation.child_token().into()),
      incoming_closed: incoming_cancellation,
      closed: overall_cancellation,
      baggage: Arc::new(baggage),
    }
  }

  pub fn from_quinn_connection(
    id: TunnelId,
    new_connection: quinn::NewConnection,
    side: TunnelSide,
  ) -> QuinnTunnel<B>
  where
    B: Default,
  {
    Self::from_quinn_connection_with_baggage(id, new_connection, side, Default::default())
  }
}

impl<B> TunnelControl for QuinnTunnel<B> {
  fn close<'a>(
    &'a self,
    reason: TunnelCloseReason,
  ) -> BoxFuture<'a, Result<Arc<TunnelCloseReason>, Arc<TunnelCloseReason>>> {
    // Set the close reason only if it is currently [TunnelCloseReason::Unspecified]
    let prev = self.close_reason.rcu({
      let reason = Arc::new(reason);
      move |previous_reason| {
        Arc::clone(if previous_reason.is_unspecified() {
          &reason
        } else {
          previous_reason
        })
      }
    });
    if !self.closed.is_cancelled() {
      self.closed.cancel();
    }
    // Return failure if the tunnel was already closed, otherwise success
    future::ready(if prev.is_unspecified() {
      Ok(prev)
    } else {
      Err(prev)
    })
    .boxed()
  }

  fn report_authentication_success<'a>(
    &self,
    tunnel_name: super::TunnelName,
  ) -> BoxFuture<'a, Result<(), Option<Arc<TunnelCloseReason>>>> {
    let authenticated_store = Arc::clone(&self.authenticated);
    let authenticated_notifier = Arc::clone(&self.authenticated_notifier);
    let close_reason_store = Arc::clone(&self.close_reason);
    let closed = Arc::clone(&self.closed);
    if closed.is_cancelled() {
      return future::ready(Err(Some(close_reason_store.load_full()))).boxed();
    }
    async move {
      let mut authenticated_store = authenticated_store.write_owned().await;
      if closed.is_cancelled() {
        Err(Some(close_reason_store.load_full()))
      } else if authenticated_store.is_some() {
        Err(None)
      } else {
        *authenticated_store = Some(tunnel_name.clone());
        authenticated_notifier.send_replace(Some(tunnel_name));
        Ok(())
      }
    }
    .boxed()
  }
}

impl<B> TunnelMonitoring for QuinnTunnel<B> {
  fn is_closed(&self) -> bool {
    self.closed.is_cancelled()
  }

  fn on_closed(&'_ self) -> BoxFuture<'static, Arc<TunnelCloseReason>> {
    let closed = CancellationListener::from(&**self.closed);
    let close_reason_store = Arc::clone(&self.close_reason);
    async move {
      closed
        .cancelled()
        .map(move |_| close_reason_store.load_full())
        .await
    }
    .boxed()
  }

  fn on_authenticated(
    &'_ self,
  ) -> BoxFuture<'static, Result<super::TunnelName, Arc<TunnelCloseReason>>> {
    let mut subscription = self.authenticated_notifier.subscribe();
    let closed = Arc::clone(&self.closed);
    let close_reason_store = Arc::clone(&self.close_reason);
    async move {
      // If closed, abort early
      if closed.is_cancelled() {
        return Err(close_reason_store.load_full());
      }
      // Check the current state, and return it if it is populated with authentication data
      let current_value = (*subscription.borrow_and_update()).clone();
      if let Some(v) = current_value {
        // Return our existing authentication state
        Ok(v)
      } else {
        // We're not authenticated yet, so wait for the next update, or bail when closed
        let subscription = WatchStream::new(subscription);
        // Select only authentication events that contain authentication results
        let mut subscription = subscription.filter_map(|v| future::ready(v));
        // Wait for the next authentication event, or bail when closed
        let res = subscription
          .next()
          .poll_until(closed.cancelled())
          .await
          .flatten();
        // If our stream ended with no authentication data, we've been closed; return whatever close reason is present
        res.ok_or_else(|| close_reason_store.load_full())
      }
    }
    .boxed()
  }
}

impl<B> TunnelMonitoringPerChannel for QuinnTunnel<B> {
  fn is_closed_uplink(&self) -> bool {
    self.outgoing_closed.is_cancelled()
  }

  fn on_closed_uplink(&'_ self) -> BoxFuture<'static, Arc<TunnelCloseReason>> {
    let out_close = CancellationToken::clone(&self.outgoing_closed);
    let close_reason_store = Arc::clone(&self.close_reason);
    async move {
      out_close
        .cancelled()
        .map(move |_| close_reason_store.load_full())
        .await
    }
    .boxed()
  }

  fn is_closed_downlink(&self) -> bool {
    self.incoming_closed.is_cancelled()
  }

  fn on_closed_downlink(&'_ self) -> BoxFuture<'static, Arc<TunnelCloseReason>> {
    let in_close = CancellationToken::clone(&self.incoming_closed);
    let close_reason_store = Arc::clone(&self.close_reason);
    async move {
      in_close
        .cancelled()
        .map(move |_| close_reason_store.load_full())
        .await
    }
    .boxed()
  }
}

impl<B> WithTunnelId for QuinnTunnel<B> {
  fn id(&self) -> &TunnelId {
    &self.id
  }
}

impl<B> Sided for QuinnTunnel<B> {
  fn side(&self) -> TunnelSide {
    self.side
  }
}

impl<B> TunnelUplink for QuinnTunnel<B> {
  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream, TunnelError>> {
    if self.is_closed_uplink() {
      return future::ready(Err(TunnelError::ConnectionClosed)).boxed();
    }
    // TODO: make individual sub-streams exit when close() is called, using `quinn::Connection::close()`
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
        let close_outgoing = self.outgoing_closed.clone();
        let close_reason_store = Arc::clone(&self.close_reason);
        move |tunnel_error: &TunnelError| {
          let close_reason = TunnelCloseReason::Error(tunnel_error.clone());
          {
            let close_reason_store = &close_reason_store;
            close_reason_store.store(Arc::new(close_reason));
          };
          if !close_outgoing.is_cancelled() {
            close_outgoing.cancel();
          }
        }
      })
      .boxed()
  }

  fn addr(&self) -> TunnelAddressInfo {
    TunnelAddressInfo::Socket(self.connection.remote_address())
  }
}

impl<B> Tunnel for QuinnTunnel<B>
where
  B: Send + Sync + 'static,
{
  fn downlink<'a>(&'a self) -> BoxFuture<'a, Option<Box<dyn TunnelDownlink + Send + Unpin>>> {
    if self.is_closed_downlink() {
      return future::ready(None).boxed();
    }
    // [TunnelIncoming] is constructed upon opening the tunnel
    // Logic to cut it upon and after downlink closure is handled at time of construction
    // TODO: make individual sub-streams exit when close() is called, using `quinn::Connection::close()`
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

/// Allows attachment of arbitrary data to the lifetime of the tunnel object
///
/// If a value is only required until disconnection, perform cleanup with an
/// `on_closed` handle, and use a Mutex<Option<T>> to represent removed bags
///
/// It is strictly illegal to store a reference to a tunnel in a bag. Memory
/// leaks cannot be reasoned about if any tunnel can extend the lifetimes of
/// any tunnel (including itself) beyond the scope of a live Request. Module
/// handles (`TunnelRegistry`, `ServiceRegistry`, etc) must all be WeakRefs.
impl<B> Baggage for QuinnTunnel<B> {
  type Bag<'a> = Arc<B> where B: 'a;

  fn bag<'a>(&'a self) -> Self::Bag<'a> {
    self.baggage.clone()
  }
}

#[deprecated(
  since = "0.6.0-alpha.7",
  note = "Use `QuinnTunnel::from_quinn_connection` instead"
)]
pub fn from_quinn_connection(
  id: TunnelId,
  new_connection: quinn::NewConnection,
  side: TunnelSide,
) -> QuinnTunnel<()> {
  QuinnTunnel::<()>::from_quinn_connection(id, new_connection, side)
}

#[deprecated(
  since = "0.6.0-alpha.7",
  note = "Use `QuinnTunnel::from_quinn_connection_with_baggage` instead"
)]
pub fn from_quinn_connection_with_baggage<B>(
  id: TunnelId,
  new_connection: quinn::NewConnection,
  side: TunnelSide,
  baggage: B,
) -> QuinnTunnel<B> {
  QuinnTunnel::from_quinn_connection_with_baggage(id, new_connection, side, baggage)
}

impl<Baggage> AssignTunnelId<QuinnTunnel<Baggage>> for (quinn::NewConnection, TunnelSide, Baggage)
where
  Baggage: Send + Sync + 'static,
{
  fn assign_tunnel_id(self, tunnel_id: TunnelId) -> QuinnTunnel<Baggage> {
    let (new_connection, side, baggage) = self;
    QuinnTunnel::from_quinn_connection_with_baggage(tunnel_id, new_connection, side, baggage)
  }
}
