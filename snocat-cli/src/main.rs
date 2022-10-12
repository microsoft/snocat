// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![allow(stable_features)]
#![feature(backtrace)]
#![warn(unused_imports)]

use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use snocat::util;
use std::{
  path::{Path, PathBuf},
  str::FromStr,
};

use util::validators::{
  parse_ipaddr, parse_port_range, parse_socketaddr, validate_existing_file, validate_ipaddr,
  validate_port_range, validate_socketaddr,
};

mod services;

mod certgen;
mod client;
mod server;

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
  let app = Command::new(env!("CARGO_BIN_NAME"))
    .version(env!("CARGO_PKG_VERSION"))
    .about(env!("CARGO_PKG_DESCRIPTION"))
    .subcommand(
      Command::new("client")
        .alias("-c")
        .about("Bind a local port to a remote server")
        // .arg(Arg::new("client-cert").long("client-cert").short('c').validator(validate_existing_file).takes_value(true))
        .arg(
          Arg::new("authority")
            .long("authority")
            .short('a')
            .validator(validate_existing_file)
            .takes_value(true)
            .required(false),
        )
        .arg(
          Arg::new("driver")
            .long("driver")
            .short('d')
            .validator(validate_socketaddr)
            .takes_value(true)
            .required(true),
        )
        .arg(
          Arg::new("driver-san")
            .long("driver-san")
            .visible_alias("san")
            .short('s')
            .takes_value(true)
            .required(true),
        )
        .arg(
          Arg::new("target")
            .long("target")
            .short('t')
            .validator(validate_socketaddr)
            .takes_value(true)
            .required(true),
        ),
    )
    .subcommand(
      Command::new("server")
        .alias("-s")
        .about("Run in server mode, supporting connections from multiple clients")
        .arg(
          Arg::new("cert")
            .long("cert")
            .short('c')
            .validator(validate_existing_file)
            .takes_value(true)
            .required(true),
        )
        .arg(
          Arg::new("key")
            .long("key")
            .short('k')
            .validator(validate_existing_file)
            .takes_value(true)
            .required(true),
        )
        .arg(
          Arg::new("tcp")
            .long("tcp")
            .alias("bindip")
            .short('i')
            .validator(validate_ipaddr)
            .default_value("127.0.0.1")
            .takes_value(true),
        )
        .arg(
          Arg::new("bind_range")
            .long("ports")
            .short('p')
            .validator(validate_port_range)
            .default_value("8080:8090")
            .takes_value(true),
        )
        .arg(
          Arg::new("quic")
            .help("Port that will accept tunneling clients to receive forwarded connections")
            .long("quic")
            .short('q')
            .validator(validate_socketaddr)
            .default_value("127.0.0.1:9090")
            .takes_value(true),
        ),
    )
    .subcommand(
      Command::new("cert")
        .about("Generate self-signed certificates for local usage")
        .arg(Arg::new("path").takes_value(true).required(true))
        .arg(
          Arg::new("san")
            .long("san")
            .takes_value(true)
            .required(false)
            .default_value("localhost"),
        ),
    )
    .subcommand_required(true)
    .arg_required_else_help(true);
  let matches = app.get_matches();
  let mode = matches.subcommand_name().unwrap_or("<No subcommand?>");
  let handler = main_args_handler(&matches);
  let rt = tokio::runtime::Builder::new_multi_thread()
    .thread_name("tokio-reactor-worker")
    .enable_all()
    .build()
    .expect("Tokio Runtime setup failure");
  match rt.block_on(handler) {
    Err(err) => {
      tracing::error!(mode = mode, err = ?err, "dispatch_command_failure");
    }
    Ok(_) => tracing::info!("{} exited successfully", mode),
  }
}

pub async fn client_arg_handling(args: &'_ ArgMatches) -> Result<client::ClientArgs> {
  let authority_cert_path = args
    .value_of("authority")
    .map(|path| PathBuf::from_str(path))
    // flip Option<Result<T, E>> to Result<Option<T>, E>
    .map_or(Ok(None), |v| v.map(Some))?;
  Ok(client::ClientArgs {
    authority_cert: authority_cert_path,
    driver_host: parse_socketaddr(args.value_of("driver").unwrap())?,
    driver_san: args.value_of("driver-san").unwrap().into(),
    proxy_target_host: parse_socketaddr(args.value_of("target").unwrap())?,
  })
}

pub async fn server_arg_handling(args: &'_ ArgMatches) -> Result<server::ServerArgs> {
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

async fn main_args_handler(matches: &'_ ArgMatches) -> Result<()> {
  match matches
    .subcommand()
    .expect("Subcommand is marked as required")
  {
    ("server", opts) => {
      let config = server_arg_handling(opts).await?;
      tracing::info!("Running as server with config {:#?}", config);
      server::server_main(config).await
    }
    ("client", opts) => {
      let config = client_arg_handling(opts).await?;
      tracing::info!("Running as client with config {:#?}", config);
      client::client_main(config).await
    }
    ("cert", opts) => {
      tracing::info!("Generating certs...");
      let path_raw = opts.value_of("path").expect("Path argument is required");
      let san = opts.value_of("san").expect("SAN argument must exist");
      certgen::certgen_main(path_raw.into(), san.into()).await
    }
    (_, _) => unreachable!(),
  }
}

#[cfg(test)]
mod tests {}
