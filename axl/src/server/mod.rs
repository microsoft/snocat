//! Types for building an AXL server and accepting, authenticating, and routing connections

use crate::common::MetaStreamHeader;
use crate::server::deferred::{
  AxlClientIdentifier, ConcurrentDeferredTunnelServer, TunnelManager, TunnelServerEvent,
};
use crate::util::{
  self,
  validators::{parse_ipaddr, parse_port_range, parse_socketaddr},
};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use async_std::sync::{Arc, Mutex};
use futures::future::*;
use futures::{
  future,
  future::FutureExt,
  pin_mut, select_biased,
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

pub mod authentication;
pub mod deferred;
pub mod routing;

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

  tracing::info!("Sending HELO...");
  let mut buffer = [0u8; 64];
  use std::io::Write;
  write!(&mut buffer[..], "HELO").unwrap();
  proxy_connection.0.write_all(&buffer).await?;
  buffer = [0u8; 64];
  proxy_connection.1.read_exact(&mut buffer).await?;
  let read_string = std::str::from_utf8(&buffer).unwrap();
  if !read_string.starts_with("HELO/HELO\0") {
    tracing::trace!(raw = read_string, "bad_client_ack");
    return Err(AnyErr::msg("Invalid client ack"));
  }
  tracing::trace!("client_ack");

  tracing::info!("Beginning proxying...");
  let proxy_res =
    util::proxy_from_tcp_stream(source, (&mut proxy_connection.0, &mut proxy_connection.1)).await;
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

// Accept connections from a TCP socket and forward them to new connections over AXL
// Watch for failures on BuildConnection, which is responsible for timeout logic if needed
async fn accept_loop<Provider: ProxyConnectionProvider>(
  listener: &mut TcpListener,
  addr: SocketAddr,
  proxy_provider: Provider,
) -> Result<()> {
  use async_std::prelude::*;
  use futures::stream::{self, FuturesUnordered, StreamExt, TryStreamExt};
  listener
    .incoming()
    .map_err(|e| -> AnyErr { e.into() })
    .scan((proxy_provider, addr), |baggage, res: Result<_, _>| {
      future::ready(match res {
        Ok(conn) => Some(Ok((conn, baggage.clone()))),
        Err(e) => Some(Err(e)),
      })
    })
    .try_for_each_concurrent(None, move |(stream, (prov, addr))| {
      async move {
        Ok(
          handle_connection(stream, addr.clone(), prov)
            .await
            .context("Error handling connection")?,
        )
      }
      .boxed()
    })
    .await
    .context("Failure running acceptance loop")?;
  Ok(())
}

type ProxyConnectionOutput = (MetaStreamHeader, (quinn::SendStream, quinn::RecvStream));

trait ProxyConnectionProvider: Send + Sync + Clone + std::fmt::Debug {
  fn open_connection(&self, peer_address: SocketAddr) -> BoxFuture<Result<ProxyConnectionOutput>>;
}

#[derive(Clone)]
struct BasicProxyConnectionProvider {
  conn: quinn::Connection,
}

impl BasicProxyConnectionProvider {
  pub fn new(conn: quinn::Connection) -> BasicProxyConnectionProvider {
    BasicProxyConnectionProvider { conn }
  }
}

impl ProxyConnectionProvider for BasicProxyConnectionProvider {
  fn open_connection(&self, _peer_address: SocketAddr) -> BoxFuture<Result<ProxyConnectionOutput>> {
    async move {
      let (send, recv) = self.conn.open_bi().await?;
      Ok((MetaStreamHeader::new(), (send, recv)))
    }
    .boxed()
  }
}

impl std::fmt::Debug for BasicProxyConnectionProvider {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("<BasicProxyConnectionProvider>")
  }
}

/// Binds ports from a given range on the host, allocating one to each authenticated tunnel client.
#[derive(Debug)]
pub struct TcpTunnelManager {
  range: std::ops::RangeInclusive<u16>,
  bind_ip: IpAddr,
  bound_ports: Arc<Mutex<std::collections::HashSet<u16>>>,
}

impl TcpTunnelManager {
  pub fn new<T: Into<u16>>(
    bind_port_range: std::ops::RangeInclusive<T>,
    bind_ip: IpAddr,
  ) -> TcpTunnelManager {
    let (start, end): (u16, u16) = {
      let (a, b) = bind_port_range.into_inner();
      (a.into(), b.into())
    };
    TcpTunnelManager {
      range: std::ops::RangeInclusive::new(start, end),
      bind_ip,
      bound_ports: Default::default(),
    }
  }

  /// Connection lifetime handler for a single tunnel; multiple tunnels will run concurrently.
  async fn handle_connection(
    &self,
    z: &mut gen_z::Yielder<TunnelServerEvent>,
    tunnel: quinn::NewConnection,
    shutdown_notifier: triggered::Listener,
  ) -> Result<()> {
    let port_range = self.range.clone();
    let bind_ip = self.bind_ip;
    let bound_ports = self.bound_ports.clone();
    if shutdown_notifier.is_triggered() {
      return Err(AnyErr::msg("Connection aborted due to pre-closure"));
    }

    let remote_addr = tunnel.connection.remote_address();
    let id = AxlClientIdentifier::new(remote_addr.to_string());
    z.send(TunnelServerEvent::Identified(id.clone(), remote_addr))
      .await;
    // TODO: register a connection *only after* session authentication (make an async authn trait)
    let next_port: u16 = {
      let mut lock = bound_ports.lock().await;
      let port = port_range
        .into_iter()
        .filter(|test_port| !lock.contains(test_port))
        .min()
        .ok_or_else(|| {
          AnyErr::msg(format!(
            "No free ports available in range {:?}",
            &self.range
          ))
        })?;
      lock.insert(port);
      port
    };
    let res: Result<(), _> = {
      let bind_addr = SocketAddr::new(bind_ip, next_port);
      tracing::info!(
        "Binding client {:?} ({:?}) on address {:?}",
        id,
        remote_addr,
        bind_addr
      );
      let mut listener = TcpListener::bind(bind_addr).await?;
      let quinn::NewConnection {
        connection: conn,
        bi_streams: _bi,
        uni_streams: uni,
        ..
      } = tunnel;

      // TODO: Periodically attempt to send small amounts of data, in order to trigger a connection failure when the connection has been lost
      let connection_provider = BasicProxyConnectionProvider::new(conn);
      let res: Result<(), AnyErr> = {
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
        // TODO: Pass shutdown_notifier as a parameter instead
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
          uni
            .filter_map(async move |x| x.err())
            .take(1)
            .map(|x| Err(x).context("Peer disconnected"))
            .fuse()
            .boxed(),
        );

        stream::select_all(streams)
          .boxed()
          .next()
          .await
          .unwrap_or_else(|| Err(AnyErr::msg("All streams ended without a message?")))
      };
      res
      // .map(|x| match x {
      //   Either::Left((x, _)) => x,
      //   Either::Right((x, _)) => x
      // }).await
    };

    // On success or failure, unbind port
    {
      let mut lock = bound_ports.lock().await;
      lock.remove(&next_port);
      tracing::trace!("Unbound port {}", next_port);
    }

    res

    // tunnel
    //   .connection
    //   .close(quinn::VarInt::from_u32(42), "Fake handler error".as_bytes());

    // Err(AnyErr::msg("Fake handler error"))
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
