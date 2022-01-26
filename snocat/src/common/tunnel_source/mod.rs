// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! Sources both listen- and connection-based tunnels

use futures::stream::{BoxStream, Stream, StreamExt};
use std::{
  fmt::Debug,
  hash::Hash,
  net::SocketAddr,
  pin::Pin,
  sync::{Arc, TryLockError},
  task::{Context, Poll},
};

use tokio_stream::StreamMap;

use crate::common::protocol::tunnel::{BoxedTunnel, TunnelSide};

pub struct QuinnListenEndpoint<Baggage> {
  bind_addr: SocketAddr,
  quinn_config: quinn::ServerConfig,
  endpoint: quinn::Endpoint,
  incoming: BoxStream<'static, quinn::NewConnection>,
  baggage_constructor: Box<dyn (Fn(&quinn::NewConnection) -> Baggage) + Send + Sync>,
}

impl QuinnListenEndpoint<()> {
  pub fn bind(
    bind_addr: SocketAddr,
    quinn_config: quinn::ServerConfig,
  ) -> Result<Self, std::io::Error> {
    Self::bind_with_baggage(bind_addr, quinn_config, |_| ())
  }
}

impl<Baggage> QuinnListenEndpoint<Baggage> {
  pub fn bind_with_baggage<F>(
    bind_addr: SocketAddr,
    quinn_config: quinn::ServerConfig,
    create_baggage: F,
  ) -> Result<Self, std::io::Error>
  where
    F: (Fn(&quinn::NewConnection) -> Baggage) + Send + Sync + 'static,
  {
    let (endpoint, incoming) = quinn::Endpoint::server(quinn_config.clone(), bind_addr)?;
    let incoming = incoming
      .filter_map(|connecting| async move { connecting.await.ok() })
      .boxed();
    Ok(Self {
      bind_addr,
      quinn_config,
      endpoint,
      incoming,
      baggage_constructor: Box::new(create_baggage),
    })
  }
}

impl<Baggage> Stream for QuinnListenEndpoint<Baggage>
where
  Self: Send + Unpin,
{
  type Item = (quinn::NewConnection, TunnelSide, Baggage);

  fn poll_next(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Option<Self::Item>> {
    let res = futures::ready!(Stream::poll_next(Pin::new(&mut self.incoming), cx));
    match res {
      None => Poll::Ready(None),
      Some(new_connection) => {
        let baggage = (self.baggage_constructor)(&new_connection);
        Poll::Ready(Some((new_connection, TunnelSide::Listen, baggage)))
      }
    }
  }
}

/// Structure used to hold boxed streams which have an ID associated with them
///
/// Primarily for use alongside StreamMap or DynamicStreamSet.
pub struct NamedBoxedStream<Id, StreamItem> {
  id: Id,
  stream: BoxStream<'static, StreamItem>,
}

impl<Id, StreamItem> NamedBoxedStream<Id, StreamItem> {
  pub fn new<TStream>(id: Id, stream: TStream) -> Self
  where
    TStream: Stream<Item = StreamItem> + Send + Sync + 'static,
  {
    Self::new_pre_boxed(id, stream.boxed())
  }

  pub fn new_pre_boxed(id: Id, stream: BoxStream<'static, StreamItem>) -> Self {
    Self { id, stream }
  }
}

impl<Id, StreamItem> Stream for NamedBoxedStream<Id, StreamItem>
where
  Id: Unpin,
{
  type Item = StreamItem;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    Stream::poll_next(Pin::new(&mut self.stream), cx)
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.stream.size_hint()
  }
}

impl<Id, StreamItem> std::fmt::Debug for NamedBoxedStream<Id, StreamItem>
where
  Id: Debug,
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct(stringify!(DynamicConnection))
      .field("id", &self.id)
      .finish_non_exhaustive()
  }
}

/// A set of connections / endpoints that can be updated dynamically, to allow runtime addition and
/// removal of connections / "Tunnel sources" to those being handled by a tunnel server.
pub type DynamicConnectionSet<Id, TunnelType = BoxedTunnel<'static>> =
  DynamicStreamSet<Id, TunnelType>;

