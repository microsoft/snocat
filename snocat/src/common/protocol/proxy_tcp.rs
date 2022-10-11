// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use futures::future::{BoxFuture, FutureExt};
use std::{
  convert::{Infallible, TryFrom, TryInto},
  fmt::Display,
  net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
  str::FromStr,
};
use tokio::{
  io::{AsyncRead, AsyncWrite},
  net::TcpStream,
};
use tracing_futures::Instrument;

use super::{
  address::RouteAddressParseError,
  service::{Client, ClientResult, ProtocolInfo, RouteAddressBuilder},
  tunnel::ArcTunnel,
  RouteAddress, Service, ServiceError,
};
use crate::{
  common::protocol::service::ClientError,
  util::{proxy_generic_tokio_streams, tunnel_stream::TunnelStream},
};

#[derive(Debug, Clone)]
pub struct TcpStreamClient<Reader, Writer> {
  recv: Reader,
  send: Writer,
}

impl<Reader, Writer> TcpStreamClient<Reader, Writer> {
  pub fn new(recv: Reader, send: Writer) -> Self {
    Self { recv, send }
  }

  pub fn build_addr(target: TcpStreamTarget) -> RouteAddress {
    target.into()
  }
}

impl ProtocolInfo for TcpStreamService {
  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    "proxy-tcp"
  }
}

impl<Reader, Writer> ProtocolInfo for TcpStreamClient<Reader, Writer> {
  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    TcpStreamService::protocol_name()
  }
}

impl<Reader, Writer> RouteAddressBuilder for TcpStreamClient<Reader, Writer> {
  type Params = TcpStreamTarget;
  type BuildError = Infallible;

  fn build_addr(args: Self::Params) -> Result<RouteAddress, Self::BuildError>
  where
    Self: Sized,
  {
    Ok(args.into())
  }
}

impl<'stream, TStream, Reader, Writer> Client<'stream, TStream> for TcpStreamClient<Reader, Writer>
where
  Reader: AsyncRead + Send + Unpin + 'stream,
  Writer: AsyncWrite + Send + Unpin + 'stream,
  TStream: TunnelStream + Send + 'stream,
{
  type Response = (u64, u64);

  type Error = std::io::Error;

  type Future = BoxFuture<'stream, ClientResult<'stream, Self, TStream>>;

  fn handle(mut self, _addr: RouteAddress, tunnel: TStream) -> Self::Future {
    let fut = async move {
      // TODO: Read protocol version here, and ServiceError::Refused if unsupported
      // TODO: Send protocol version here, allow other side to refuse if unsupported
      // If a confirmation of support is received by the reading side, resume as supported version
      let (mut tunr, mut tunw) = tokio::io::split(tunnel);
      match proxy_generic_tokio_streams((&mut self.send, &mut self.recv), (&mut tunw, &mut tunr))
        .await
      {
        Ok((tcp_to_tunnel_bytes, tunnel_to_tcp_bytes)) => {
          tracing::info!(
            target = "proxy_tcp_close",
            tcp_to_tunnel = tcp_to_tunnel_bytes,
            tunnel_to_tcp = tunnel_to_tcp_bytes,
            "Closing stream",
          );
          Ok((tcp_to_tunnel_bytes, tunnel_to_tcp_bytes))
        }
        Err(e) => {
          tracing::error!(
            target = "proxy_tcp_error",
            error = ?e,
            "TCP proxy IO error: {:#?}",
            e,
          );
          Err(ClientError::IllegalResponse(e))
        }
      }
    };
    fut.fuse().boxed()
  }
}

#[derive(Debug)]
pub struct TcpStreamService {
  pub local_only: bool,
}

#[derive(thiserror::Error, Debug)]
enum TcpConnectError {
  #[error("Service failed to connect to remote TCP target")]
  ConnectionFailed,
  #[error(
    "No addresses provided for target connection fulfilled loopback requirements in local mode"
  )]
  NoLoopbackAddressesFound,
}

#[derive(thiserror::Error, Debug)]
pub enum TargetResolutionError {
  #[error("DNS resolution failure")]
  IOError(
    #[from]
    #[cfg_attr(feature = "backtrace", backtrace)]
    std::io::Error,
  ),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsTarget {
  /// "PreferHigher" is encoded as `dns`, specifying that the highest IP stack available should be used
  PreferHigher {
    host: String,
    port: u16,
  },
  Dns4 {
    host: String,
    port: u16,
  },
  Dns6 {
    host: String,
    port: u16,
  },
}

impl DnsTarget {
  pub fn includes_ipv6(&self) -> bool {
    match self {
      DnsTarget::PreferHigher { .. } => true,
      DnsTarget::Dns6 { .. } => true,
      DnsTarget::Dns4 { .. } => false,
    }
  }

