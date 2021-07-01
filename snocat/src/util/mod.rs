// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#[warn(unused_imports)]
#[allow(dead_code)]
use anyhow::Result;
use futures::future::*;
use futures::stream::{self, SelectAll, Stream, StreamExt};
use futures::AsyncReadExt;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use std::boxed::Box;
use std::path::Path;
use std::task::{Context, Poll};
use std::{net::SocketAddr, sync::Arc};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub mod cancellation;
pub mod delegation;
pub mod dropkick;
pub mod framed;
mod mapped_owned_async_mutex;
pub(crate) mod merge_streams;
pub mod tunnel_stream;
pub mod validators;
pub(crate) mod vtdroppable;
pub use mapped_owned_async_mutex::MappedOwnedMutexGuard;

// HTTP protocol constant from quinn/examples/common
pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];

pub async fn proxy_tokio_stream<
  Send: tokio::io::AsyncWrite + Unpin,
  Recv: tokio::io::AsyncRead + Unpin,
>(
  recv: &mut Recv,
  send: &mut Send,
) -> Result<u64> {
  use tokio::io::AsyncWriteExt;
  tokio::io::copy(
    &mut tokio::io::BufReader::with_capacity(1024 * 32, recv),
    send,
  )
  .await
  .map_err(Into::into)
}

pub async fn proxy_generic_tokio_streams<
  SenderA: tokio::io::AsyncWrite + Unpin,
  ReaderA: tokio::io::AsyncRead + Unpin,
  SenderB: tokio::io::AsyncWrite + Unpin,
  ReaderB: tokio::io::AsyncRead + Unpin,
>(
  a: (&mut SenderA, &mut ReaderA),
  b: (&mut SenderB, &mut ReaderB),
) -> Either<(), ()> {
  let (sender_a, reader_a) = a;
  let (sender_b, reader_b) = b;
  let proxy_a2b = Box::pin(proxy_tokio_stream(reader_a, sender_b).fuse());
  let proxy_b2a = Box::pin(proxy_tokio_stream(reader_b, sender_a).fuse());
  tracing::trace!("polling");
  let res: Either<(), ()> = match futures::future::try_select(proxy_a2b, proxy_b2a).await {
    Ok(Either::Left((_i2o, resume_o2i))) => {
      tracing::debug!("Source connection closed gracefully, shutting down proxy");
      std::mem::drop(resume_o2i); // Kill the copier, allowing us to send end-of-connection
      Either::Right(())
    }
    Ok(Either::Right((_o2i, resume_i2o))) => {
      tracing::debug!("Proxy connection closed gracefully, shutting down source");
      std::mem::drop(resume_i2o); // Kill the copier, allowing us to send end-of-connection
      Either::Left(())
    }
    Err(Either::Left((e_i2o, resume_o2i))) => {
      tracing::debug!(
        "Source connection died with error {:#?}, shutting down proxy connection",
        e_i2o
      );
      std::mem::drop(resume_o2i); // Kill the copier, allowing us to send end-of-connection
      Either::Right(())
    }
    Err(Either::Right((e_o2i, resume_i2o))) => {
      tracing::debug!(
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
  let res: Either<_, _> = {
    let (mut reader, mut writer) = (&mut source).split();
    let (mut proxy_reader, mut proxy_writer) = (&mut proxy).split();
    proxy_generic_tokio_streams(
      (&mut writer, &mut reader),
      (&mut proxy_writer, &mut proxy_reader),
    )
    .await
  };
  match res {
    Either::Left(_) => {
      if let Err(shutdown_failure) = source.shutdown().await {
        tracing::error!(
          "Failed to shut down source connection with error:\n{:#?}",
          shutdown_failure
        );
      }
    }
    Either::Right(_) => {
      if let Err(shutdown_failure) = proxy.shutdown().await {
        tracing::error!(
          "Failed to shut down proxy connection with error:\n{:#?}",
          shutdown_failure
        );
      }
    }
  }
  Ok(())
}

pub async fn proxy_from_tcp_stream<Sender: AsyncWrite + Unpin, Reader: AsyncRead + Unpin>(
  mut source: TcpStream,
  proxy: (&mut Sender, &mut Reader),
) -> Result<()> {
  let res: Either<_, _> = {
    let (mut reader, mut writer) = (&mut source).split();
    proxy_generic_tokio_streams((&mut writer, &mut reader), proxy).await
  };
  match res {
    Either::Left(_) => {
      if let Err(shutdown_failure) = source.shutdown().await {
        tracing::error!(
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

/// Run a block, then, regardless of success/failure, run another block, with access to the results.
/// Exceptions from the first block are preferred, then from the finally block, then successes
pub async fn finally_async<
  T,
  E,
  FT: Future<Output = Result<T, E>>,
  FC: Future<Output = Result<(), E>>,
>(
  cb: impl FnOnce() -> FT,
  cleanup: impl FnOnce(&mut Result<T, E>) -> FC,
) -> Result<T, E> {
  let mut cb_res = cb().await;
  let cleanup_res = cleanup(&mut cb_res).await;
  match cleanup_res {
    Ok(_) => cb_res,
    Err(e) => match cb_res {
      Ok(_res) => Err(e),
      Err(e2) => Err(e2),
    },
  }
}