/// A strict wrapper for StreamMap that requires boxing of the items and handles locking for updates
/// Can be used to merges outputs from a runtime-editable set of endpoint ports
pub struct DynamicStreamSet<Id, TStream> {
  // RwLock is semantically better here but poll_next is a mutation, so we'd have to
  // trick it by using something like a refcell internally, losing most of the benefits.
  //
  // As this is to facilitate async, this is likely to be a near-uncontested mutex, but
  // we use a std::sync::Mutex instead of an async one as we only expect to lock briefly.
  streams: Arc<std::sync::Mutex<StreamMap<Id, NamedBoxedStream<Id, TStream>>>>,
}

pub struct DynamicStreamSetHandle<Id, TStream> {
  // RwLock is semantically better here but poll_next is a mutation, so we'd have to
  // trick it by using something like a refcell internally, losing most of the benefits.
  //
  // As this is to facilitate async, this is likely to be a near-uncontested mutex, but
  // we use a std::sync::Mutex instead of an async one as we only expect to lock briefly.
  streams: Arc<std::sync::Mutex<StreamMap<Id, NamedBoxedStream<Id, TStream>>>>,
}

impl<Id, StreamItem> DynamicStreamSet<Id, StreamItem> {
  pub fn new() -> Self {
    Self {
      streams: Arc::new(std::sync::Mutex::new(StreamMap::new())),
    }
  }

  pub fn attach(
    &self,
    source: NamedBoxedStream<Id, StreamItem>,
  ) -> Option<NamedBoxedStream<Id, StreamItem>>
  where
    Id: Clone + Hash + Eq,
  {
    let mut streams = self.streams.lock().expect("Mutex poisoned");
    streams.insert(source.id.clone(), source)
  }

  pub fn attach_stream(
    &self,
    id: Id,
    source: BoxStream<'static, StreamItem>,
  ) -> Option<NamedBoxedStream<Id, StreamItem>>
  where
    Id: Clone + Hash + Eq,
  {
    let endpoint = NamedBoxedStream::new_pre_boxed(id.clone(), source);
    self.attach(endpoint)
  }

  pub fn detach(&self, id: &Id) -> Option<NamedBoxedStream<Id, StreamItem>>
  where
    Id: Hash + Eq,
  {
    let mut streams = self.streams.lock().expect("Mutex poisoned");
    streams.remove(id)
  }

  pub fn handle(&self) -> DynamicStreamSetHandle<Id, StreamItem> {
    DynamicStreamSetHandle {
      streams: self.streams.clone(),
    }
  }

  pub fn into_handle(self) -> DynamicStreamSetHandle<Id, StreamItem> {
    DynamicStreamSetHandle {
      streams: self.streams,
    }
  }

  fn poll_next(
    streams: &std::sync::Mutex<StreamMap<Id, NamedBoxedStream<Id, StreamItem>>>,
    cx: &mut Context<'_>,
  ) -> Poll<Option<(Id, StreamItem)>>
  where
    Id: Clone + Unpin,
  {
    // Use try_lock to ensure that we don't deadlock in a single-threaded async scenario
    let mut streams = match streams.try_lock() {
      Ok(s) => s,
      Err(TryLockError::WouldBlock) => {
        // Queue for another wake, to retry the mutex; essentially, yield for other async
        // Note that this effectively becomes a spin-lock if the mutex is held while the
        // async runtime has nothing else to work on.
        cx.waker().wake_by_ref();
        return Poll::Pending;
      }
      Err(TryLockError::Poisoned(poison)) => Err(poison).expect("Lock poisoned"),
    };
    Stream::poll_next(Pin::new(&mut *streams), cx)
  }
}

impl<Id, StreamItem> Stream for DynamicStreamSet<Id, StreamItem>
where
  Id: Clone + Unpin,
{
  type Item = (Id, StreamItem);

  fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    Self::poll_next(&*self.streams, cx)
  }

  // Size is hintable but slow to calculate and only useful if all sub-stream hints are precise
  // Implement this only if the maintainability cost of a membership-update driven design is lower
  // than that of the performance cost of doing so. Also consider the cost of mutex locking.
  // fn size_hint(&self) -> (usize, Option<usize>) { (0, None) }
}

