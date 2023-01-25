// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use anyhow::Result;
use futures::future::*;
use std::io::Error as IOError;
use std::pin::Pin;
use std::task::{Context, Poll};
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
  use std::time::Duration;

  use futures::future::{self, BoxFuture, FutureExt, TryFuture, TryFutureExt};
  use futures::{pin_mut, Future};
  use tokio::io::AsyncWriteExt;
  use tokio::io::DuplexStream;
  use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

  use crate::util::proxy_generic_tokio_streams;

  /// Panics if an async test takes "too long" (more than a few seconds)
  ///
  /// Use this only for near-instantaneous behaviours. This helper gives
  /// extra time for cases of a CPU-overloaded build/test machine,
  /// but tests should not contain a time component.
  async fn async_test_timeout_panic<Fut: TryFuture>(fut: Fut) -> Fut::Ok
  where
    Fut::Error: std::fmt::Debug + 'static,
  {
    const TIMEOUT: Duration = Duration::from_secs(10);
    let fut = fut.map_ok(|s| s);
    pin_mut!(fut);
    use future::Either;
    match future::select(fut, tokio::time::sleep(TIMEOUT).map(Ok).boxed()).await {
      Either::Left((Ok(success), _)) => success,
      Either::Right((Ok(_), _)) => panic!("Timeout reached running async unit test"),
      Either::Left((Err(e), _)) | Either::Right((Err(e), _)) => {
        panic!("Error running async unit test: {:#?}", e);
      }
    }
  }

  /// Simply swaps the order of a tuple
  ///
  /// Reduces boilerplate from the non-standardized order of (Send, Receive) / (Read, Write) stream splitting
  fn swap_tuple<A, B>((a, b): (A, B)) -> (B, A) {
    (b, a)
  }

  #[track_caller]
  fn create_sendrec_asserter(
    name: &'static str,
    channel: DuplexStream,
    message_to_send: &'static [u8],
    expected_to_receive: &'static [u8],
    send_cue: BoxFuture<'static, ()>,
    after_sent: impl FnOnce() -> (),
  ) -> impl Future<Output = Result<(), std::io::Error>> {
    async move {
      let (mut recv, mut send) = tokio::io::split(channel);
      future::try_join(
        async move {
          let mut buf = Vec::new();
          println!("{} RECV reading", name);
          recv.read_to_end(&mut buf).await?;
          println!("{} RECV asserting", name);
          assert_eq!(&buf, expected_to_receive);
          println!("{} RECV dropping", name);
          drop(recv);
          println!("{} RECV complete", name);
          Ok(())
        },
        async move {
          println!("{} SEND waiting", name);
          send_cue.await;
          println!("{} SEND writing", name);
          send.write_all(message_to_send).await?;
          println!("{} SEND shutting down", name);
          // Without this, we apparently hang forever, due to nuances of DuplexTunnel lifespan and tokio::io::split.
          // This does not occur with TCP streams, thankfully, unless timeouts are disabled or extraordinarily long.
          send.shutdown().await?;
          println!("{} SEND dropping", name);
          drop(send);
          println!("{} SEND triggering notifier", name);
          after_sent();
          println!("{} SEND complete", name);
          Ok(())
        },
      )
      .map_ok(|_| ())
      .await
    }
  }

  /// Test that both sides reaching completion ends the proxying task
  #[tokio::test]
  async fn proxy_completion_on_end() {
    const MESSAGE_FROM_A: &[u8] = b"Hello world from A!";
    const MESSAGE_FROM_B: &[u8] = b"Hello world from B!";
    let (a_to_p, p_to_a) = tokio::io::duplex(64);
    let (p_to_b, b_to_p) = tokio::io::duplex(64);

    let a = create_sendrec_asserter(
      "A->P",
      a_to_p,
      MESSAGE_FROM_A,
      MESSAGE_FROM_B,
      future::ready(()).boxed(),
      || (),
    );
    let b = create_sendrec_asserter(
      "B->P",
      b_to_p,
      MESSAGE_FROM_B,
      MESSAGE_FROM_A,
      future::ready(()).boxed(),
      || (),
    );
    let proxy = async move {
      println!("Proxy starting...");
      let p_to_a = swap_tuple(tokio::io::split(p_to_a));
      let p_to_b = swap_tuple(tokio::io::split(p_to_b));
      proxy_generic_tokio_streams(p_to_a, p_to_b).await?;
      println!("Proxy complete.");
      Ok(())
    };
    async_test_timeout_panic(future::try_join3(a, b, proxy)).await;
  }

  /// Test that one side successfully completing does not end the proxying stream immediately for the other side
  #[tokio::test]
  async fn proxy_independent_stream_completion() {
    const MESSAGE_FROM_A: &[u8] = b"Hello world from A!";
    const MESSAGE_FROM_B: &[u8] = b"Hello world from B!";
    let (a_to_p, p_to_a) = tokio::io::duplex(64);
    let (p_to_b, b_to_p) = tokio::io::duplex(64);

    let (notify_a_completed, a_completion_receiver) = futures::channel::oneshot::channel();

    let a = create_sendrec_asserter(
      "A->P",
      a_to_p,
      MESSAGE_FROM_A,
      MESSAGE_FROM_B,
      future::ready(()).boxed(),
      || notify_a_completed.send(()).unwrap(),
    );
    let b = create_sendrec_asserter(
      "B->P",
      b_to_p,
      MESSAGE_FROM_B,
      MESSAGE_FROM_A,
      a_completion_receiver.map(|x| x.unwrap()).boxed(),
      || (),
    );
    let proxy = async move {
      println!("Proxy starting...");
      let p_to_a = swap_tuple(tokio::io::split(p_to_a));
      let p_to_b = swap_tuple(tokio::io::split(p_to_b));
      proxy_generic_tokio_streams(p_to_a, p_to_b).await?;
      println!("Proxy complete.");
      Ok(())
    };
    async_test_timeout_panic(future::try_join3(a, b, proxy)).await;
  }

  /// Test that one side erroring ends the stream immediately for the other side, and returns the error from the proxying task
  #[tokio::test]
  async fn proxy_completion_on_any_error() {
    use std::{
      pin::Pin,
      task::{Context, Poll},
    };
    struct ErroringStream;
    impl AsyncWrite for ErroringStream {
      fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
      ) -> Poll<Result<usize, std::io::Error>> {
        Poll::Ready(Ok(buf.len()))
      }

      fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
      ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
      }

      fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
      ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
      }
    }

    impl AsyncRead for ErroringStream {
      fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
      ) -> Poll<std::io::Result<()>> {
        let e = std::io::Error::new(
          std::io::ErrorKind::NotConnected,
          "Placeholder failure generator for tests",
        );
        Poll::Ready(Err(e))
      }
    }

    let p_to_a = ErroringStream;
    let (p_to_b, b_to_p) = tokio::io::duplex(64);

    let b = create_sendrec_asserter(
      "B->P",
      b_to_p,
      b"Hello /dev/null!",
      // Nothing received, stream closes immediately.
      //
      // In a networked context, this read would error instead of being empty,
      // we just get this behaviour due to how DuplexStream handles "shutdown"
      // as a graceful end of stream under all contexts.
      b"",
      future::ready(()).boxed(),
      || (),
    );
    let proxy = async move {
      println!("Proxy starting...");
      let p_to_a = swap_tuple(tokio::io::split(p_to_a));
      let p_to_b = swap_tuple(tokio::io::split(p_to_b));
      match proxy_generic_tokio_streams(p_to_a, p_to_b).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {
          println!("Proxy failed with the expected error.");
          Ok(())
        }
        other => {
          panic!(
            "Proxy exited before the expected failure with value {:#?}",
            other
          );
        }
      }
    };
    async_test_timeout_panic(future::try_join(b, proxy)).await;
  }
}
