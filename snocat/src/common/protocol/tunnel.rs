use std::{net::SocketAddr, pin::Pin};

use crate::{server::deferred::SnocatClientIdentifier, util::tunnel_stream::WrappedStream};
use futures::{
  future::{BoxFuture, Either},
  stream::{BoxStream, Stream, StreamFuture},
  FutureExt, StreamExt,
};
use quinn::{crypto::Session, generic::RecvStream, ApplicationClose, SendStream};
use tokio::{
  io::{AsyncRead, AsyncWrite},
  sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};

type BoxedTunnel<'a> = Box<dyn Tunnel + Send + Sync + Unpin + 'a>;
type BoxedTunnelPair<'a> = (BoxedTunnel<'a>, TunnelIncoming);

pub struct QuinnTunnel<S: quinn::crypto::Session> {
  connection: quinn::generic::Connection<S>,
}

impl<S> Tunnel for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream, TunnelError>> {
    use futures::future::FutureExt;
    self
      .connection
      .open_bi()
      .map(|result| match result {
        Ok((send, recv)) => Ok(WrappedStream::Boxed(Box::new(recv), Box::new(send))),
        Err(e) => Err(e.into()),
      })
      .boxed()
  }

  fn info(&self) -> TunnelInfo {
    TunnelInfo::Socket(self.connection.remote_address())
  }
}

pub fn from_quinn_endpoint<S>(
  new_connection: quinn::generic::NewConnection<S>,
) -> (QuinnTunnel<S>, TunnelIncoming)
where
  S: quinn::crypto::Session + 'static,
{
  let quinn::generic::NewConnection {
    connection,
    bi_streams,
    ..
  } = new_connection;
  let stream_tunnels = bi_streams
    .map(|r| match r {
      Ok((send, recv)) => {
        TunnelIncomingType::BiStream(WrappedStream::Boxed(Box::new(recv), Box::new(send)))
      }
      Err(e) => TunnelIncomingType::Closed(e.into()),
    })
    .boxed();
  (
    QuinnTunnel { connection },
    TunnelIncoming {
      inner: stream_tunnels,
    },
  )
}

pub struct DuplexTunnel {
  channel_to_remote: UnboundedSender<WrappedStream>,
}

impl Tunnel for DuplexTunnel {
  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream, TunnelError>> {
    let (local, remote) = tokio::io::duplex(8192);
    futures::future::ready(
      self
        .channel_to_remote
        .send(WrappedStream::DuplexStream(remote))
        .map_err(|_| TunnelError::ConnectionClosed)
        .map(|_| WrappedStream::DuplexStream(local)),
    )
    .boxed()
  }
}

/// Produces two entangled [Tunnel] Pairs
/// Each pair maps its [Tunnel] to the opposite member's entangled [TunnelIncoming]
pub fn duplex() -> (
  (DuplexTunnel, TunnelIncoming),
  (DuplexTunnel, TunnelIncoming),
) {
  fn duplex_for(
    up: UnboundedSender<WrappedStream>,
    down: UnboundedReceiver<WrappedStream>,
  ) -> (DuplexTunnel, TunnelIncoming) {
    let tunnel = DuplexTunnel {
      channel_to_remote: up,
    };
    let incoming = down
      .map(TunnelIncomingType::BiStream)
      .chain(futures::stream::once(futures::future::ready(
        TunnelIncomingType::Closed(TunnelError::ConnectionClosed),
      )))
      .boxed();
    (tunnel, TunnelIncoming { inner: incoming })
  }
  let (left_up, right_down) = mpsc::unbounded_channel::<WrappedStream>();
  let (right_up, left_down) = mpsc::unbounded_channel::<WrappedStream>();
  let (left, right) = (
    duplex_for(left_up, left_down),
    duplex_for(right_up, right_down),
  );
  (left, right)
}

#[derive(Debug, Clone)]
pub enum TunnelError {
  ConnectionClosed,
  ApplicationClosed,
  TimedOut,
  TransportError,
  LocallyClosed,
}

