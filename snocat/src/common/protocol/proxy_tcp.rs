// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use futures::{
  future::{BoxFuture, FutureExt},
  AsyncReadExt,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::{
  io::{AsyncRead, AsyncWrite},
  net::{TcpStream, ToSocketAddrs},
};
use tracing_futures::Instrument;

use super::{
  tunnel::Tunnel, Client, ClientError, DynamicResponseClient, Request, Response, RouteAddress,
  Router, RoutingError, Service, ServiceError,
};
use crate::util::{proxy_generic_tokio_streams, tunnel_stream::TunnelStream};

#[derive(Debug, Clone)]
pub struct TcpStreamClient<Reader, Writer> {
  recv: Reader,
  send: Writer,
}

impl<Reader, Writer> Client for TcpStreamClient<Reader, Writer>
where
  Reader: AsyncRead + Send + Unpin + 'static,
  Writer: AsyncWrite + Send + Unpin + 'static,
{
  // TODO: make Response the number of bytes forwarded by the client
  type Response = ();

  fn handle(
    mut self,
    _addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Self::Response, ClientError>> {
    let fut = async move {
      // TODO: Read protocol version here, and ServiceError::Refused if unsupported
      // TODO: Send protocol version here, allow other side to refuse if unsupported
      // If a confirmation of support is received by the reading side, resume as supported version
      let (mut tunr, mut tunw) = tokio::io::split(tunnel);
      proxy_generic_tokio_streams((&mut self.send, &mut self.recv), (&mut tunw, &mut tunr)).await;
      tracing::info!(target = "proxy_tcp_close", "Closing stream");
      Ok(())
    };
    fut.fuse().boxed()
  }
}

#[derive(Debug)]
pub struct TcpStreamService {
  pub local_only: bool,
}

#[derive(Debug, Copy, Clone)]
enum TcpConnectError {
  ConnectionFailed,
  NoLoopbackAddressesFound,
}

impl TcpStreamService {
  pub fn new(local_only: bool) -> Self {
    Self { local_only }
  }

  fn parse_address(addr: &str) -> Result<SocketAddr, ()> {
    // lack of an ip* prefix implies localhost
    // dns resolution will occur in a middleware service
    // Expects /tcp/<port>, /ip4/address/tcp/port, or /ip6/address/tcp/port
    let parts = addr.splitn(5, '/').collect::<Vec<_>>();
    let (addr, port) = match parts.as_slice() {
      ["", "tcp", port] => port
        .parse::<u16>()
        .map_err(|_| ())
        .map(|port| (IpAddr::V4(Ipv4Addr::LOCALHOST), port)),
      ["", "ip4", addr, "tcp", port] => port.parse::<u16>().map_err(|_| ()).and_then(|port| {
        addr
          .parse::<Ipv4Addr>()
          .map_err(|_| ())
          .map(|addr| (IpAddr::V4(addr), port))
      }),
      ["", "ip6", addr, "tcp", port] => port.parse::<u16>().map_err(|_| ()).and_then(|port| {
        addr
          .parse::<Ipv6Addr>()
          .map_err(|_| ())
          .map(|addr| (IpAddr::V6(addr), port))
      }),
      _ => Err(()),
    }?;
    Ok(SocketAddr::new(addr, port))
  }

  /// The `connect` future outlives the read reference lifetime to `self`
  /// This allows us to capture configuration such as `local_only` before
  /// producing a generator; that generator can then refuse to serve when
  /// it is processed.
  fn connect(
    &'_ self,
    mut addrs: Vec<SocketAddr>,
  ) -> BoxFuture<'_, Result<Result<TcpStream, std::io::Error>, TcpConnectError>> {
    let local_only = self.local_only;
    let fut = async move {
      if addrs.is_empty() {
        return Err(TcpConnectError::ConnectionFailed);
      }
      if local_only {
        addrs.drain_filter(|x| !x.ip().is_loopback()).last();
        if addrs.is_empty() {
          return Err(TcpConnectError::NoLoopbackAddressesFound);
        }
      }
      Ok(TcpStream::connect(addrs.as_slice()).await)
    };
    fut.fuse().boxed()
  }
}

impl Service for TcpStreamService {
  fn accepts(&self, addr: &RouteAddress) -> bool {
    addr.starts_with("/tcp/") || addr.starts_with("/ip4/") || addr.starts_with("/ip6/")
  }

  fn handle(
    &'_ self,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<'_, Result<(), ServiceError>> {
    use futures::future::Either;
    tracing::debug!(
      "TCP proxy connection received for {}; building span...",
      addr
    );
    let span = tracing::span!(tracing::Level::DEBUG, "proxy_tcp", target = ?addr);
    let addrs = match Self::parse_address(&addr).map_err(|_| ServiceError::AddressError) {
      Err(e) => return futures::future::ready(Err(e)).boxed(),
      Ok(addr) => addr,
    };
    let connector = self.connect(vec![addrs]);
    let fut = async move {
      // TODO: Read protocol version here, and ServiceError::Refused if unsupported
      // TODO: Send protocol version here, allow other side to refuse if unsupported
      // If a confirmation of support is received by the reading side, resume as supported version
      tracing::debug!(
        target = "proxy_tcp_connecting",
        "Connecting to proxy destination"
      );
      let mut connection: TcpStream = connector
        .await
        .map_err(|e| match e {
          TcpConnectError::ConnectionFailed => ServiceError::DependencyFailure,
          TcpConnectError::NoLoopbackAddressesFound => ServiceError::AddressError,
        })?
        .map_err(|_| ServiceError::DependencyFailure)?;
      tracing::debug!(target = "proxy_tcp_streaming", "Performing proxy streaming");

      let (mut tcpr, mut tcpw) = connection.split();
      let (mut tunr, mut tunw) = tokio::io::split(tunnel);

      proxy_generic_tokio_streams((&mut tcpw, &mut tcpr), (&mut tunw, &mut tunr)).await;
      tracing::info!(target = "proxy_tcp_close", "Closing stream");
      Ok(())
    };

    fut.instrument(span).boxed()
  }
}
