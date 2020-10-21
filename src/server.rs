use crate::common::MetaStreamHeader;
use crate::util::{self, parse_ipaddr, parse_port_range, parse_socketaddr};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use async_std::sync::{Arc, Mutex};
use futures::future::*;
use futures::{
  future,
  pin_mut,
  select_biased,
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

type ProxyConnectionProvider<'a, 'b, 'c: 'b> = dyn Fn(
  SocketAddr, // Peer address
) -> future::BoxFuture<
  'a,
  (
    MetaStreamHeader,
    future::BoxFuture<'b, Result<&'c quinn::NewConnection>>,
  ),
>;

#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Debug)]
#[repr(transparent)]
struct AxlClientIdentifier(String);

#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Debug)]
pub enum TunnelServerEvent {}

pub trait TunnelManager<U: Stream<Item = quinn::NewConnection>> {
  fn handle_incoming(
    &self,
    stream: U,
    shutdown_notifier: triggered::Listener,
  ) -> futures::stream::BoxStream<TunnelServerEvent>;
}

impl TunnelManager<stream::BoxStream<'_, quinn::NewConnection>> for TcpRangeBindingTunnelServer {
  fn handle_incoming(
    &self,
    stream: stream::BoxStream<'_, quinn::NewConnection>,
    shutdown_notifier: triggered::Listener,
  ) -> stream::BoxStream<'_, TunnelServerEvent> {
    TcpRangeBindingTunnelServer::handle_incoming(self, stream, shutdown_notifier)
  }
}

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
}

impl TcpRangeBindingTunnelServer {
  fn new<T: Into<u16>>(
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
    }
  }

  fn handle_incoming(
    &self,
    stream: impl futures::stream::Stream<Item = quinn::NewConnection>,
    shutdown_notifier: triggered::Listener,
  ) -> futures::stream::BoxStream<TunnelServerEvent> {
    stream::unfold(shutdown_notifier, async move |notif| {
      notif.await;
      println!("Graceful shutdown of handler...");
      None
    }).boxed()
  }

  /*
  fn try_adopt_tunnel<'a, 'b: 'a>(
    &'a mut self,
    tunnel: quinn::NewConnection,
  ) -> BoxFuture<'b, Result<bool>> {
    if self.shutdown_in_progress {
      // TODO: "Not accepting new tunnel connections because of impending shutdown" message
      return future::ready(Ok(false)).boxed();
    }
    let shutdown_notifier = self.shutdown_notifiers.1.clone();
    let connections = self.connections.clone();
    let port_range = self.range.clone();
    let bind_ip = self.bind_ip;
    let id = AxlClientIdentifier(tunnel.connection.remote_address().to_string());

    async move {
      // TODO: register a connection *only after* session authentication (make an async authn trait)
      let next_port: u16 = {
        let lock = connections.lock().await;
        port_range.into_iter()
          .filter(|&test_port| !lock.values().any(|v| v.port == test_port))
          .min()
          .ok_or_else(|| AnyErr::msg(format!("No free ports available in range {:?}", &self.range)))?
      };
      let bind_addr = SocketAddr::new(bind_ip, next_port);
      let handler = async move {
        todo!(); // Handler code here
      }.fuse().boxed();
      let connection = TcpConnection::new(bind_addr, id.clone(), handler);
      {
        let mut lock = connections.lock().await;
        // If the shutdown notifier was triggered between now and the above check, the handler
        // will immediately complete an await upon it, gracefully closing the connection
        lock.insert(id, connection);
      }
      Ok(true)
    }.fuse().boxed()
  }
  */
}

pub async fn server_main(config: self::ServerArgs) -> Result<()> {
  let quinn_config = build_quinn_config(&config)?;
  let (_endpoint, incoming) = {
    let mut endpoint = quinn::Endpoint::builder();
    endpoint.listen(quinn_config);
    endpoint.bind(&config.quinn_bind_addr)?
  };

  let mut manager: Box<dyn TunnelManager<_>> = Box::new(TcpRangeBindingTunnelServer::new(
    config.tcp_bind_port_range,
    config.tcp_bind_ip,
  ));

  use futures::stream::TryStreamExt;
  let connections: stream::BoxStream<'_, quinn::NewConnection> = incoming
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

  let (trigger_shutdown, shutdown_notifier) = triggered::trigger();
  let mut events = manager
    .handle_incoming(connections, shutdown_notifier.clone()).fuse();
  {
    let mut signal_watcher = async_signals::Signals::new(vec![libc::SIGINT])?;
    let sigint_watcher = signal_watcher.next().fuse();
    pin_mut!(sigint_watcher);
    loop {
      select_biased! {
        // Wait for SIGINT; begin graceful shutdown if we receive one
        signal = sigint_watcher => {
          match signal {
            None => { println!("`None` returned from signal watcher??"); },
            Some(s) => {
              assert_eq!(s, libc::SIGINT);
              println!("\nShutdown triggered");
              // Tell manager to start shutting down tunnels; new adoption requests should return errors
              trigger_shutdown.trigger();
            }
          }
        }
        ev = events.next() => {
          match ev {
            None => break, // Stream ended; break, as we can ignore the shutdown handler now
            Some(e) => {
              // A stream event occurred- just print it for now
              println!("Event: {:#?}", e);
            }
          }
        }
        complete => break,
      }
    }
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