impl From<quinn::ConnectionError> for TunnelError {
  fn from(connection_error: quinn::ConnectionError) -> Self {
    match connection_error {
      quinn::ConnectionError::VersionMismatch => Self::TransportError,
      quinn::ConnectionError::TransportError(_) => Self::TransportError,
      quinn::ConnectionError::ConnectionClosed(_) => Self::ConnectionClosed,
      quinn::ConnectionError::ApplicationClosed(_) => Self::ApplicationClosed,
      quinn::ConnectionError::Reset => Self::TransportError,
      quinn::ConnectionError::TimedOut => Self::TimedOut,
      quinn::ConnectionError::LocallyClosed => Self::LocallyClosed,
    }
  }
}

#[derive(Debug, Clone)]
pub enum TunnelInfo {
  Unidentified,
  Socket(SocketAddr),
  Port(u16),
}

pub type TunnelId = u128;
pub type TunnelName = SnocatClientIdentifier;

pub trait Tunnel: Send + Sync + Unpin {
  fn info(&self) -> TunnelInfo {
    TunnelInfo::Unidentified
  }

  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream, TunnelError>>;
}

pub enum TunnelIncomingType {
  BiStream(WrappedStream),
  Closed(TunnelError),
}

pub struct TunnelIncoming {
  inner: BoxStream<'static, TunnelIncomingType>,
}

impl TunnelIncoming {
  pub fn streams(self) -> BoxStream<'static, TunnelIncomingType> {
    self.inner
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use futures::{AsyncReadExt, AsyncWriteExt};
  use tokio::io::AsyncWrite;