  pub fn includes_ipv4(&self) -> bool {
    match self {
      DnsTarget::PreferHigher { .. } => true,
      DnsTarget::Dns6 { .. } => false,
      DnsTarget::Dns4 { .. } => true,
    }
  }

  /// Host string, without port if present
  ///
  /// Not all DNS records have a constant, known port; See [SRV records](https://en.wikipedia.org/wiki/SRV_record)
  pub fn host(&self) -> &str {
    match self {
      DnsTarget::PreferHigher { host, .. } => host.as_str(),
      DnsTarget::Dns6 { host, .. } => host.as_str(),
      DnsTarget::Dns4 { host, .. } => host.as_str(),
    }
  }

  /// Exposed port for the specified address
  ///
  /// Not all DNS records have a constant, known port; See [SRV records](https://en.wikipedia.org/wiki/SRV_record)
  pub fn port(&self) -> Option<u16> {
    match self {
      DnsTarget::PreferHigher { port, .. } => Some(*port),
      DnsTarget::Dns6 { port, .. } => Some(*port),
      DnsTarget::Dns4 { port, .. } => Some(*port),
    }
  }

  /// Checks that a [SocketAddr] is valid in the range of the specified DNS class
  pub fn contains(&self, addr: &SocketAddr, check_port: bool) -> bool {
    if check_port && Some(addr.port()) != self.port() {
      false
    } else {
      addr.is_ipv6() && self.includes_ipv6() || addr.is_ipv4() && self.includes_ipv4()
    }
  }
}

impl From<DnsTarget> for TcpStreamTarget {
  fn from(val: DnsTarget) -> Self {
    TcpStreamTarget::Dns(val)
  }
}

impl From<TcpStreamTarget> for (String, Option<u16>) {
  fn from(target: TcpStreamTarget) -> Self {
    match target {
      TcpStreamTarget::Port(p) => (Ipv4Addr::LOCALHOST.to_string(), Some(p)),
      TcpStreamTarget::SocketAddr(s) => (s.ip().to_string(), Some(s.port())),
      TcpStreamTarget::Dns(d) => (d.host().to_string(), d.port()),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TcpStreamTarget {
  Port(u16),
  SocketAddr(SocketAddr),
  Dns(DnsTarget),
}

// Uses the Display for TcpStreamTarget implementation to create the second portion of the route address
impl From<&TcpStreamTarget> for RouteAddress {
  fn from(target: &TcpStreamTarget) -> Self {
    let base_addr = RouteAddress::from_iter([TcpStreamService::protocol_name()]);
    match target {
      TcpStreamTarget::Port(port) => base_addr.into_suffixed(["tcp", port.to_string().as_str()]),
      TcpStreamTarget::SocketAddr(SocketAddr::V4(s)) => base_addr.into_suffixed([
        "ip4",
        s.ip().to_string().as_str(),
        "tcp",
        s.port().to_string().as_str(),
      ]),
      TcpStreamTarget::SocketAddr(SocketAddr::V6(s)) => base_addr.into_suffixed([
        "ip6",
        s.ip().to_string().as_str(),
        "tcp",
        s.port().to_string().as_str(),
      ]),
      TcpStreamTarget::Dns(DnsTarget::PreferHigher { host, port }) => {
        base_addr.into_suffixed(["dns", host.as_str(), "tcp", port.to_string().as_str()])
      }
      TcpStreamTarget::Dns(DnsTarget::Dns4 { host, port }) => {
        base_addr.into_suffixed(["dns4", host.as_str(), "tcp", port.to_string().as_str()])
      }
      TcpStreamTarget::Dns(DnsTarget::Dns6 { host, port }) => {
        base_addr.into_suffixed(["dns6", host.as_str(), "tcp", port.to_string().as_str()])
      }
    }
  }
}

impl From<TcpStreamTarget> for RouteAddress {
  fn from(target: TcpStreamTarget) -> Self {
    (&target).into()
  }
}

impl TryFrom<RouteAddress> for TcpStreamTarget {
  type Error = TcpStreamTargetFormatError;

  fn try_from(value: RouteAddress) -> Result<Self, Self::Error> {
    (&value).try_into()
  }
}

impl TryFrom<&RouteAddress> for TcpStreamTarget {
  type Error = TcpStreamTargetFormatError;

  fn try_from(value: &RouteAddress) -> Result<Self, Self::Error> {
    let parts: Vec<&str> =
      if let Some(stripped) = value.strip_segment_prefix([TcpStreamService::protocol_name()]) {
        stripped
      } else {
        return Err(TcpStreamTargetFormatError::NoMatchingFormat)?;
      }
      .take(4)
      .collect();
    let (port, parts) = parts
      .split_last()
      .ok_or(TcpStreamTargetFormatError::TooFewSegments)?;
    let port: u16 = port.parse()?;
    match parts {
      // `/tcp/$PORT`
      // Implies IPv4 on localhost at the given $Port
      ["tcp"] => Ok(TcpStreamTarget::SocketAddr(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        port,
      ))),
      // `/ip4/$IPv4HostAddr/tcp/$PORT`
      // IPv4 on $IPv4HostAddr at the given $Port
      ["ip4", addr, "tcp"] => addr
        .parse::<Ipv4Addr>()
        .map_err(Into::into)
        .map(|addr| TcpStreamTarget::SocketAddr(SocketAddr::new(IpAddr::V4(addr), port))),
      // `/ip6/$IPv6Addr/tcp/$Port`
      // IPv6 on $IPv6HostAddr at the given $Port
      ["ip6", addr, "tcp"] => addr
        .parse::<Ipv6Addr>()
        .map_err(Into::into)
        .map(|addr| TcpStreamTarget::SocketAddr(SocketAddr::new(IpAddr::V6(addr), port))),
      // `/dns[46]?/$SAN/tcp/$Port`
      // DNS mode, resolved using the specified IP stack preference, at the given $Port on that preferred stack
      [dns_class @ ("dns" | "dns4" | "dns6"), host, "tcp"] => {
        let host = host.to_string();
        Ok(TcpStreamTarget::Dns(match *dns_class {
          "dns" => DnsTarget::PreferHigher { host, port },
          "dns6" => DnsTarget::Dns6 { host, port },
          "dns4" => DnsTarget::Dns4 { host, port },
          _ => unreachable!("Checked statically via matcher"),
        }))
      }
      _ => Err(TcpStreamTargetFormatError::NoMatchingFormat),
    }
  }
}

/// Format a [RouteAddress] from a [TcpStreamTarget]
impl Display for TcpStreamTarget {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let route_address: RouteAddress = self.into();
    Display::fmt(&route_address, f)
  }
}

#[derive(thiserror::Error, Debug)]
pub enum TcpStreamTargetFormatError {
  #[error("Not enough segments present to represent valid target")]
  TooFewSegments,
  #[error("No supported address type matches the provided format")]
  NoMatchingFormat,
  #[error("Port specification invalid")]
  InvalidPort {
    #[from]
    inner: std::num::ParseIntError,
    #[cfg(feature = "backtrace")]
    #[cfg_attr(feature = "backtrace", backtrace)]
    backtrace: std::backtrace::Backtrace,
  },
  #[error("IP format invalid")]
  InvalidIP {
    #[from]
    inner: std::net::AddrParseError,
    #[cfg(feature = "backtrace")]
    #[cfg_attr(feature = "backtrace", backtrace)]
    backtrace: std::backtrace::Backtrace,
  },
}

#[derive(thiserror::Error, Debug)]
pub enum TcpStreamTargetParseError {
  #[error(transparent)]
  RouteAddressParseError(#[from] RouteAddressParseError),
  #[error(transparent)]
  TcpStreamTargetFormatError(#[from] TcpStreamTargetFormatError),
}

/// Try to parse a [RouteAddress] into a [TcpStreamTarget]
///
/// Expects /tcp/<port>, /ip[46]/address/tcp/port, or /dns[46]?/address/tcp/port
///
/// DNS resolution is not handled here, only parsed to its own class for use later.
///
/// /tcp/<port> directs to localhost with an IPv6 preference, and is equivalent to
/// /dns/localhost/tcp/<port> but skips the DNS resolver and ignores the hostfile.
// TODO: hostname validation; use a dedicated DNS library and fail invalid names.
// TODO: Use a recursive descent parsing combinator library such as Nom
impl FromStr for TcpStreamTarget {
  type Err = TcpStreamTargetParseError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    let route_addr = s.parse::<RouteAddress>()?;
    (&route_addr).try_into().or_else(|e| match e {
      // If we had an error where the prefix could be missing, retry parsing with it added
      TcpStreamTargetFormatError::TooFewSegments | TcpStreamTargetFormatError::NoMatchingFormat => {
        match route_addr.iter_segments().nth(0) {
          Some("dns" | "dns4" | "dns6" | "ip4" | "ip6" | "tcp") => {
            let prefixed =
              format!("/{}{}", TcpStreamService::protocol_name(), s).parse::<RouteAddress>()?;
            Ok(prefixed.try_into()?)
          }
          _ => Err(TcpStreamTargetParseError::TcpStreamTargetFormatError(
            TcpStreamTargetFormatError::NoMatchingFormat,
          )),
        }
      }
      _ => Err(e.into()),
    })
  }
}

impl TcpStreamService {
  pub fn new(local_only: bool) -> Self {
    Self { local_only }
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
        // Retain only loopback connections
        // This is simpler than may be necessary, but a custom service
        // implementation could be provided to fulfill more complex logic
        addrs.retain(|x| x.ip().is_loopback());
        // When in local mode,
        if addrs.is_empty() {
          return Err(TcpConnectError::NoLoopbackAddressesFound);
        }
      }
      Ok(TcpStream::connect(addrs.as_slice()).await.and_then(|c| {
        c.set_nodelay(true)?;
        Ok(c)
      }))
    };
    fut.fuse().boxed()
  }

