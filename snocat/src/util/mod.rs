// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#[allow(dead_code)]
use anyhow::Result;
use futures::future::*;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

pub mod cancellation;
pub mod dropkick;
pub mod framed;
pub mod tunnel_stream;
pub mod validators;

// ALPN protocol names and prefixes for snocat variations
pub const ALPN_PREFIX_MS: &[u8] = b"ms-";
pub const ALPN_PREFIX_MS_SNOCAT: &[u8] = b"ms-snocat-";
pub const ALPN_MS_SNOCAT_1: &[u8] = b"ms-snocat-1";

#[deprecated(
  since = "0.4.0",
  note = "Use tokio::io::copy or tokio::io::copy_buf instead"
)]
pub async fn proxy_tokio_stream<
  Send: tokio::io::AsyncWrite + Unpin,
  Recv: tokio::io::AsyncRead + Unpin,
>(
  recv: &mut Recv,
  send: &mut Send,
) -> Result<u64, std::io::Error> {
  tokio::io::copy_buf(
    &mut tokio::io::BufReader::with_capacity(1024 * 32, recv),
    send,
  )
  .await
  .map_err(Into::into)
}

#[tracing::instrument(level = "trace", err, skip(a, b))]
pub async fn proxy_generic_tokio_streams<
  SenderA: tokio::io::AsyncWrite + Unpin,
  ReaderA: tokio::io::AsyncRead + Unpin,
  SenderB: tokio::io::AsyncWrite + Unpin,
  ReaderB: tokio::io::AsyncRead + Unpin,
>(
  a: (&mut SenderA, &mut ReaderA),
  b: (&mut SenderB, &mut ReaderB),
) -> Result<(u64, u64), std::io::Error> {
  const PROXY_BUFFER_CAPACITY: usize = 1024 * 32;
  let (sender_a, reader_a) = a;
  let (sender_b, reader_b) = b;
  let mut reader_a = tokio::io::BufReader::with_capacity(PROXY_BUFFER_CAPACITY, reader_a);
  let mut reader_b = tokio::io::BufReader::with_capacity(PROXY_BUFFER_CAPACITY, reader_b);
  let proxy_a2b = tokio::io::copy_buf(&mut reader_a, sender_b).fuse();
  let proxy_b2a = tokio::io::copy_buf(&mut reader_b, sender_a).fuse();
  tracing::trace!("polling");
  match futures::future::try_join(proxy_a2b, proxy_b2a).await {
    Ok((a_to_b, b_to_a)) => Ok((a_to_b, b_to_a)),
    Err(e) => {
      tracing::debug!(error = ?e, "Proxy connection copy with error {:#?}", e);
      Err(e)
    }
  }
}

#[deprecated(since = "0.4.0", note = "Use tokio::io::copy_bidirectional")]
#[tracing::instrument(level = "trace", err)]
pub async fn proxy_tcp_streams(
  mut source: TcpStream,
  mut proxy: TcpStream,
) -> Result<(u64, u64), std::io::Error> {
  tokio::io::copy_bidirectional(&mut source, &mut proxy).await
}

#[deprecated(since = "0.4.0", note = "Use tokio::io::copy or tokio::io::copy_buf")]
pub async fn proxy_from_tcp_stream<Sender: AsyncWrite + Unpin, Reader: AsyncRead + Unpin>(
  mut source: TcpStream,
  proxy: (&mut Sender, &mut Reader),
) -> Result<(u64, u64), std::io::Error> {
  let (mut reader, mut writer) = (&mut source).split();
  Ok(proxy_generic_tokio_streams((&mut writer, &mut reader), proxy).await?)
}

#[deprecated(
  since = "0.4.0",
  note = "Use snocat::util::dropkick for async finalizers or #![feature(try_blocks)]"
)]
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

#[cfg(test)]
mod tests {
  use std::sync::Arc;
  use std::time::Duration;

  use futures::FutureExt;
  use tokio::io::duplex;
  use tokio::io::AsyncReadExt;
  use tokio::io::AsyncWriteExt;
  use tokio::sync::Barrier;

  // Given input to one side terminating, ensure that a source stream closing also closes its output
  // TODO: There has to be a simpler way to assert this
  #[tokio::test]
  async fn independent_directional_closure() {
    let (near, far) = duplex(2048);

    let request_input = Vec::from(*b"request").repeat(128);
    let response_input = Vec::from(*b"response").repeat(128);
    let (mut input_r, mut input_w) = tokio::io::duplex(2048);
    let (mut near_r, mut near_w) = tokio::io::split(near);
    let (mut far_r, mut far_w) = tokio::io::split(far);

    // Uses read-to-end to determine that a stream has closed
    // By ordering these calls, we can determine that one stream closes after another

    let all_proxying_ready = Arc::new(Barrier::new(3));
    let all_proxy_exit = Arc::new(Barrier::new(3));

    let a = tokio::task::spawn({
      let all_proxying_ready = all_proxying_ready.clone();
      let all_proxy_exit = all_proxy_exit.clone();
      async move {
        println!("A initializing");
        all_proxying_ready.wait().await;
        println!("A writing");
        input_w.write_all(&request_input).await.unwrap();
        println!("A shutting down");
        input_w.shutdown().await.unwrap();
        println!("A dropping");
        drop(input_w);
        println!("A dropped");
        let mut buf = Vec::new();
        far_r.read_to_end(&mut buf).await.unwrap();
        println!("A read far_r");
        all_proxy_exit.wait().await;
        println!("A finished");
      }
    });

    let b = tokio::task::spawn({
      let all_proxying_ready = all_proxying_ready.clone();
      let all_proxy_exit = all_proxy_exit.clone();
      async move {
        println!("B initializing");
        all_proxying_ready.wait().await;
        println!("B reading");
        let mut buf = Vec::new();
        near_r.read_to_end(&mut buf).await.unwrap();
        println!("B read complete");
        drop(near_r);
        println!("B dropped");
        all_proxy_exit.wait().await;
        println!("B finished");
      }
    });

    let proxy = tokio::task::spawn({
      let all_proxying_ready = all_proxying_ready.clone();
      let all_proxy_exit = all_proxy_exit.clone();
      async move {
        // Send content written by A from `input` to `far`, then have `far` echo its stream to `near`, which will be read by B
        println!("Proxying initializing");
        let mut response_cursor = std::io::Cursor::new(response_input);
        let proxy_future = super::proxy_generic_tokio_streams(
          (&mut near_w, &mut input_r),
          (&mut far_w, &mut response_cursor),
        );
        all_proxying_ready.wait().await;
        println!("Proxying started");
        proxy_future.await.unwrap();
        println!("Proxying complete");
        all_proxy_exit.wait().await;
      }
    });

    use futures::future::Either;
    match futures::future::select(
      futures::future::try_join3(a, b, proxy).boxed(),
      tokio::time::sleep(Duration::from_secs(10)).map(Ok).boxed(),
    )
    .await
    {
      Either::Left((Ok(_), _)) => {}
      Either::Right((Ok(_), _)) => panic!("Timeout reached running async test"),
      Either::Left((Err(e), _)) | Either::Right((Err(e), _)) => {
        panic!("Error running unit test: {:#?}", e)
      }
    }
  }
}
