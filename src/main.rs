#![feature(nll)]
#![feature(async_closure)]
#![allow(unused_imports)]

use anyhow::{anyhow, bail, Context as AnyhowContext, Result};
use clap::{App, Arg, SubCommand};
// #[macro_use]
use async_std::io::{BufReader, BufWriter};
use async_std::net::{IpAddr, Ipv4Addr, SocketAddr};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use async_std::prelude::*;
use futures::future::Either;
use futures::{self, Future, FutureExt, *};
use quinn::TransportConfig;
use std::{
  path::{Path, PathBuf},
  sync::Arc,
  task::{Context, Poll},
};
use tokio::io::PollEvented;
use tokio::runtime::Runtime;
use tracing::{error, info, info_span};
use tracing_futures::Instrument as _;

fn validate_existing_file(v: String) -> Result<(), String> {
  if !Path::new(&v).exists() {
    Err(String::from("A file must exist at the given path"))
  } else {
    Ok(())
  }
}

fn parse_socketaddr(v: &str) -> Result<SocketAddr> {
  use std::convert::TryFrom;
  use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
  ToSocketAddrs::to_socket_addrs(v)
    .map_err(|e| e.into())
    .and_then(|mut items| {
      items.nth(0).ok_or(anyhow::Error::msg(
        "No addresses were resolved from the given host",
      ))
    })
    .into()
}

fn validate_socketaddr(v: String) -> Result<(), String> {
  parse_socketaddr(&v).map(|_| ()).map_err(|e| e.to_string())
}

mod util;

fn main() {
  let app = App::new(env!("CARGO_PKG_NAME"))
    .version(env!("CARGO_PKG_VERSION"))
    .about(env!("CARGO_PKG_DESCRIPTION"))
    .subcommand(
      SubCommand::with_name("client")
        .alias("-c")
        .about("Bind a local port to a remote server")
        // .arg(Arg::with_name("client-cert").long("client-cert").short("c").validator(validate_existing_file).takes_value(true))
        .arg(
          Arg::with_name("authority")
            .long("authority")
            .short("a")
            .validator(validate_existing_file)
            .takes_value(true)
            .required(true),
        )
        .arg(
          Arg::with_name("driver")
            .long("driver")
            .short("d")
            .validator(validate_socketaddr)
            .takes_value(true)
            .required(true),
        )
        .arg(
          Arg::with_name("target")
            .long("target")
            .short("t")
            .validator(validate_socketaddr)
            .takes_value(true)
            .required(true),
        ),
    )
    .subcommand(
      SubCommand::with_name("server")
        .alias("-s")
        .about("Run in server mode, supporting connections from multiple clients")
        .arg(
          Arg::with_name("cert")
            .long("cert")
            .short("c")
            .validator(validate_existing_file)
            .takes_value(true)
            .required(true),
        )
        .arg(
          Arg::with_name("key")
            .long("key")
            .short("k")
            .validator(validate_existing_file)
            .takes_value(true)
            .required(true),
        )
        // Port that will accept traffic to be forwarded to clients
        .arg(
          Arg::with_name("tcp")
            .long("bind")
            .short("b")
            .validator(validate_socketaddr)
            .default_value("127.0.0.1:8080")
            .takes_value(true)
            .required(true),
        )
        // PPort that will accept tunneling clients to receive forwarded connections
        .arg(
          Arg::with_name("quic")
            .long("quic")
            .short("q")
            .validator(validate_socketaddr)
            .default_value("127.0.0.1:9090")
            .takes_value(true)
            .required(true),
        ),
    )
    .subcommand(
      SubCommand::with_name("cert")
        .about("Generate self-signed certificates for local usage")
        .arg(Arg::with_name("path").takes_value(true).required(true))
        .arg(
          Arg::with_name("san")
            .long("san")
            .takes_value(true)
            .required(false)
            .default_value("localhost"),
        ),
    )
    .setting(clap::AppSettings::SubcommandRequiredElseHelp);
  let matches = app.get_matches();
  let mode = matches.subcommand_name().unwrap_or("<No subcommand?>");
  match async_std::task::block_on(main_args_handler(&matches)) {
    Err(err) => eprintln!("{0} failed with error:\n{1:#?}", mode, err),
    Ok(_) => println!("{} exited successfully", mode),
  }
}

