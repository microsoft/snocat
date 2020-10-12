use crate::util::{self, parse_socketaddr};
use anyhow::{Error as AnyErr, Result, Context as AnyhowContext};
use futures::future;
use futures::future::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  path::{Path, PathBuf},
  sync::Arc,
  task::{Context, Poll},
};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};


#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ServerArgs {
  pub cert: PathBuf,
  pub key: PathBuf,
  pub quinn_bind_ip: std::net::SocketAddr,
  pub tcp_bind_ip: std::net::SocketAddr,
}

pub async fn server_arg_handling(args: &'_ clap::ArgMatches<'_>) -> Result<ServerArgs> {
  let cert_path = Path::new(args.value_of("cert").unwrap()).to_path_buf();
  let key_path = Path::new(args.value_of("key").unwrap()).to_path_buf();

  Ok(ServerArgs {
    cert: cert_path,
    key: key_path,
    quinn_bind_ip: parse_socketaddr(args.value_of("quic").unwrap())?,
    tcp_bind_ip: parse_socketaddr(args.value_of("tcp").unwrap())?,
  })
}
type ProxyConnectionProvider<'a, 'b> =
dyn Fn(
  SocketAddr,
) -> future::BoxFuture<'a, (SocketAddr, future::BoxFuture<'b, Result<TcpStream>>)>;

async fn handle_connection(
  source: TcpStream,
  listen_port: SocketAddr,
  build_proxy_connection: &ProxyConnectionProvider<'_, '_>,
) -> Result<()> {
  println!("Received connection from port {}", listen_port.port());
  use anyhow::Error;
  use async_std::prelude::*;
  use futures::future::BoxFuture;
  use futures::stream::{self, FuturesUnordered, StreamExt, TryStreamExt};
  use std::{boxed::Box, pin::Pin};
  let (proxy_target, await_connection) = build_proxy_connection(source.peer_addr().unwrap()).await;
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
    let watcher: Pin<Box<BoxFuture<Result<(), Error>>>> = Box::pin(
      util::PollerVortex::new(util::async_tcpstream_as_evented_fd(
        &source,
        &mut watcher_fd_holder,
      ))
        .map(|r| r.context("Client disconnected"))
        .fuse()
        .boxed(),
    );
    use futures::future::{AbortHandle, AbortRegistration, Abortable, FutureExt};
    let abort_handler: Pin<Box<BoxFuture<Result<(), Error>>>> = Box::pin(
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
          Ok(proxy_if_successful)
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

async fn accept_loop(
  listener: &mut TcpListener,
  addr: &SocketAddr,
  build_connection: &ProxyConnectionProvider<'_, '_>,
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

pub async fn server_main(config: self::ServerArgs) -> Result<()> {
  let quinn_config = {
    let cert_der = std::fs::read(config.cert).context("Failed reading cert file")?;
    let priv_der = std::fs::read(config.key).context("Failed reading private key file")?;
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
    cfg_builder.build()
  };

  let res: Result<()> = async {
    let mut listener = TcpListener::bind(SocketAddr::new(
      IpAddr::from(Ipv4Addr::new(127, 0, 0, 1)),
      8080,
    ))
      .await?;
    let local_addr = listener
      .local_addr()
      .context("Failed to get local address for socket")?;
    accept_loop(&mut listener, &local_addr, &|_peer| {
      // let addr = SocketAddr::new(IpAddr::from(Ipv4Addr::new(213,136,8,188)), 23); // Blinkenlights
      let addr = SocketAddr::new(IpAddr::from(Ipv4Addr::new(216, 58, 217, 46)), 80);
      async move {
        (
          addr,
          TcpStream::connect(addr)
            .map_err(|e| e.into())
            .fuse()
            .boxed(),
        )
      }
        .boxed()
    })
      .await?;
    Ok(())
  }.await;
  res
  // Err(anyhow::Error::msg(format!(
  //   "Not implemented (config is {:#?})",
  //   &config
  // )))
}