  async fn resolve_dns(&self, target: DnsTarget) -> Result<Vec<SocketAddr>, TargetResolutionError> {
    // TODO: use a purpose-built library for DNS resolution
    use tokio::net::lookup_host;
    let resolved = lookup_host(match &target {
      DnsTarget::PreferHigher { host, port }
      | DnsTarget::Dns6 { host, port }
      | DnsTarget::Dns4 { host, port } => {
        format!("{}:{}", host, port)
      }
    })
    .await?;
    let matching_scheme = resolved.filter(|addr| target.contains(addr, true));
    Ok(matching_scheme.collect())
  }

  async fn resolve(
    &self,
    target: TcpStreamTarget,
  ) -> Result<Vec<SocketAddr>, TargetResolutionError> {
    match target {
      TcpStreamTarget::Port(port) => Ok(
        [
          SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
          SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        ]
        .to_vec(),
      ),
      TcpStreamTarget::SocketAddr(s) => Ok([s].to_vec()),
      TcpStreamTarget::Dns(dns_target) => self.resolve_dns(dns_target).await,
    }
  }
}

impl Service for TcpStreamService {
  type Error = anyhow::Error;

  fn accepts(&self, addr: &RouteAddress, _tunnel: &ArcTunnel) -> bool {
    TcpStreamTarget::try_from(addr).is_ok()
  }

