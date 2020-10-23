#[allow(dead_code)]
use anyhow::{Error as AnyErr, Result};
use async_std::net::TcpStream;
use futures::future::*;
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::{self, SelectAll, Stream, StreamExt};
use futures::AsyncReadExt;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use std::boxed::Box;
use std::path::Path;
use std::task::{Context, Poll};
use std::{error::Error, net::SocketAddr, sync::Arc};
use tokio::io::PollEvented;

pub fn validate_existing_file(v: String) -> Result<(), String> {
  if !Path::new(&v).exists() {
    Err(String::from("A file must exist at the given path"))
  } else {
    Ok(())
  }
}

pub fn parse_socketaddr(v: &str) -> Result<SocketAddr> {
  use std::convert::TryFrom;
  use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
  ToSocketAddrs::to_socket_addrs(v)
    .map_err(|e| e.into())
    .and_then(|mut items| {
      items.nth(0).ok_or(AnyErr::msg(
        "No addresses were resolved from the given host",
      ))
    })
    .into()
}

pub fn parse_ipaddr(v: &str) -> Result<std::net::IpAddr> {
  use std::convert::TryFrom;
  use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
  match v.parse::<Ipv4Addr>() {
    Ok(addr) => Ok(addr.into()),
    Err(_) => match v.parse::<Ipv6Addr>() {
      Ok(addr) => Ok(addr.into()),
      Err(_) => Err(anyhow::Error::msg(
        "Could not parse input as ipv4 or ipv6 address",
      )),
    },
  }
}

pub fn parse_port_range(v: &str) -> Result<std::ops::RangeInclusive<u16>> {
  use std::convert::TryFrom;
  match v.split_once(':') {
    None => Err(AnyErr::msg("Could not match ':' in port range string")),
    Some((start, end)) => {
      let (start, end) = (start.parse::<u16>(), end.parse::<u16>());
      match (start, end) {
        (Ok(s), Ok(e)) => Ok(std::ops::RangeInclusive::new(s, e)),
        (Err(_), Err(_)) => Err(AnyErr::msg("Range components were not valid u16s")),
        (Err(_), _) => Err(AnyErr::msg("Range start component was not a valid u16")),
        (_, Err(_)) => Err(AnyErr::msg("Range end component was not a valid u16")),
      }
    }
  }
}

pub fn validate_socketaddr(v: String) -> Result<(), String> {
  parse_socketaddr(&v).map(|_| ()).map_err(|e| e.to_string())
}

pub fn validate_ipaddr(v: String) -> Result<(), String> {
  parse_ipaddr(&v).map(|_| ()).map_err(|e| e.to_string())
}

pub fn validate_port_range(v: String) -> Result<(), String> {
  parse_port_range(&v).map(|_| ()).map_err(|e| e.to_string())
}

pub async fn proxy_generic_streams<
  SenderA: AsyncWrite + Unpin,
  ReaderA: AsyncRead + Unpin,
  SenderB: AsyncWrite + Unpin,
  ReaderB: AsyncRead + Unpin,
>(
  (mut sender_a, mut reader_a): (&mut SenderA, &mut ReaderA),
  (mut sender_b, mut reader_b): (&mut SenderB, &mut ReaderB),
) -> Either<(), ()> {
  let proxy_i2o = Box::pin(async_std::io::copy(&mut reader_a, &mut sender_b).fuse());
  let proxy_o2i = Box::pin(async_std::io::copy(&mut reader_b, &mut sender_a).fuse());
  println!("Polling proxy streams...");
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
  res
}

pub async fn proxy_tcp_streams(mut source: TcpStream, mut proxy: TcpStream) -> Result<()> {
  let (mut reader, mut writer) = (&mut source).split();
  let (mut proxy_reader, mut proxy_writer) = (&mut proxy).split();
  let res = proxy_generic_streams(
    (&mut proxy_writer, &mut reader),
    (&mut writer, &mut proxy_reader),
  )
    .await;
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

pub async fn proxy_from_tcp_stream<
  Sender: AsyncWrite + Unpin,
  Reader: AsyncRead + Unpin,
>(mut source: TcpStream, (mut proxy_writer, mut proxy_reader): (Sender, Reader)) -> Result<()> {
  let (mut reader, mut writer) = (&mut source).split();
  let res = proxy_generic_streams(
    (&mut proxy_writer, &mut reader),
    (&mut writer, &mut proxy_reader),
  )
  .await;
  std::mem::drop(reader);
  std::mem::drop(writer);
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
      // Close proxy connection somehow?
    }
  }
  Ok(())
}

pub fn merge_streams<'a, T: 'a>(
  source: impl futures::stream::Stream<Item = stream::BoxStream<'a, T>> + 'a + std::marker::Send,
) -> stream::BoxStream<'a, T> {
  let mut source_empty = false;
  let mut source = Box::pin(source);
  let mut items = Box::pin(futures::stream::SelectAll::new());
  futures::stream::poll_fn(move |ctx| -> Poll<Option<T>> {
    let source_ref = source.as_mut();
    if !source_empty {
      match Stream::poll_next(source_ref, ctx) {
        Poll::Ready(Some(new_stream)) => {
          items.push(new_stream);
        }
        Poll::Ready(None) => {
          source_empty = true;
          // Mark that we're at the end of the list of streams, so we know when to bail
        }
        Poll::Pending => {
          // Just poll the existing streams, do nothing here
        }
      };
    }

    let items_ref = items.as_mut();
    match Stream::poll_next(items_ref, ctx) {
      Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
      Poll::Ready(None) => {
        if source_empty {
          Poll::Ready(None)
        } else {
          Poll::Pending
        }
      }
      Poll::Pending => Poll::Pending,
    }
  })
  .fuse()
  .boxed()
}

