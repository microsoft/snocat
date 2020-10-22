use crate::common::MetaStreamHeader;
use crate::util::{self, parse_ipaddr, parse_port_range, parse_socketaddr};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use async_std::sync::{Arc, Mutex};
use futures::future::*;
use futures::{
  future, pin_mut, select_biased,
  stream::{self, Stream, StreamExt},
};
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

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ServerArgs {
  pub cert: PathBuf,
  pub key: PathBuf,
  pub quinn_bind_addr: std::net::SocketAddr,
  pub tcp_bind_ip: std::net::IpAddr,
  pub tcp_bind_port_range: std::ops::RangeInclusive<u16>,
}

pub async fn server_arg_handling(args: &'_ clap::ArgMatches<'_>) -> Result<ServerArgs> {
  let cert_path = Path::new(args.value_of("cert").unwrap()).to_path_buf();
  let key_path = Path::new(args.value_of("key").unwrap()).to_path_buf();

  Ok(ServerArgs {
    cert: cert_path,
    key: key_path,
    quinn_bind_addr: parse_socketaddr(args.value_of("quic").unwrap())?,
    tcp_bind_ip: parse_ipaddr(args.value_of("tcp").unwrap())?,
    tcp_bind_port_range: parse_port_range(args.value_of("bind_range").unwrap())?,
  })
}

async fn handle_connection(
  source: TcpStream,
  listen_port: SocketAddr,
  build_proxy_connection: &ProxyConnectionProvider<'_, '_, '_>,
) -> Result<()> {
  let peer_addr = source.peer_addr().context("Error fetching peer address")?;
  println!(
    "Received connection on port {} from {:#?}",
    listen_port.port(),
    &peer_addr
  );
  let (proxy_target, await_connection) = build_proxy_connection(peer_addr).await;
  let proxy_connect_res = {
    let timeout_future: Pin<Box<BoxFuture<Result<()>>>> = Box::pin(
      async_std::future::timeout(
        std::time::Duration::from_millis(5000),
        future::pending::<Result<()>>(),
      )
      .map(|_| Err(anyhow::Error::msg("Timeout occurred")))
      .fuse()
      .boxed(),
    );
    let mut watcher_fd_holder: i32 = 0;
    let watcher: Pin<Box<BoxFuture<Result<(), AnyErr>>>> = Box::pin(
      util::PollerVortex::new(util::async_tcpstream_as_evented_fd(
        &source,
        &mut watcher_fd_holder,
      ))
      .map(|r| r.context("Client disconnected"))
      .fuse()
      .boxed(),
    );
    use futures::future::{AbortHandle, AbortRegistration, Abortable, FutureExt};
    let abort_handler: Pin<Box<BoxFuture<Result<(), AnyErr>>>> = Box::pin(
      futures::future::try_select(watcher, timeout_future)
        .map(|_| -> Result<()> { Err(anyhow::Error::msg("Aborted by handler")) })
        .fuse()
        .boxed(),
    );
    let proxy_connect_res: Result<TcpStream> =
      match futures::future::try_select(await_connection, abort_handler).await {
        Ok(Either::Left((proxy_if_successful, resume_watcher))) => {
          println!("Connected- dropping resumption of watcher");
          std::mem::drop(resume_watcher);
          println!("Watcher dropped");
          // Ok(proxy_if_successful)
          todo!()
        }
        Ok(Either::Right(_)) => Err(anyhow::Error::msg("Timeout awaiting connection to proxy")),
        Err(Either::Left((e, _))) => Err(e).context("Failure trying to connect to proxy"),
        Err(Either::Right((e, _))) => {
          Err(e).context("Failure in source stream while connecting to proxy")
        }
      };
    proxy_connect_res
  };
  let proxy_res = match proxy_connect_res {
    Ok(proxy) => {
      println!("Beginning proxying...");
      util::proxy_tcp_streams(source, proxy).await
    }
    Err(e) => Err(e),
  };
  if let Err(e) = proxy_res {
    eprintln!(
      "Proxy execution from port {} to {:?} failed with error:\n{:#?}",
      listen_port.port(),
      proxy_target,
      e
    );
  }
  println!("Closed connection on port {}", listen_port.port());
  Ok(())
}

// Accept connections from a TCP socket and forward them to new connections over AXL
// Watch for failures on BuildConnection, which is responsible for timeout logic if needed
async fn accept_loop(
  listener: &mut TcpListener,
  addr: &SocketAddr,
  build_connection: &ProxyConnectionProvider<'_, '_, '_>,
) -> Result<()> {
  use async_std::prelude::*;
  use futures::stream::{self, FuturesUnordered, StreamExt, TryStreamExt};
  listener
    .incoming()
    .map_err(|e| e.into())
    .try_for_each_concurrent(None, async move |stream| -> Result<()> {
      Ok(
        handle_connection(stream, addr.clone(), build_connection)
          .await
          .context("Error handling connection")?,
      )
    })
    .await
    .context("Failure running acceptance loop")?;
  Ok(())
}

