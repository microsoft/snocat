#![feature(nll)]
#![feature(async_closure)]
#![feature(label_break_value)]
#![feature(str_split_once)]
#![allow(dead_code)]
#![allow(unused_imports)]

use anyhow::{anyhow, bail, Context as AnyhowContext, Result};
use clap::{App, Arg, SubCommand};
// #[macro_use]
use async_std::io::{BufReader, BufWriter};
use async_std::net::{TcpListener, TcpStream, ToSocketAddrs};
use async_std::prelude::*;
use futures::future::Either;
use futures::{self, Future, FutureExt, *};
use quinn::TransportConfig;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{
  path::{Path, PathBuf},
  sync::Arc,
  task::{Context, Poll},
};
use tracing::{error, info, info_span, trace};
use tracing_futures::Instrument as _;

pub mod common;
pub mod util;

pub mod certgen;
pub mod client;
pub mod server;

use util::validators::{
  parse_socketaddr, validate_existing_file, validate_ipaddr, validate_port_range,
  validate_socketaddr,
};

#[cfg(test)]
mod tests {
  #[async_std::test]
  async fn stream_one_byte() {
    use async_std;
  }
}
