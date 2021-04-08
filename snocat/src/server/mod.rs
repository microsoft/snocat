// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! Types for building an Snocat server and accepting, authenticating, and routing connections

use crate::common::{
  authentication,
  protocol::tunnel::{Tunnel, TunnelIncomingType, TunnelSide},
  MetaStreamHeader,
};
use crate::server::deferred::{
  ConcurrentDeferredTunnelServer, SnocatClientIdentifier, TunnelManager, TunnelServerEvent,
};
use crate::util::framed::write_framed_json;
use crate::util::tunnel_stream::{QuinnTunnelStream, TunnelStream};
use crate::util::{
  self, finally_async,
  validators::{parse_ipaddr, parse_port_range, parse_socketaddr},
};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use futures::{
  future::FutureExt,
  future::{self, *},
  pin_mut, select_biased,
  stream::{self, Stream, StreamExt},
};
use gen_z::gen_z as generate_stream;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use std::{
  boxed::Box,
  ops::RangeInclusive,
  path::{Path, PathBuf},
  pin::Pin,
  sync::Arc,
  task::{Context, Poll},
};
use std::{
  collections::HashSet,
  net::{IpAddr, Ipv4Addr, SocketAddr},
};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::Mutex;
use tokio_stream::wrappers::TcpListenerStream;
use tracing::{info, instrument, trace};
use tracing_futures::Instrument;

pub mod deferred;
pub mod modular;

#[tracing::instrument(skip(source, proxy_connection_provider), err)]
async fn handle_connection<Provider: ProxyConnectionProvider>(
  source: TcpStream,
  if_addr: SocketAddr,
  proxy_connection_provider: Provider,
) -> Result<()> {
  let peer_addr = source.peer_addr().context("Error fetching peer address")?;
  tracing::info!(peer_addr = ?peer_addr, "Peer address identified as {:?}", peer_addr);
  let (proxy_header, mut proxy_connection) =
    proxy_connection_provider.open_connection(peer_addr).await?;

  {
    let header = MetaStreamHeader::new();
    write_framed_json(&mut proxy_connection, &header).await?;
  }

  tracing::info!("Beginning proxying...");
  let (mut receiver, mut sender) = tokio::io::split(&mut proxy_connection);
  let proxy_res = util::proxy_from_tcp_stream(source, (&mut sender, &mut receiver)).await;
  if let Err(e) = proxy_res {
    tracing::error!(
      header = ?proxy_header,
      err = ?e,
      "Proxy failure for {:?}: {:#?}",
      proxy_header,
      e,
    );
  }
  tracing::info!("Connection closed");
  Ok(())
}

// Accept connections from a TCP socket and forward them to new connections over Snocat
// Watch for failures on BuildConnection, which is responsible for timeout logic if needed
async fn accept_loop<Provider: ProxyConnectionProvider + 'static>(
  listener: &mut TcpListenerStream,
  addr: SocketAddr,
  proxy_provider: Provider,
) -> Result<()> {
  use futures::stream::{self, FuturesUnordered, StreamExt, TryStreamExt};
  listener
    .map_err(|e| -> AnyErr { e.into() })
    .scan(
      (Arc::new(proxy_provider), addr),
      |baggage, res: Result<_, _>| {
        future::ready(match res {
          Ok(conn) => Some(Ok((conn, baggage.clone()))),
          Err(e) => Some(Err(e)),
        })
      },
    )
    .try_for_each_concurrent(None, move |(stream, (prov, addr))| {
      async move {
        Ok(
          tokio::task::spawn(handle_connection(stream, addr.clone(), prov))
            .await
            .expect("Panicked task")
            .context("Error handling connection")?,
        )
      }
      .boxed()
    })
    .await
    .context("Failure running acceptance loop")?;
  Ok(())
}

type ProxyConnectionOutput = (MetaStreamHeader, Box<dyn TunnelStream>);

trait ProxyConnectionProvider: Send + Sync + std::fmt::Debug {
  fn open_connection(&self, peer_address: SocketAddr) -> BoxFuture<Result<ProxyConnectionOutput>>;
}