type ProxyConnectionProvider<'a, 'b, 'c> = dyn Fn(
  SocketAddr, // Peer address
) -> future::BoxFuture<
  'a,
  (
    MetaStreamHeader,
    future::BoxFuture<'b, Result<&'c quinn::NewConnection>>,
  ),
>;

#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone)]
#[repr(transparent)]
pub struct AxlClientIdentifier(String);

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
  Failure(AxlClientIdentifier, SocketAddr, String),
}

// pub trait TunnelManager<'stream, U: Stream<Item = quinn::NewConnection> + 'stream> {
//   fn handle_incoming<'a>(
//     &'a self,
//     stream: U,
//     shutdown_notifier: triggered::Listener,
//   ) -> futures::stream::BoxStream<'b, TunnelServerEvent>;
// }
//
// impl TunnelManager<stream::BoxStream<'_, quinn::NewConnection>> for TcpRangeBindingTunnelServer {
//   fn handle_incoming<'a, 'b : 'a>(
//     &'a self,
//     stream: stream::BoxStream<'b, quinn::NewConnection>,
//     shutdown_notifier: triggered::Listener,
//   ) -> stream::BoxStream<'b, TunnelServerEvent> {
//     TcpRangeBindingTunnelServer::handle_incoming(self, stream, shutdown_notifier)
//   }
// }

struct TcpConnection<'a> {
  port: u16,
  addr: SocketAddr,
  id: AxlClientIdentifier,
  future: BoxFuture<'a, Result<()>>,
}

impl<'a> TcpConnection<'a> {
  fn new(
    addr: SocketAddr,
    id: AxlClientIdentifier,
    future: BoxFuture<'a, Result<()>>,
  ) -> TcpConnection<'a> {
    TcpConnection {
      id,
      port: addr.port(),
      addr,
      future,
    }
  }
}

impl Future for TcpConnection<'_> {
  type Output = Result<(AxlClientIdentifier, SocketAddr)>;

  // TODO: Lift from res<(id, addr)> to (id, addr, res<()>)
  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let (id, addr) = (self.id.clone(), self.addr);
    let f = unsafe { self.map_unchecked_mut(|x| &mut x.future) };
    let res = futures::ready!(f.poll(cx));
    Poll::Ready(res.map(|_| (id, addr)))
  }
}

pub struct TcpRangeBindingTunnelServer {
  range: std::ops::RangeInclusive<u16>,
  bind_ip: IpAddr,
  bound_ports: Arc<Mutex<std::collections::HashSet<u16>>>,
}

impl TcpRangeBindingTunnelServer {
  pub fn new<T: Into<u16>>(
    bind_port_range: std::ops::RangeInclusive<T>,
    bind_ip: IpAddr,
  ) -> TcpRangeBindingTunnelServer {
    let (start, end): (u16, u16) = {
      let (a, b) = bind_port_range.into_inner();
      (a.into(), b.into())
    };
    TcpRangeBindingTunnelServer {
      range: std::ops::RangeInclusive::new(start, end),
      bind_ip,
      bound_ports: Default::default(),
    }
  }