// Utility helpers from quinn/examples/common

/// Constructs a QUIC endpoint configured for use a client only.
///
/// ## Args
///
/// - server_certs: list of trusted certificates.
#[allow(unused)]
pub fn make_client_endpoint(
  bind_addr: SocketAddr,
  server_certs: &[&[u8]],
) -> Result<Endpoint, Box<dyn Error>> {
  let client_cfg = configure_client(server_certs)?;
  let mut endpoint_builder = Endpoint::builder();
  endpoint_builder.default_client_config(client_cfg);
  let (endpoint, _incoming) = endpoint_builder.bind(&bind_addr)?;
  Ok(endpoint)
}

/// Constructs a QUIC endpoint configured to listen for incoming connections on a certain address
/// and port.
///
/// ## Returns
///
/// - a sream of incoming QUIC connections
/// - server certificate serialized into DER format
#[allow(unused)]
pub fn make_server_endpoint(bind_addr: SocketAddr) -> Result<(Incoming, Vec<u8>), Box<dyn Error>> {
  let (server_config, server_cert) = configure_server()?;
  let mut endpoint_builder = Endpoint::builder();
  endpoint_builder.listen(server_config);
  let (_endpoint, incoming) = endpoint_builder.bind(&bind_addr)?;
  Ok((incoming, server_cert))
}

/// Builds default quinn client config and trusts given certificates.
///
/// ## Args
///
/// - server_certs: a list of trusted certificates in DER format.
fn configure_client(server_certs: &[&[u8]]) -> Result<ClientConfig, Box<dyn Error>> {
  let mut cfg_builder = ClientConfigBuilder::default();
  for cert in server_certs {
    cfg_builder.add_certificate_authority(Certificate::from_der(&cert)?)?;
  }
  Ok(cfg_builder.build())
}

/// Returns default server configuration along with its certificate.
fn configure_server() -> Result<(ServerConfig, Vec<u8>), Box<dyn Error>> {
  let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
  let cert_der = cert.serialize_der().unwrap();
  let priv_key = cert.serialize_private_key_der();
  let priv_key = PrivateKey::from_der(&priv_key)?;

  let mut transport_config = TransportConfig::default();
  transport_config.stream_window_uni(0);
  let mut server_config = ServerConfig::default();
  server_config.transport = Arc::new(transport_config);
  let mut cfg_builder = ServerConfigBuilder::new(server_config);
  let cert = Certificate::from_der(&cert_der)?;
  cfg_builder.certificate(CertificateChain::from_certs(vec![cert]), priv_key)?;

  Ok((cfg_builder.build(), cert_der))
}

#[allow(unused)]
pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];

#[cfg(test)]
mod tests {
  #[tokio::test]
  async fn test_stream_merging() {
    use futures::{
      future::FutureExt,
      stream::{self, Stream, StreamExt},
    };

    let x = stream::unfold(1i32, async move |state| {
      if state <= 5 {
        Some((state, state + 1))
      } else {
        None
      }
    })
    .boxed();

    let y = stream::unfold(15, async move |state| {
      if state <= 17 {
        Some((state, state + 1))
      } else {
        None
      }
    })
    .boxed();

    let z = stream::unfold(-1i32, async move |state| {
      if state >= -10 {
        Some((state, state - 1))
      } else {
        None
      }
    })
    .boxed();

    let (trigger_z_end, listener_z_end) = triggered::trigger();
    let (trigger_x_3, listener_x_3) = triggered::trigger();
    let first = stream::iter(vec![
      async {
        println!("x started");
        None
      }
      .into_stream()
      .boxed(),
      x.map(|x| Some(x))
        .inspect(|v| {
          if *v == Some(3i32) && !trigger_x_3.is_triggered() {
            trigger_x_3.trigger()
          }
        })
        .boxed(),
      async {
        println!("x exhausted");
        None
      }
      .into_stream()
      .boxed(),
      async {
        listener_z_end.await;
        println!("y started");
        None
      }
      .into_stream()
      .boxed(),
      y.map(|x| Some(x)).boxed(),
      async {
        println!("y exhausted");
        None
      }
      .into_stream()
      .boxed(),
    ])
    .flatten()
    .filter_map(async move |x| x)
    .boxed();
    let second = stream::iter(vec![
      async {
        listener_x_3.await;
        println!("z started");
        None
      }
      .into_stream()
      .boxed(),
      z.map(|x| Some(x)).boxed(),
      async {
        println!("z exhausted");
        trigger_z_end.trigger();
        None
      }
      .into_stream()
      .boxed(),
    ])
    .flatten()
    .filter_map(async move |x| x)
    .boxed();

    let stream_source: stream::BoxStream<'_, stream::BoxStream<'_, _>> =
      stream::iter(vec![first, second]).boxed();
    let out: stream::BoxStream<'_, i32> = super::merge_streams(stream_source);

    let mut items = Vec::new();
    out
      .fold(&mut items, async move |i, m| {
        println!("-> {:?}", &m);
        i.push(m.clone());
        i
      })
      .await;

    let pos_of = |x: i32| items.iter().position(|&v| v == x).unwrap();
    assert_eq!(
      pos_of(-1),
      pos_of(3) + 1,
      "Z must start directly after X reaches 3"
    );
    assert_eq!(pos_of(15), pos_of(-10) + 1, "Y must start after Z ends");
    assert_eq!(
      pos_of(-2),
      pos_of(4) + 1,
      "Z must start directly after X reaches 3"
    );
    assert_eq!(
      pos_of(5),
      pos_of(-2) + 1,
      "X must end just after Z reaches -2"
    );
  }
}