// ProxyConnectionProvider is required to be Send + Sync, so we can trivially forward across Arc
impl<T: ProxyConnectionProvider> ProxyConnectionProvider for Arc<T> {
  fn open_connection(
    &self,
    peer_address: SocketAddr,
  ) -> BoxFuture<'_, Result<ProxyConnectionOutput, anyhow::Error>> {
    ProxyConnectionProvider::open_connection(Arc::as_ref(self), peer_address)
  }
}

struct BasicProxyConnectionProvider {
  conn: Box<dyn Tunnel + Send + Sync + 'static>,
}

impl BasicProxyConnectionProvider {
  pub fn new(conn: Box<dyn Tunnel + Send + Sync + 'static>) -> BasicProxyConnectionProvider {
    BasicProxyConnectionProvider { conn }
  }
}

impl ProxyConnectionProvider for BasicProxyConnectionProvider {
  fn open_connection(&self, _peer_address: SocketAddr) -> BoxFuture<Result<ProxyConnectionOutput>> {
    async move {
      let stream = self.conn.open_link().await?;
      let boxed: Box<dyn TunnelStream> = Box::new(stream);
      Ok((MetaStreamHeader::new(), boxed))
    }
    .boxed()
  }
}

impl std::fmt::Debug for BasicProxyConnectionProvider {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("<BasicProxyConnectionProvider>")
  }
}

#[derive(Debug, Clone)]
pub struct PortRangeAllocator {
  range: std::ops::RangeInclusive<u16>,
  allocated: Arc<Mutex<std::collections::HashSet<u16>>>,
  mark_queue: tokio::sync::mpsc::UnboundedSender<u16>,
  // UnboundedReceiver does not implement clone, so we need an ArcMut of it
  mark_receiver: Arc<Mutex<tokio::sync::mpsc::UnboundedReceiver<u16>>>,
}

#[derive(thiserror::Error, Debug)]
pub enum PortRangeAllocationError {
  #[error("No ports were available to be allocated in range {0:?}")]
  NoFreePorts(std::ops::RangeInclusive<u16>),
}

impl PortRangeAllocator {
  pub fn new<T: Into<u16>>(bind_port_range: std::ops::RangeInclusive<T>) -> PortRangeAllocator {
    let (start, end): (u16, u16) = {
      let (a, b) = bind_port_range.into_inner();
      (a.into(), b.into())
    };
    let (mark_sender, mark_receiver) = tokio::sync::mpsc::unbounded_channel();
    PortRangeAllocator {
      range: std::ops::RangeInclusive::new(start, end),
      allocated: Default::default(),
      mark_queue: mark_sender,
      mark_receiver: Arc::new(Mutex::new(mark_receiver)),
    }
  }

  pub async fn allocate(&self) -> Result<PortRangeAllocationHandle, PortRangeAllocationError> {
    // Used for cleaning up in the deallocator
    let cloned_self = self.clone();
    let range = self.range.clone();
    let mark_receiver = Arc::clone(&self.mark_receiver);
    let mut lock = self.allocated.lock().await;

    // Consume existing marks for freed ports
    {
      let mut mark_receiver = mark_receiver.lock().await;
      Self::cleanup_freed_ports(&mut *lock, &mut *mark_receiver);
    }

    let port = range
      .clone()
      .into_iter()
      .filter(|test_port| !lock.contains(test_port))
      .min()
      .ok_or_else(|| PortRangeAllocationError::NoFreePorts(range.clone()))?;

    let allocation = PortRangeAllocationHandle::new(port, cloned_self);
    lock.insert(allocation.port);
    Ok(allocation)
  }

  pub async fn free(&self, port: u16) -> Result<bool> {
    let mark_receiver = Arc::clone(&self.mark_receiver);
    let mut lock = self.allocated.lock().await;
    let removed = lock.remove(&port);
    if removed {
      tracing::trace!(port = port, "unbound port");
    }
    let mut mark_receiver = mark_receiver.lock().await;
    Self::cleanup_freed_ports(&mut *lock, &mut *mark_receiver);
    Ok(removed)
  }