  pub fn handle_incoming<'a>(
    &'a self,
    stream: impl futures::stream::Stream<Item = quinn::NewConnection> + Send + 'a,
    shutdown_notifier: triggered::Listener,
  ) -> futures::stream::BoxStream<'a, TunnelServerEvent> {
    use futures::stream::{BoxStream, TryStreamExt};
    let streams: BoxStream<BoxStream<TunnelServerEvent>> = stream
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
      .and_then(async move |(mut conn, mut notifier): (quinn::NewConnection, triggered::Listener)| -> Result<stream::BoxStream<'_, TunnelServerEvent>> {
        let addr = conn.connection.remote_address();
        // TODO: Build identifier based on handshake / authentication session
        use async_stream::stream as sgen;

        Ok(sgen! {
          yield TunnelServerEvent::Open(addr);
          let id = self.handle_identification(&mut conn, &mut notifier).await.unwrap();// TODO: Fallible
          yield TunnelServerEvent::Identified(id.clone(), addr);
          let handler_id = id.clone();
          let res =
            self.handle_connection(id.clone(), conn, notifier)
              .map(move |x| match x {
                Err(e) => TunnelServerEvent::Failure(handler_id.clone(), addr, e.to_string()),
                Ok(res) => TunnelServerEvent::Close(handler_id.clone(), addr)
              }).await;
          yield res;
        }.boxed())
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
      .boxed()
      ;

    util::merge_streams(streams)
  }

  async fn handle_identification(
    &self,
    tunnel: &mut quinn::NewConnection,
    shutdown_notifier: &mut triggered::Listener,
  ) -> Result<AxlClientIdentifier> {
    Ok(AxlClientIdentifier(
      tunnel.connection.remote_address().to_string(),
    ))
  }

  fn handle_connection(
    &self,
    id: AxlClientIdentifier,
    tunnel: quinn::NewConnection,
    shutdown_notifier: triggered::Listener,
  ) -> BoxFuture<Result<bool>> {
    let port_range = self.range.clone();
    let bind_ip = self.bind_ip;
    let bound_ports = self.bound_ports.clone();

    async move {
      // TODO: register a connection *only after* session authentication (make an async authn trait)
      let next_port: u16 = {
        let lock = bound_ports.lock().await;
        port_range
          .into_iter()
          .filter(|test_port| !lock.contains(test_port))
          .min()
          .ok_or_else(|| {
            AnyErr::msg(format!(
              "No free ports available in range {:?}",
              &self.range
            ))
          })?
      };
      let bind_addr = SocketAddr::new(bind_ip, next_port);
      println!("Bound client on address {:?}", bind_addr);

      // TODO: Handle connection

      // {
      //   use futures::AsyncWriteExt;
      //   let (mut send, _recv) = tunnel.connection.open_bi().await?;
      //   send.write(&[42u8]).await?;
      //   send.finish();
      //   send.close().await;
      // }

      tunnel
        .connection
        .close(quinn::VarInt::from_u32(42), "Fake handler error".as_bytes());

      Err(AnyErr::msg("Fake handler error"))
      // Ok(true)
    }
    .fuse()
    .boxed()
  }
}

pub async fn server_main(config: self::ServerArgs) -> Result<()> {
  let quinn_config = build_quinn_config(&config)?;
  let (_endpoint, incoming) = {
    let mut endpoint = quinn::Endpoint::builder();
    endpoint.listen(quinn_config);
    endpoint.bind(&config.quinn_bind_addr)?
  };

  let mut manager/*: Box<dyn TunnelManager<_>>*/ = Box::new(TcpRangeBindingTunnelServer::new(
    config.tcp_bind_port_range,
    config.tcp_bind_ip,
  ));

  use futures::stream::TryStreamExt;
  let (trigger_shutdown, shutdown_notifier) = triggered::trigger();
  let connections: stream::BoxStream<'_, quinn::NewConnection> = incoming
    .take_until(shutdown_notifier.clone())
    .map(|x| -> Result<_> { Ok(x) })
    .and_then(async move |connecting| {
      // When a new connection arrives, establish the connection formally, and pass it on
      let tunnel = connecting.await?;
      // TODO: Protocol header can occur here, or as part of the later "binding" phase
      // It can also be built as an isomorphic middleware intercepting a TryStream of NewConnection
      Ok(tunnel)
    })
    .inspect_err(|e| {
      eprintln!("Connection failure during stream pickup: {:#?}", e);
    })
    .filter_map(async move |x| x.ok()) // only keep the successful connections
    .boxed();

  let mut events = manager
    .handle_incoming(connections, shutdown_notifier.clone())
    .fuse();
  {
    let mut signal_watcher = async_signals::Signals::new(vec![libc::SIGINT])?;
    let signal_watcher = signal_watcher
      .filter(|&x| future::ready(x == libc::SIGINT))
      .take(1)
      .map(|_| {
        println!("\nShutdown triggered");
        // Tell manager to start shutting down tunnels; new adoption requests should return errors
        trigger_shutdown.trigger();
        None
      })
      .fuse();
    futures::stream::select(signal_watcher, events.map(|e| Some(e)))
      .filter_map(|x| future::ready(x))
      .for_each(async move |ev| {
        println!("Event: {:#?}", ev);
      })
      .await;
  }

  Ok(())
}

fn build_quinn_config(config: &ServerArgs) -> Result<quinn::ServerConfig> {
  let cert_der = std::fs::read(&config.cert).context("Failed reading cert file")?;
  let priv_der = std::fs::read(&config.key).context("Failed reading private key file")?;
  let priv_key =
    quinn::PrivateKey::from_der(&priv_der).context("Quinn .der parsing of private key failed")?;
  let mut config = quinn::ServerConfigBuilder::default();
  config.use_stateless_retry(true);
  let mut transport_config = TransportConfig::default();
  transport_config.stream_window_uni(0);
  transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(15)));
  let mut server_config = quinn::ServerConfig::default();
  server_config.transport = Arc::new(transport_config);
  let mut cfg_builder = quinn::ServerConfigBuilder::new(server_config);
  cfg_builder.protocols(util::ALPN_QUIC_HTTP);
  let cert = quinn::Certificate::from_der(&cert_der)?;
  cfg_builder.certificate(quinn::CertificateChain::from_certs(vec![cert]), priv_key)?;
  Ok(cfg_builder.build())
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
