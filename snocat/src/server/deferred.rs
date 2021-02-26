// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! Server backend allowing a single-threaded reactor to concurrently run multiple tunnels.

use crate::common::MetaStreamHeader;
use crate::util::{
  self,
  validators::{parse_ipaddr, parse_port_range, parse_socketaddr},
};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use async_std::sync::{Arc, Mutex};
use futures::future::*;
use futures::{
  future, pin_mut, select_biased,
  stream::{self, Stream, StreamExt},
};
use gen_z::gen_z as generate_stream;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use serde::{Deserializer, Serializer};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  boxed::Box,
  path::{Path, PathBuf},
  pin::Pin,
  task::{Context, Poll},
};
use tracing::{info, instrument, trace};

/// A name for an Snocat tunnel, used to identify its connection in [`TunnelServerEvent`]s.
#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone)]
#[repr(transparent)]
pub struct SnocatClientIdentifier(Arc<String>);

impl serde::Serialize for SnocatClientIdentifier {
  fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&self.0)
  }
}
impl<'de> serde::de::Deserialize<'de> for SnocatClientIdentifier {
  fn deserialize<D>(deserializer: D) -> Result<Self, <D as Deserializer<'de>>::Error>
  where
    D: Deserializer<'de>,
  {
    let s: String = serde::Deserialize::deserialize(deserializer)?;
    Ok(SnocatClientIdentifier::new(s))
  }
}

impl SnocatClientIdentifier {
  pub fn new<T: std::convert::Into<String>>(t: T) -> SnocatClientIdentifier {
    SnocatClientIdentifier(t.into().into())
  }
}

impl std::fmt::Debug for SnocatClientIdentifier {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "(snocat ({}))", self.0)
  }
}

/// High-level logging events a [`TunnelManager`] *may* report to its connection handler.
/// Should only be used for high-level logging; individual stream monitoring and lifecycle watching
/// should instead be done by implementing or wrapping a [`TunnelManager`] to monitor connections.
#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Debug)]
pub enum TunnelServerEvent {
  DebugMessage(String),
  Open(SocketAddr),
  Identified(SnocatClientIdentifier, SocketAddr),
  Close(SnocatClientIdentifier, SocketAddr),
  Failure(SocketAddr, Option<SnocatClientIdentifier>, String),
}

/// A [`TunnelManager`](trait@TunnelManager) is responsible for the lifecycle of individual tunnels.
/// The tunnel manager can act to coordinate with another non-Send or non-Sync entity,
/// or manage all connections itself. In either case, it is responsible for synchronization.
pub trait TunnelManager: std::fmt::Debug + Send + Sync {
  /// [`TunnelManager::handle_connection`] must return a future which resolves when the tunnel is closed.
  /// It is responsible for handling authentication of the source of the tunnel connection,
  /// as well as handling incoming streams and datagrams, and owns all outgoing data to the client.
  /// If `shutdown_notifier` is triggered, handle_connection must exit promptly, without
  /// awaiting response from the client, to prevent a hostile client from holding the server alive.
  fn handle_connection<'connection, 'manager: 'connection>(
    &'manager self,
    events: &'connection mut gen_z::Yielder<TunnelServerEvent>,
    tunnel: quinn::NewConnection,
    shutdown_notifier: triggered::Listener,
  ) -> futures::future::BoxFuture<'connection, Result<()>>;
}

/// Manages an [`Incoming`](struct@quinn::generic::Incoming) stream and maps it to a
/// [`TunnelManager`](trait@TunnelManager) with asynchronous concurrency on a single thread.
/// [`ConcurrentDeferredTunnelServer::handle_incoming`] converts a stream of connections to a
/// stream of events on those connections, and runs the connections concurrently.
#[derive(Debug)]
pub struct ConcurrentDeferredTunnelServer<Manager>
where
  Manager: TunnelManager + Send + Sync + std::fmt::Debug,
{
  manager: Manager,
}

impl<Manager: TunnelManager + Send + Sync> ConcurrentDeferredTunnelServer<Manager> {
  pub fn new(manager: Manager) -> ConcurrentDeferredTunnelServer<Manager> {
    ConcurrentDeferredTunnelServer { manager }
  }

  #[tracing::instrument(skip(stream, shutdown_notifier))]
  pub fn handle_incoming<'a>(
    &'a self,
    stream: impl futures::stream::Stream<Item = quinn::NewConnection> + Send + 'a,
    shutdown_notifier: triggered::Listener,
  ) -> futures::stream::BoxStream<'a, TunnelServerEvent> {
    use futures::stream::{BoxStream, TryStreamExt};
    let streams = stream
      // Drop new connections once shutdown has been requested
      .take_until(shutdown_notifier.clone())
      // Mix a copy of the shutdown notifier into each element of the stream
      .scan(shutdown_notifier.clone(), |notif, connection| {
        future::ready(Some((connection, notif.clone())))
      })
      // Convert into a TryStream, within which we'll handle failure reporting later in the pipeline
      .map(|x| -> Result<_, AnyErr> { Ok(x) })
      // Filter out new connections during shutdown
      .try_filter(|(conn, notifier): &(quinn::NewConnection, triggered::Listener)| {
        let triggered = notifier.is_triggered();
        let addr = conn.connection.remote_address();
        future::ready(if triggered {
          conn.connection.close(quinn::VarInt::from_u32(1), "shutdown pending".as_bytes());
          tracing::warn!(?addr, "Refusing connection due to impending shutdown");
          false
        } else {
          true
        })
      })
      .and_then(async move |(conn, notifier): (quinn::NewConnection, triggered::Listener)| -> Result<stream::BoxStream<'_, TunnelServerEvent>> {
        Ok(generate_stream(move |yielder| self.connection_lifecycle(yielder, conn, notifier)).boxed())
      })
      .filter_map(|x: Result<_, _>| {
        future::ready(match x {
          Ok(s) => Some(s),
          Err(e) => {
            tracing::warn!("Failure in tunnel connection: {:#?}", e);
            None
          }
        })
      })
      .map(|stream: BoxStream<TunnelServerEvent>| stream) // no-op to aid type inference in IDEs
      ;

    util::merge_streams::merge_streams(streams)
  }

  #[tracing::instrument(skip(z, tunnel, shutdown_notifier))]
  fn connection_lifecycle(
    &self,
    mut z: gen_z::Yielder<TunnelServerEvent>,
    tunnel: quinn::NewConnection,
    shutdown_notifier: triggered::Listener,
  ) -> BoxFuture<()> {
    let addr = tunnel.connection.remote_address();
    async move {
      z.send(TunnelServerEvent::Open(addr)).await;
      let err = self
        .manager
        .handle_connection(&mut z, tunnel, shutdown_notifier)
        .map(move |x| match x {
          Err(e) => Some(TunnelServerEvent::Failure(addr, None, e.to_string())),
          Ok(_res) => None,
        })
        .await;
      if let Some(err) = err {
        z.send(err).await;
      }
    }
    .boxed()
  }
}