  fn cleanup_freed_ports(
    allocations: &mut HashSet<u16>,
    mark_receiver: &mut tokio::sync::mpsc::UnboundedReceiver<u16>,
  ) {
    // recv waits forever if a sender can still produce values
    // skip that by only receiving those immediately available
    // HACK: Relies on unbounded receivers being immediately available without intermediate polling
    while let Some(Some(marked)) = mark_receiver.recv().now_or_never() {
      let removed = allocations.remove(&marked);
      if removed {
        tracing::trace!(port = marked, "unbound marked port");
      }
    }
  }

  pub fn mark_freed(&self, port: u16) {
    match self.allocated.try_lock() {
      // fast path if synchronous is possible, skipping the mark queue
      Ok(mut allocations) => {
        // remove specified port; we don't care if it succeeded or not
        let _removed = allocations.remove(&port);
        return;
      }
      Err(_would_block) => {
        match self.mark_queue.send(port) {
          // Message queued, do nothing
          Ok(()) => (),
          // Other side was closed
          // Without a receiver, we don't actually need to free anything, so do nothing
          Err(_send_error) => (),
        }
      }
    }
  }

  pub fn range(&self) -> &RangeInclusive<u16> {
    &self.range
  }
}

pub struct PortRangeAllocationHandle {
  port: u16,
  allocated_in: Option<PortRangeAllocator>,
}

impl PortRangeAllocationHandle {
  pub fn new(port: u16, allocated_in: PortRangeAllocator) -> Self {
    Self {
      port,
      allocated_in: Some(allocated_in),
    }
  }
  pub fn port(&self) -> u16 {
    self.port
  }
}

impl Drop for PortRangeAllocationHandle {
  fn drop(&mut self) {
    match std::mem::replace(&mut self.allocated_in, None) {
      None => (),
      Some(allocator) => {
        allocator.mark_freed(self.port);
      }
    }
  }
}

/// Binds ports from a given range on the host, allocating one to each authenticated tunnel client.
#[derive(Debug)]
pub struct TcpTunnelManager {
  bind_ip: IpAddr,
  bound_ports: Arc<PortRangeAllocator>,
  authenticator: Box<dyn authentication::AuthenticationHandler>,
}

impl TcpTunnelManager {
  pub fn new<TPortRange: Into<u16>>(
    bind_port_range: std::ops::RangeInclusive<TPortRange>,
    bind_ip: IpAddr,
    authenticator: Box<dyn authentication::AuthenticationHandler>,
  ) -> TcpTunnelManager {
    TcpTunnelManager {
      bind_ip,
      bound_ports: PortRangeAllocator::new(bind_port_range).into(),
      authenticator,
    }
  }

