use anyhow::{Error as AnyErr, Result};
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use std::boxed::Box;
use std::path::Path;
use std::task::{Context, Poll};
use std::{net::SocketAddr, sync::Arc};

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
