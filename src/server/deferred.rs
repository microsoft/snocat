use crate::common::MetaStreamHeader;
use crate::util::{self, validators::{parse_ipaddr, parse_port_range, parse_socketaddr}};
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
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  boxed::Box,
  path::{Path, PathBuf},
  pin::Pin,
  task::{Context, Poll},
};
use tracing::{info, instrument, trace};

#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone)]
#[repr(transparent)]
pub struct AxlClientIdentifier(Arc<String>);

impl AxlClientIdentifier {
  pub fn new<T: std::convert::Into<String>>(t: T) -> AxlClientIdentifier {
    AxlClientIdentifier(t.into().into())
  }
}

impl std::fmt::Debug for AxlClientIdentifier {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "(axl ({}))", self.0)
  }
}

#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Debug)]
pub enum TunnelServerEvent {
  DebugMessage(String),
  Open(SocketAddr),
  Identified(AxlClientIdentifier, SocketAddr),
  Close(AxlClientIdentifier, SocketAddr),
  Failure(SocketAddr, String),
}

pub trait TunnelManager: Send + std::fmt::Debug + Sync {
  fn handle_connection<'connection, 'manager: 'connection>(
    &'manager self,
    events: &'connection mut gen_z::Yielder<TunnelServerEvent>,
    tunnel: quinn::NewConnection,
    shutdown_notifier: triggered::Listener,
  ) -> futures::future::BoxFuture<'connection, Result<()>>;
}

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
        async move {
          if triggered {
            // TODO: "Not accepting new tunnel connections because of impending shutdown" message
            eprintln!("Refusing connection from {:?} because shutdown is already in progress", addr);
            return false;
          }
          true
        }
      })
      .and_then(async move |(conn, notifier): (quinn::NewConnection, triggered::Listener)| -> Result<stream::BoxStream<'_, TunnelServerEvent>> {
        Ok(generate_stream(move |yielder| self.connection_lifecycle(yielder, conn, notifier)).boxed())
      })
      .filter_map(|x: Result<_, _>| {
        future::ready(match x {
          Ok(s) => Some(s),
          Err(e) => {
            eprintln!("Failure in connection {:#?}", e);
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
      // TODO: Build identifier based on handshake / authentication session
      z.send(TunnelServerEvent::Open(addr)).await;
      let err = self
        .manager
        .handle_connection(&mut z, tunnel, shutdown_notifier)
        .map(move |x| match x {
          Err(e) => Some(TunnelServerEvent::Failure(addr, e.to_string())),
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