  fn handle<'a>(
    &'a self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'static>,
    _tunnel_id: ArcTunnel,
  ) -> BoxFuture<'a, Result<(), ServiceError<Self::Error>>> {
    tracing::debug!(
      "TCP proxy connection received for {}; building span...",
      addr
    );
    let span = tracing::span!(tracing::Level::DEBUG, "proxy_tcp", target = ?addr);
    let target: TcpStreamTarget = match addr.try_into().map_err(|_| ServiceError::AddressError) {
      Err(e) => return futures::future::ready(Err(e)).boxed(),
      Ok(target) => target,
    };
    let fut = async move {
      // TODO: Read protocol version here, and ServiceError::Refused if unsupported
      // TODO: Send protocol version here, allow other side to refuse if unsupported
      // If a confirmation of support is received by the reading side, resume as supported version
      let addrs = self
        .resolve(target)
        .await
        .or(Err(ServiceError::AddressError))?;
      let connector = self.connect(addrs);
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
      let (mut tunr, mut tunw) = tokio::io::split(stream);

      match proxy_generic_tokio_streams((&mut tcpw, &mut tcpr), (&mut tunw, &mut tunr)).await {
        Ok((tcp_to_tunnel_bytes, tunnel_to_tcp_bytes)) => {
          tracing::info!(
            target = "proxy_tcp_close",
            tcp_to_tunnel = tcp_to_tunnel_bytes,
            tunnel_to_tcp = tunnel_to_tcp_bytes,
            "Closing stream",
          );
          Ok(())
        }
        Err(e) => {
          tracing::error!(
            target = "proxy_tcp_error",
            error = ?e,
            "TCP proxy IO error: {:#?}",
            e,
          );
          Err(ServiceError::InternalError(e.into()))
        }
      }
    };