async fn main_args_handler(matches: &'_ clap::ArgMatches<'_>) -> Result<()> {
  match matches.subcommand() {
    ("server", Some(opts)) => {
      let config = server_arg_handling(opts).await?;
      println!("Running as server with config {:#?}", &config);
      server_main(config).await
    }
    ("client", Some(opts)) => {
      let config = client_arg_handling(opts).await?;
      println!("Running as client with config {:#?}", &config);
      client_main(config).await
    }
    ("cert", Some(opts)) => {
      println!("Generating certs...");
      let path_raw = opts.value_of("path").expect("Path argument is required");
      let san = opts.value_of("san").expect("SAN argument must exist");
      certgen_main(path_raw.into(), san.into()).await
    }
    (_, _) => unreachable!(),
  }
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ServerArgs {
  pub cert: PathBuf,
  pub key: PathBuf,
  pub quinn_bind_ip: std::net::SocketAddr,
  pub tcp_bind_ip: std::net::SocketAddr,
}

async fn server_arg_handling(args: &'_ clap::ArgMatches<'_>) -> Result<ServerArgs> {
  let cert_path = Path::new(args.value_of("cert").unwrap()).to_path_buf();
  let key_path = Path::new(args.value_of("key").unwrap()).to_path_buf();

  Ok(ServerArgs {
    cert: cert_path,
    key: key_path,
    quinn_bind_ip: parse_socketaddr(args.value_of("quic").unwrap())?,
    tcp_bind_ip: parse_socketaddr(args.value_of("tcp").unwrap())?,
  })
}

fn async_tcpstream_as_evented_fd<'a>(
  stream: &'a async_std::net::TcpStream,
  fd_holder: &'a mut i32,
) -> PollEvented<mio::unix::EventedFd<'a>> {
  use std::os::unix::io::AsRawFd;
  *fd_holder = stream.as_raw_fd();
  let evented = mio::unix::EventedFd(fd_holder);
  PollEvented::new(evented).unwrap()
  // PollEvented::new_with_ready(evented, mio::unix::UnixReady::hup().into()).unwrap()
}

struct PollerVortex<'a> {
  poll_evented: PollEvented<mio::unix::EventedFd<'a>>,
}

impl Future for PollerVortex<'_> {
  type Output = Result<(), io::Error>;

  fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    self.get_mut().poll_accept(cx)
  }
}

impl PollerVortex<'_> {
  pub fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
    use mio::{unix::UnixReady, Evented, Ready};
    let mut ready = Ready::readable();
    ready.insert(mio::unix::UnixReady::hup());
    ready.insert(Ready::from_usize(0b00_1000));
    ready.insert(mio::unix::UnixReady::error());

    print!("Polling readiness...");
    let poll_res = self.poll_evented.poll_read_ready(cx, ready);
    ready.remove(mio::unix::UnixReady::hup());
    ready.remove(mio::unix::UnixReady::error());
    println!("   Result was {:#?}", poll_res);
    match poll_res {
      Poll::Ready(Err(err)) if err.kind() == io::ErrorKind::WouldBlock => {
        self.poll_evented.clear_read_ready(cx, ready)?;
        Poll::Pending
      }
      Poll::Ready(Ok(ok)) => {
        let ok = mio::unix::UnixReady::from(ok);
        println!("Ok with ready-state {:#?} (is hup? {})", ok, ok.is_hup());
        self.poll_evented.clear_read_ready(cx, ready)?;
        Poll::Pending
      }
      Poll::Ready(Err(err)) => {
        println!("Error with ready-state {:#?}", err);
        self.poll_evented.clear_read_ready(cx, ready)?;
        Poll::Pending
      }
      Poll::Pending => Poll::Pending,
    }
  }
}