impl<Id, StreamItem> Stream for DynamicStreamSetHandle<Id, StreamItem>
where
  Id: Clone + Unpin,
{
  type Item = (Id, StreamItem);

  fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    DynamicStreamSet::poll_next(&*self.streams, cx)
  }

  // See size_hint note on [DynamicStreamSet] for why we do not implement this
  // fn size_hint(&self) -> (usize, Option<usize>) { (0, None) }
}

#[cfg(test)]
mod tests {
  use super::{DynamicStreamSet, QuinnListenEndpoint};
  use crate::common::protocol::tunnel::quinn_tunnel::QuinnTunnel;
  use crate::common::protocol::tunnel::AssignTunnelId;

  use futures::{stream, FutureExt, StreamExt};
  use std::collections::HashSet;
  use std::iter::FromIterator;

  /// Enforce that the content of the endpoint is a valid tunnel assignment content stream
  fn static_test_endpoint_items_assign_tunnel_id<Baggage>(
    mut endpoint: QuinnListenEndpoint<Baggage>,
  ) -> Option<impl AssignTunnelId<QuinnTunnel<Baggage>>>
  where
    Baggage: Send + Sync + 'static,
    QuinnListenEndpoint<Baggage>: Send + Unpin + 'static,
  {
    endpoint.next().now_or_never().flatten()
  }

  #[tokio::test]
  async fn add_and_remove() {
    let set = DynamicStreamSet::<u32, char>::new();
    let a = stream::iter(vec!['a']).boxed();
    let b = stream::iter(vec!['b']).boxed();
    let c = stream::iter(vec!['c']).boxed();
    assert!(set.attach_stream(1u32, a).is_none(), "Must attach to blank");
    assert!(
      set.attach_stream(2u32, b).is_none(),
      "Must attach to non-blank with new key"
    );
    let mut replaced_b = set
      .attach_stream(2u32, c)
      .expect("Must overwrite keys and return an old one");
    let mut detached_a = set.detach(&1u32).expect("Must detach fresh keys by ID");
    let mut detached_c = set.detach(&2u32).expect("Must detach replaced keys by ID");
    assert_eq!(detached_a.id, 1u32);
    assert_eq!(
      detached_a.stream.next().await.expect("Must have item"),
      'a',
      "Fresh-key stream identity mismatch"
    );
    assert_eq!(replaced_b.id, 2u32);
    assert_eq!(
      replaced_b.stream.next().await.expect("Must have item"),
      'b',
      "Replaced stream identity mismatch"
    );
    assert_eq!(detached_c.id, 2u32);
    assert_eq!(
      detached_c.stream.next().await.expect("Must have item"),
      'c',
      "Replacement stream identity mismatch"
    );
  }

  #[tokio::test]
  async fn poll_contents() {
    let set = DynamicStreamSet::<u32, char>::new();
    let a = stream::iter(vec!['a']).boxed();
    let b = stream::iter(vec!['b']).boxed();
    let c = stream::iter(vec!['c']).boxed();
    assert!(set.attach_stream(1u32, a).is_none(), "Must attach to blank");
    assert!(
      set.attach_stream(2u32, b).is_none(),
      "Must attach to non-blank with new key"
    );
    set
      .attach_stream(2u32, c)
      .expect("Must replace existing keys");
    // We use a hashset because we don't specify a strict ordering, that's internal to StreamMap
    let results = set.collect::<HashSet<_>>().await;
    // Note that 'b' must not occur here because we've detached it
    assert_eq!(
      results,
      HashSet::from_iter(vec![(1, 'a'), (2, 'c')].into_iter())
    );
  }

  #[tokio::test]
  async fn end_of_stream_removal() {
    use std::sync::Arc;
    let set = Arc::new(DynamicStreamSet::<u32, i32>::new());
    let a = stream::iter(vec![1, 2, 3]).boxed();
    assert!(set.attach_stream(1u32, a).is_none(), "Must attach to blank");
    let collected = set.handle().collect::<Vec<_>>().await;
    assert_eq!(collected.as_slice(), &[(1, 1), (1, 2), (1, 3)]);
    assert!(
      set.detach(&1u32).is_none(),
      "Must have already detached if polled to empty"
    );
  }
}