  #[tokio::test]
  async fn duplex_tunnel() {
    use super::Tunnel;
    use futures::StreamExt;
    let ((a_tun, a_inc), (b_tun, b_inc)) = super::duplex();

    let fut = async move {
      a_tun.open_link().await.unwrap();
      drop(a_tun); // Dropping the A tunnel ends the incoming streams for B
      let count_of_b: u32 = super::TunnelIncoming::streams(b_inc)
        .fold(0, async move |memo, _stream| memo + 1)
        .await;
      assert_eq!(count_of_b, 2); // stream then ConnectionClosed event
      b_tun.open_link().await.unwrap();
      drop(b_tun); // Dropping the B tunnel ends the incoming streams for A
      let count_of_a: u32 = super::TunnelIncoming::streams(a_inc)
        .fold(0, async move |memo, _stream| memo + 1)
        .await;
      assert_eq!(count_of_a, 2); // stream then ConnectionClosed event
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), fut)
      .await
      .expect("DuplexTunnel test may be failing due to an await deadlock");
  }

  #[tokio::test]
  async fn duplex_tunnel_concurrency() {
    use super::{Tunnel, TunnelIncomingType};
    use crate::util::tunnel_stream::TunnelStream;
    use futures::{future, FutureExt, StreamExt};
    use std::time::Duration;
    use tokio::{sync::Mutex, time::timeout};
    let (server, client) = super::duplex();

    // For this test, the server will listen for two incoming streams,
    // echo the content down new outgoing streams, and stop listening.
    //
    // Note that DuplexTunnel's "Incoming", unlike a Quinn connection,
    // provides bistreams in the order they are opened by the tunnels,
    // not the order in which they have their first data sent on them.
    let fut_server = async move {
      let (tun, inc) = server; // Explicit move
      let tun_ref = &tun;
      inc
        .streams()
        .take(2) // Expect two incoming streams in a row
        .for_each_concurrent(None, async move |stream| {
          let incoming_stream = match stream {
            TunnelIncomingType::Closed(_) => {
              panic!("Test connection closed before producing 2 streams")
            }
            TunnelIncomingType::BiStream(stream) => stream,
          };
          let (mut incoming_downlink, _incoming_uplink) = tokio::io::split(incoming_stream);
          let (_outgoing_downlink, mut outgoing_uplink) =
            tokio::io::split(tun_ref.open_link().await.unwrap());
          crate::util::proxy_tokio_stream(&mut incoming_downlink, &mut outgoing_uplink)
            .await
            .unwrap();
        })
        .await;
    };
    // The client will attempt to make 3 streams, and will expect that the third will produce no result
    let fut_client = async move {
      let (tun, inc) = client; // Explicit move
      let inc_streams = Arc::new(Mutex::new(inc.streams().boxed()));
      const CLIENT_TASK_DURATION: Duration = Duration::from_secs(5);

      // Async Barriers are used to ensure that we are making new
      // streams in order and able to fulfill them them out-of-order
      use tokio::sync::Barrier;
      let step_1 = Barrier::new(2);
      let step_2 = Barrier::new(2);
      let step_3 = Barrier::new(3);
      let step_4 = Barrier::new(3);

      let client_a = {
        let (tun, inc_streams) = (&tun, Arc::clone(&inc_streams));
        let (step_1, step_2, step_3, step_4) = (&step_1, &step_2, &step_3, &step_4);
        let task = async move {
          let test_data_a = vec![1, 2, 3, 4];
          use tokio::prelude::io::BufWriter;
          let mut s: Box<dyn TunnelStream> = Box::new(tun.open_link().await.unwrap());
          s.write_all(test_data_a.as_slice()).await.unwrap();
          AsyncWriteExt::flush(&mut s).await.unwrap();
          // Wait until B finishes starting a stream and sending on it, then receive a stream
          println!("a1");
          step_1.wait().await;
          let inc = inc_streams
            .lock()
            .await
            .next()
            .await
            .expect("Server must produce one stream per stream sent");
          let mut downlink = match inc {
            TunnelIncomingType::Closed(_) => panic!("Server closed before responding to stream"),
            TunnelIncomingType::BiStream(stream) => stream,
          };
          // We've received a stream, wait until B receives its own before dropping our write-end
          println!("a2");
          step_2.wait().await;
          println!("a3");
          step_3.wait().await;
          drop(s); // Dropping our write-end allows the echo server to complete the downlink
          let mut buf = Vec::new();
          downlink.read_to_end(&mut buf).await.unwrap();
          assert_eq!(&buf, &test_data_a);
          println!("a4");
          step_4.wait().await;
        };
        task
      };

      let client_b = {
        let (tun, inc_streams) = (&tun, Arc::clone(&inc_streams));
        let (step_1, step_2, step_3, step_4) = (&step_1, &step_2, &step_3, &step_4);
        let task = async move {
          let test_data_b = vec![4, 3, 2];
          use tokio::prelude::io::BufWriter;
          // Wait until A has started a stream to ensure ordering
          println!("b1");
          step_1.wait().await;
          let mut s: Box<dyn TunnelStream> = Box::new(tun.open_link().await.unwrap());
          s.write_all(test_data_b.as_slice()).await.unwrap();
          AsyncWriteExt::flush(&mut s).await.unwrap();
          // Wait until A has received a stream before receiving our own
          println!("b2");
          step_2.wait().await;
          let inc = inc_streams
            .lock()
            .await
            .next()
            .await
            .expect("Server must produce one stream per stream sent");
          let mut downlink = match inc {
            TunnelIncomingType::Closed(_) => panic!("Server closed before responding to stream"),
            TunnelIncomingType::BiStream(stream) => stream,
          };
          drop(s);
          println!("b3");
          step_3.wait().await;
          // Wait for C to affirm that no further connections are being provided
          println!("b4");
          step_4.wait().await;
          let mut buf = Vec::new();
          downlink.read_to_end(&mut buf).await.unwrap();
          assert_eq!(&buf, &test_data_b);
        };
        task
      };

      let client_c = {
        let (step_3, step_4) = (&step_3, &step_4);
        let inc_streams = Arc::clone(&inc_streams);
        let task = async move {
          println!("c1 (skipped)\nc2 (skipped)");
          println!("c3");
          step_3.wait().await;
          let last = inc_streams.lock().await.next().await.unwrap();
          assert!(matches!(last, TunnelIncomingType::Closed(_)));
          println!("c4");
          step_4.wait().await;
        };
        task
      };

      match timeout(
        CLIENT_TASK_DURATION,
        future::join3(client_a, client_b, client_c),
      )
      .await
      {
        Ok(_) => (),
        Err(_timeout) => {
          eprintln!(
            "Client barrier status: {:#?} {:#?} {:#?} {:#?}",
            &step_1, &step_2, &step_3, &step_4
          );
          panic!("Client timeout");
        }
      }
    };
    timeout(
      Duration::from_secs(10),
      future::join(fut_server, fut_client),
    )
    .await
    .expect("Server/client test has apparent await deadlock");
  }
}