async fn proxy_tcp_streams(mut source: TcpStream, mut proxy: TcpStream) -> Result<()> {
  let (mut reader, mut writer) = (&mut source).split();
  let (mut proxy_reader, mut proxy_writer) = (&mut proxy).split();
  let proxy_i2o = Box::pin(async_std::io::copy(&mut reader, &mut proxy_writer).fuse());
  let proxy_o2i = Box::pin(async_std::io::copy(&mut proxy_reader, &mut writer).fuse());
  let res: Either<(), ()> = match futures::future::try_select(proxy_i2o, proxy_o2i).await {
    Ok(Either::Left((_i2o, resume_o2i))) => {
      println!("Source connection closed gracefully, shutting down proxy");
      std::mem::drop(resume_o2i); // Kill the copier, allowing us to send end-of-connection
      Either::Right(())
    }
    Ok(Either::Right((_o2i, resume_i2o))) => {
      println!("Proxy connection closed gracefully, shutting down source");
      std::mem::drop(resume_i2o); // Kill the copier, allowing us to send end-of-connection
      Either::Left(())
    }
    Err(Either::Left((e_i2o, resume_o2i))) => {
      println!(
        "Source connection died with error {:#?}, shutting down proxy connection",
        e_i2o
      );
      std::mem::drop(resume_o2i); // Kill the copier, allowing us to send end-of-connection
      Either::Right(())
    }
    Err(Either::Right((e_o2i, resume_i2o))) => {
      println!(
        "Proxy connection died with error {:#?}, shutting down source connection",
        e_o2i
      );
      std::mem::drop(resume_i2o); // Kill the copier, allowing us to send end-of-connection
      Either::Left(())
    }
  };
  std::mem::drop(reader);
  std::mem::drop(writer);
  std::mem::drop(proxy_reader);
  std::mem::drop(proxy_writer);
  match res {
    Either::Left(_) => {
      if let Err(shutdown_failure) = source.shutdown(async_std::net::Shutdown::Both) {
        eprintln!(
          "Failed to shut down source connection with error:\n{:#?}",
          shutdown_failure
        );
      }
    }
    Either::Right(_) => {
      if let Err(shutdown_failure) = proxy.shutdown(async_std::net::Shutdown::Both) {
        eprintln!(
          "Failed to shut down proxy connection with error:\n{:#?}",
          shutdown_failure
        );
      }
    }
  }
  Ok(())
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
      PollerVortex {
        poll_evented: async_tcpstream_as_evented_fd(&source, &mut watcher_fd_holder),
      }
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
      println!("Converting back to an async stream...");
      // stream = unsafe { tcp_stream_to_async(sync_stream) };
      println!("Beginning proxying...");
      proxy_tcp_streams(source, proxy).await
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

async fn server_main(config: ServerArgs) -> Result<()> {
  let quinn_config = {
    let cert_der = std::fs::read(config.cert).context("Failed reading cert file")?;
    let priv_der = std::fs::read(config.key).context("Failed reading private key file")?;
    let priv_key =
      quinn::PrivateKey::from_der(&priv_der).context("Quinn .der parsing of private key failed")?;
    let mut config = quinn::ServerConfigBuilder::default();
    config.use_stateless_retry(true);
    let mut transport_config = TransportConfig::default();
    transport_config.stream_window_uni(0);
    let mut server_config = quinn::ServerConfig::default();
    server_config.transport = Arc::new(transport_config);
    let mut cfg_builder = quinn::ServerConfigBuilder::new(server_config);
    let cert = quinn::Certificate::from_der(&cert_der)?;
    cfg_builder.certificate(quinn::CertificateChain::from_certs(vec![cert]), priv_key)?;
    cfg_builder.build()
  };

  let mut runtime = Runtime::new().unwrap();
  let res: Result<()> = runtime.block_on(async {
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
  });
  res
  // Err(anyhow::Error::msg(format!(
  //   "Not implemented (config is {:#?})",
  //   &config
  // )))
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct ClientArgs {
  pub authority_cert: PathBuf,
  pub driver_host: std::net::SocketAddr,
  pub proxy_target_host: std::net::SocketAddr,
}

async fn client_arg_handling(args: &'_ clap::ArgMatches<'_>) -> Result<ClientArgs> {
  let cert_path = Path::new(args.value_of("authority").unwrap()).to_path_buf();
  Ok(ClientArgs {
    authority_cert: cert_path,
    driver_host: parse_socketaddr(args.value_of("driver").unwrap())?,
    proxy_target_host: parse_socketaddr(args.value_of("target").unwrap())?,
  })
}

async fn client_main(config: ClientArgs) -> Result<()> {
  Ok(())
}

async fn certgen_main(output_base_path: String, host_san: String) -> Result<()> {
  use std::fs;
  use std::path::PathBuf;
  let path = PathBuf::from(output_base_path);
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).context("Directory creation must succeed for certs")?;
  }
  let cert =
    rcgen::generate_simple_self_signed(vec![host_san]).context("Certificate generation failed")?;
  let cert_der = cert.serialize_der().unwrap();
  let priv_der = cert.serialize_private_key_der();
  fs::write(
    path.with_file_name(path.file_name().unwrap().to_str().unwrap().to_string() + ".pub.der"),
    &cert_der,
  )
  .context("Failed writing public key")?;
  fs::write(
    path.with_file_name(path.file_name().unwrap().to_str().unwrap().to_string() + ".priv.der"),
    &priv_der,
  )
  .context("Failed writing private key")?;
  Ok(())
}

#[cfg(test)]
mod tests {
  #[async_std::test]
  async fn stream_one_byte() {
    use async_std;
  }
}
