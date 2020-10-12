use crate::common::MetaStreamHeader;
use crate::util::{self, parse_ipaddr, parse_port_range, parse_socketaddr};
use anyhow::{Context as AnyhowContext, Error as AnyErr, Result};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use futures::future::*;
use futures::{future, StreamExt};
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  boxed::Box,
  path::{Path, PathBuf},
  pin::Pin,
  sync::Arc,
  task::{Context, Poll},
};

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ServerArgs {
  pub cert: PathBuf,
  pub key: PathBuf,
  pub quinn_bind_ip: std::net::SocketAddr,
  pub tcp_bind_ip: std::net::IpAddr,
  pub tcp_bind_port_range: std::ops::RangeInclusive<u16>,
}

pub async fn server_arg_handling(args: &'_ clap::ArgMatches<'_>) -> Result<ServerArgs> {
  let cert_path = Path::new(args.value_of("cert").unwrap()).to_path_buf();
  let key_path = Path::new(args.value_of("key").unwrap()).to_path_buf();

  Ok(ServerArgs {
    cert: cert_path,
    key: key_path,
    quinn_bind_ip: parse_socketaddr(args.value_of("quic").unwrap())?,
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

pub trait AxlHandler<T: Sized + Send> {
  fn establish_tunnel<'a>(
    tunnel: quinn::NewConnection,
  ) -> BoxFuture<'a, Result<(quinn::NewConnection, T)>>;
  // fn establish_session(tunnel: quinn::NewConnection) -> BoxFuture<Result<quinn::NewConnection>>;
}

pub trait AxlServer {
  fn accept_connections<'a, 'b: 'a, 'c: 'b, T: Sized + Send>(
    &'a mut self,
    handler: &'b mut impl AxlHandler<T>,
  ) -> BoxFuture<'c, Result<()>>;
}

struct DeferredAxlServer {
  endpoint: quinn::Endpoint,
  incoming: quinn::Incoming,
}

impl DeferredAxlServer {
  fn new(quinn_config: quinn::ServerConfig, bind_addr: &SocketAddr) -> Result<DeferredAxlServer> {
    let (endpoint, mut incoming) = {
      let mut endpoint = quinn::Endpoint::builder();
      endpoint.listen(quinn_config);
      endpoint.bind(bind_addr)?
    };

    Ok(DeferredAxlServer { endpoint, incoming })
  }
}

impl AxlServer for DeferredAxlServer {
  fn accept_connections<'a, 'b: 'a, 'c: 'b, T: Sized + Send>(
    &'a mut self,
    handler: &'b mut impl AxlHandler<T>,
  ) -> BoxFuture<'c, Result<()>> {
    todo!()
  }
}

#[repr(transparent)]
struct AxlClientIdentifier(String);

// impl WebApi {
//   pub fn request_port_mapping(&mut self, client: AxlClientIdentifier) -> () {
//     todo!()
//   }
// }

pub trait TunnelManager<'connection> {
  fn try_adopt_tunnel<'a, 'b: 'a>(
    &'a mut self,
    tunnel: quinn::NewConnection,
  ) -> BoxFuture<'b, Result<bool>>;
  fn shutdown<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;
}

pub struct TcpRangeBindingTunnelServer {
  range: std::ops::RangeInclusive<u16>,
  shutdown_in_progress: bool,
}

impl TcpRangeBindingTunnelServer {
  fn new<T: Into<u16>>(
    bind_port_range: std::ops::RangeInclusive<T>,
  ) -> TcpRangeBindingTunnelServer {
    let (start, end): (u16, u16) = {
      let (a, b) = bind_port_range.into_inner();
      (a.into(), b.into())
    };
    TcpRangeBindingTunnelServer {
      range: std::ops::RangeInclusive::new(start, end),
      shutdown_in_progress: false,
    }
  }
}

impl<'connection> TunnelManager<'connection> for TcpRangeBindingTunnelServer {
  fn try_adopt_tunnel<'a, 'b: 'a>(
    &'a mut self,
    tunnel: quinn::NewConnection,
  ) -> BoxFuture<'b, Result<bool>> {
    todo!()
  }

  // After shutdown, new adoption requests should immediately return a future resolving to Ok(false)
  fn shutdown(&mut self) -> BoxFuture<Result<()>> {
    if self.shutdown_in_progress {
      return futures::future::ready(Err(AnyErr::msg("Multiple calls to shutdown occurred")))
        .boxed();
    }
    self.shutdown_in_progress = true;

    todo!()
  }
}

pub async fn server_main(config: self::ServerArgs) -> Result<()> {
  let quinn_config = build_quinn_config(&config)?;
  let (_endpoint, mut incoming) = {
    let mut endpoint = quinn::Endpoint::builder();
    endpoint.listen(quinn_config);
    endpoint.bind(&config.quinn_bind_ip)?
  };

  let mut manager: Box<dyn TunnelManager> =
    Box::new(TcpRangeBindingTunnelServer::new(config.tcp_bind_port_range));

  use futures::stream::{self, FuturesUnordered, StreamExt, TryStream, TryStreamExt};
  // Fold is used to weave the manager reference into each closure since it
  // otherwise worries about multiple potentially-concurrent mut references
  incoming
    .map(|x| -> Result<_> { Ok(x) })
    .try_fold(&mut manager, async move |manager, connecting| {
      let tunnel = connecting.await?;
      // Pass tunnel ownership to the manager
      manager.try_adopt_tunnel(tunnel).await?; // TODO: Disconnect failing connections
      // tunnel.connection.close()

      Ok(manager)
    })
    .await?;

  // Wait for SIGINT; begin graceful shutdown if we receive one
  {
    // Wrap it in brackets to destroy the watcher afterward, so a second ctrl-c ends the process immediately
    let mut signal_watcher = async_signals::Signals::new(vec![libc::SIGINT])?;
    let signal = signal_watcher.next().await.unwrap();
    assert_eq!(signal, libc::SIGINT);
  }

  // Tell manager to start shutting down tunnels; new adoption requests should return errors
  manager.shutdown().await?;
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
