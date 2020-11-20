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

use axl::{client, common, server, util};

use util::validators::{
  parse_ipaddr, parse_port_range, parse_socketaddr, validate_existing_file, validate_ipaddr,
  validate_port_range, validate_socketaddr,
};

mod certgen;

// Consider for tests : https://github.com/djc/quinn/blob/main/quinn/examples/insecure_connection.rs
fn main() {
  // let collector = tracing_subscriber::fmt()
  //   .with_max_level(tracing::Level::TRACE)
  //   .finish();
  let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("quinn=warn,quinn_proto=warn,debug"));
  let collector = tracing_subscriber::fmt()
    .pretty()
    .with_env_filter(env_filter)
    // .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
    .finish();
  tracing::subscriber::set_global_default(collector).expect("Logger init must succeed");
  let app = App::new(env!("CARGO_BIN_NAME"))
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
          Arg::with_name("driver-san")
            .long("driver-san")
            .visible_alias("san")
            .short("s")
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
        .arg(
          Arg::with_name("tcp")
            .long("bindip")
            .short("i")
            .validator(validate_ipaddr)
            .default_value("127.0.0.1")
            .takes_value(true)
            .required(true),
        )
        .arg(
          Arg::with_name("bind_range")
            .long("ports")
            .short("p")
            .validator(validate_port_range)
            .default_value("8080")
            .takes_value(true)
            .required(true),
        )
        .arg(
          Arg::with_name("quic")
            .help("Port that will accept tunneling clients to receive forwarded connections")
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
  let handler = main_args_handler(&matches);
  match async_std::task::block_on(handler) {
    Err(err) => {
      tracing::error!(mode = mode, err = ?err, "dispatch_command_failure");
    }
    Ok(_) => tracing::info!("{} exited successfully", mode),
  }
}

pub async fn client_arg_handling(args: &'_ clap::ArgMatches<'_>) -> Result<client::ClientArgs> {
  let cert_path = Path::new(args.value_of("authority").unwrap()).to_path_buf();
  Ok(client::ClientArgs {
    authority_cert: cert_path,
    driver_host: parse_socketaddr(args.value_of("driver").unwrap())?,
    driver_san: args.value_of("driver-san").unwrap().into(),
    proxy_target_host: parse_socketaddr(args.value_of("target").unwrap())?,
  })
}

pub async fn server_arg_handling(args: &'_ clap::ArgMatches<'_>) -> Result<server::ServerArgs> {
  let cert_path = Path::new(args.value_of("cert").unwrap()).to_path_buf();
  let key_path = Path::new(args.value_of("key").unwrap()).to_path_buf();

  Ok(server::ServerArgs {
    cert: cert_path,
    key: key_path,
    quinn_bind_addr: parse_socketaddr(args.value_of("quic").unwrap())?,
    tcp_bind_ip: parse_ipaddr(args.value_of("tcp").unwrap())?,
    tcp_bind_port_range: parse_port_range(args.value_of("bind_range").unwrap())?,
  })
}

async fn main_args_handler(matches: &'_ clap::ArgMatches<'_>) -> Result<()> {
  match matches.subcommand() {
    ("server", Some(opts)) => {
      let config = server_arg_handling(opts).await?;
      tracing::info!("Running as server with config {:#?}", config);
      server::server_main(config).await
    }
    ("client", Some(opts)) => {
      let config = client_arg_handling(opts).await?;
      tracing::info!("Running as client with config {:#?}", config);
      client::client_main(config).await
    }
    ("cert", Some(opts)) => {
      tracing::info!("Generating certs...");
      let path_raw = opts.value_of("path").expect("Path argument is required");
      let san = opts.value_of("san").expect("SAN argument must exist");
      certgen::certgen_main(path_raw.into(), san.into()).await
    }
    (_, _) => unreachable!(),
  }
}

#[cfg(test)]
mod tests {
  #[async_std::test]
  async fn stream_one_byte() {
    use async_std;
  }
}
