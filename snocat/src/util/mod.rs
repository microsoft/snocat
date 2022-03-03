use std::pin::Pin;
use std::task::{Context, Poll};

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#[allow(dead_code)]
use anyhow::Result;
use futures::future::*;
use std::io::Error as IOError;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite};
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

/// Merge two disparate IOStreams into one AsyncRead+AsyncWrite
struct Unsplit<W, R> {
  w: W,
  r: R,
}

impl<W, R> AsyncRead for Unsplit<W, R>
where
  R: AsyncRead + Unpin,
  W: Unpin,
{
  fn poll_read(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
    buf: &mut tokio::io::ReadBuf<'_>,
  ) -> std::task::Poll<std::io::Result<()>> {
    AsyncRead::poll_read(Pin::new(&mut self.r), cx, buf)
  }
}

impl<W, R> AsyncBufRead for Unsplit<W, R>
where
  R: AsyncBufRead + Unpin,
  W: Unpin,
{
  fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
    let inner = Pin::into_inner(self);
    AsyncBufRead::poll_fill_buf(Pin::new(&mut inner.r), cx)
  }

  fn consume(self: Pin<&mut Self>, amt: usize) {
    let inner = Pin::into_inner(self);
    AsyncBufRead::consume(Pin::new(&mut inner.r), amt)
  }
}

impl<W, R> AsyncWrite for Unsplit<W, R>
where
  R: Unpin,
  W: AsyncWrite + Unpin,
{
  fn poll_write(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &[u8],
  ) -> Poll<Result<usize, IOError>> {
    let parent_ref = Pin::into_inner(self);
    AsyncWrite::poll_write(Pin::new(&mut parent_ref.w), cx, buf)
  }

  fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
    let parent_ref = Pin::into_inner(self);
    AsyncWrite::poll_flush(Pin::new(&mut parent_ref.w), cx)
  }

  fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
    let parent_ref = Pin::into_inner(self);
    AsyncWrite::poll_shutdown(Pin::new(&mut parent_ref.w), cx)
  }

  fn poll_write_vectored(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
    bufs: &[std::io::IoSlice<'_>],
  ) -> std::task::Poll<Result<usize, std::io::Error>> {
    let parent_ref = Pin::into_inner(self);
    AsyncWrite::poll_write_vectored(Pin::new(&mut parent_ref.w), cx, bufs)
  }

  fn is_write_vectored(&self) -> bool {
    AsyncWrite::is_write_vectored(&self.w)
  }
}

#[tracing::instrument(level = "trace", err, skip(a, b))]
pub async fn proxy_generic_tokio_streams<
  SenderA: tokio::io::AsyncWrite + Unpin,
  ReaderA: tokio::io::AsyncRead + Unpin,
  SenderB: tokio::io::AsyncWrite + Unpin,
  ReaderB: tokio::io::AsyncRead + Unpin,
>(
  a: (SenderA, ReaderA),
  b: (SenderB, ReaderB),
) -> Result<(u64, u64), std::io::Error> {
  const PROXY_BUFFER_CAPACITY: usize = 1024 * 32;
  let (sender_a, reader_a) = a;
  let (sender_b, reader_b) = b;
  let reader_a = tokio::io::BufReader::with_capacity(PROXY_BUFFER_CAPACITY, reader_a);
  let reader_b = tokio::io::BufReader::with_capacity(PROXY_BUFFER_CAPACITY, reader_b);

  let mut a = Unsplit {
    r: reader_a,
    w: sender_a,
  };
  let mut b = Unsplit {
    r: reader_b,
    w: sender_b,
  };
  tracing::trace!("polling");
  match tokio::io::copy_bidirectional(&mut a, &mut b).await {
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