    fut.instrument(span).boxed()
  }
}

#[cfg(test)]
mod tests {
  use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

  use crate::common::protocol::proxy_tcp::{DnsTarget, TcpStreamTarget};

  #[test]
  fn test_dns_name_parsing() {
    assert_eq!(
      TcpStreamTarget::try_from(
        &TcpStreamTarget::Dns(DnsTarget::Dns4 {
          host: "127.0.0.1".to_string(),
          port: 0,
        })
        .into()
      )
      .unwrap(),
      TcpStreamTarget::Dns(DnsTarget::Dns4 {
        host: "127.0.0.1".to_string(),
        port: 0
      })
    );
    assert_eq!(
      TcpStreamTarget::try_from(
        &TcpStreamTarget::Dns(DnsTarget::Dns4 {
          host: "example.com".to_string(),
          port: 0,
        })
        .into()
      )
      .unwrap(),
      TcpStreamTarget::Dns(DnsTarget::Dns4 {
        host: "example.com".to_string(),
        port: 0
      })
    );
    assert_eq!(
      TcpStreamTarget::try_from(
        &(TcpStreamTarget::Dns(DnsTarget::Dns6 {
          host: "example.com".to_string(),
          port: 65535,
        }))
        .into()
      )
      .unwrap(),
      (TcpStreamTarget::Dns(DnsTarget::Dns6 {
        host: "example.com".to_string(),
        port: 65535
      }))
    );
    assert_eq!(
      TcpStreamTarget::try_from(
        &(TcpStreamTarget::Dns(DnsTarget::PreferHigher {
          host: "example.com".to_string(),
          port: 443,
        }))
        .into()
      )
      .unwrap(),
      (TcpStreamTarget::Dns(DnsTarget::PreferHigher {
        host: "example.com".to_string(),
        port: 443
      }))
    );
    // This test should actually fail, by all rights- but we don't parse the DNS name for reasonable formatting.
    // We should probably use the same formatting as https://doc.rust-lang.org/std/net/struct.SocketAddrV6.html
    // once we use DNSName handling instead of `String` for hosts.
    //
    // The standing question is: Should we support un-square-bracketed IPv6 addresses and coerce them to
    // bracketed form, or require a bracketed form for it to parse at all?
    assert_eq!(
      TcpStreamTarget::try_from(
        &(TcpStreamTarget::Dns(DnsTarget::Dns6 {
          host: "::1".to_string(),
          port: 8080,
        }))
        .into()
      )
      .unwrap(),
      (TcpStreamTarget::Dns(DnsTarget::Dns6 {
        host: "::1".to_string(),
        port: 8080
      }))
    );
    assert_eq!(
      TcpStreamTarget::try_from(
        &(TcpStreamTarget::Dns(DnsTarget::Dns6 {
          host: "[::1]".to_string(),
          port: 8080,
        }))
        .into()
      )
      .unwrap(),
      (TcpStreamTarget::Dns(DnsTarget::Dns6 {
        host: "[::1]".to_string(),
        port: 8080
      }))
    );
  }

  #[test]
  fn target_parsing() {
    assert_eq!(
      "/tcp/2468".parse::<TcpStreamTarget>().unwrap(),
      TcpStreamTarget::SocketAddr(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2468))
    );
    assert_eq!(
      "/ip4/127.0.0.1/tcp/2468"
        .parse::<TcpStreamTarget>()
        .unwrap(),
      TcpStreamTarget::SocketAddr(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2468))
    );
    assert_eq!(
      "/ip6/::1/tcp/2468".parse::<TcpStreamTarget>().unwrap(),
      TcpStreamTarget::SocketAddr(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 2468))
    );
    assert_eq!(
      "/dns4/localhost/tcp/2468"
        .parse::<TcpStreamTarget>()
        .unwrap(),
      TcpStreamTarget::Dns(DnsTarget::Dns4 {
        host: "localhost".into(),
        port: 2468
      })
    );
    assert_eq!(
      "/dns6/localhost/tcp/2468"
        .parse::<TcpStreamTarget>()
        .unwrap(),
      TcpStreamTarget::Dns(DnsTarget::Dns6 {
        host: "localhost".into(),
        port: 2468
      })
    );
  }
}