  /// Connection lifetime handler for a single tunnel; multiple tunnels will run concurrently.
  fn handle_connection<'a>(
    &'a self,
    z: &'a mut gen_z::Yielder<TunnelServerEvent>,
    tunnel: quinn::NewConnection,
    shutdown_notifier: triggered::Listener,
  ) -> BoxFuture<'a, Result<()>> {
    // Capture copies of items needed from &self before the first await
    let bound_ports = self.bound_ports.clone();
    let bind_ip = self.bind_ip;
    if shutdown_notifier.is_triggered() {
      return future::ready(Err(AnyErr::msg("Connection aborted due to pre-closure"))).boxed();
    }

    async move {
      let remote_addr = tunnel.connection.remote_address();
      let (mut tunnel, mut incoming) = crate::common::protocol::tunnel::from_quinn_endpoint(tunnel, TunnelSide::Listen);
      let id =
        authentication::perform_authentication(&self.authenticator, &mut tunnel, &mut incoming, &shutdown_notifier)
          .await??;
      z.send(TunnelServerEvent::Identified(id.clone(), remote_addr))
        .await;
      // TODO: register a connection *only after* session authentication (make an async authn trait)
      let next_port: PortRangeAllocationHandle = bound_ports.allocate().await?;
      let lifetime_handler_span =
        tracing::debug_span!("lifetime handler", id=?id, tcp_port=next_port.port());
      let lifetime_handler_res: Result<(), AnyErr> = async move {
        let bind_addr = SocketAddr::new(bind_ip, next_port.port());
        tracing::info!(target: "binding tcp listener", id=?id, remote=?remote_addr, addr=?bind_addr);
        let mut listener = TcpListenerStream::new(TcpListener::bind(bind_addr).await?);
        tracing::info!(target: "bound tcp listener", id=?id, remote=?remote_addr, addr=?bind_addr);

        let connection_provider = BasicProxyConnectionProvider::new(Box::new(tunnel));
        let mut streams: Vec<stream::BoxStream<Result<(), AnyErr>>> = Vec::new();
        streams.push(
          accept_loop(&mut listener, bind_addr, connection_provider)
            .fuse()
            .into_stream()
            .boxed(),
        );
        // Watch for SIGINT and close the acceptance loop
        // This is a bit early of placement for this check- Live connections may be abruptly closed by
        // this design, and the connection handlers themselves should be watching for shutdown notifications.
        // The accept loop should gracefully handle its own shutdown notification requests.
        // TODO: Pass shutdown_notifier as a parameter instead, and add a max-duration hard-timeout here
        streams.push(
          shutdown_notifier
            .map(|_| -> Result<(), AnyErr> {
              Err(AnyErr::msg("Shutdown aborting acceptance loop"))
            })
            .into_stream()
            .boxed(),
        );
        // Waiting for errors on uni-streams allows us to watch for a peer disconnection
        streams.push(
          incoming.streams()
            .map(|_| ())
            .filter(|_| future::ready(false))
            .chain(stream::once(future::ready(())))
            .take(1)
            .map(|_| Err(anyhow::Error::msg("Peer disconnected")))
            .fuse()
            .boxed(),
        );

        stream::select_all(streams)
          .boxed()
          .next()
          .await
          .unwrap_or_else(|| Err(AnyErr::msg("All streams ended without a message?")))?;
        Ok(())
      }
      .instrument(lifetime_handler_span)
      .await;

      lifetime_handler_res?;

      Ok(())
    }.boxed()
  }
}

impl TunnelManager for TcpTunnelManager {
  fn handle_connection<'connection, 'manager: 'connection>(
    &'manager self,
    events: &'connection mut gen_z::Yielder<TunnelServerEvent>,
    tunnel: quinn::NewConnection,
    shutdown_notifier: triggered::Listener,
  ) -> futures::future::BoxFuture<'connection, Result<()>> {
    TcpTunnelManager::handle_connection(self, events, tunnel, shutdown_notifier).boxed()
  }
}

impl<T: std::convert::AsRef<dyn TunnelManager + 'static> + std::fmt::Debug + Send + Sync>
  TunnelManager for T
{
  fn handle_connection<'connection, 'manager: 'connection>(
    &'manager self,
    events: &'connection mut gen_z::Yielder<TunnelServerEvent>,
    tunnel: quinn::NewConnection,
    shutdown_notifier: triggered::Listener,
  ) -> futures::future::BoxFuture<'connection, Result<()>> {
    TunnelManager::handle_connection(
      std::convert::AsRef::as_ref(self),
      events,
      tunnel,
      shutdown_notifier,
    )
    .boxed()
  }
}

/*

server_main will own all components, and must actively await other components' shutdown
listeners will own their tunnel instances
- thus server will own listeners, and must know which listeners own which tunnels
listeners can choose to exit if their tunnel closes
- thus server must acknowledge that a listener has exited
-- server must be aware that all tunnels are closed prior to full shutdown
--- QUIC connection does not shutdown until all listeners are closed
--- New QUIC tunnels must be refused when in a "shutting down" state


*/
