// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use std::{
  borrow::{Borrow, BorrowMut},
  net::SocketAddr,
  pin::Pin,
  sync::Arc,
};

use crate::util::tunnel_stream::WrappedStream;
use futures::{
  future::{BoxFuture, Either},
  stream::{BoxStream, LocalBoxStream, Stream, StreamFuture, TryStreamExt},
  Future, FutureExt, StreamExt,
};
use quinn::{crypto::Session, generic::RecvStream, ApplicationClose, SendStream};
use serde::{Deserializer, Serializer};
use tokio::{
  io::{AsyncRead, AsyncWrite},
  sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    OwnedMutexGuard,
  },
};

pub mod id;

pub use self::id::TunnelId;
pub type BoxedTunnel<'a> = Box<dyn Tunnel + Send + Sync + Unpin + 'a>;
pub type ArcTunnel<'a> = Arc<dyn Tunnel + Send + Sync + Unpin + 'a>;

/// A name for an Snocat tunnel, used to identify its connection in [`TunnelServerEvent`]s.
#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone)]
#[repr(transparent)]
pub struct TunnelName(Arc<String>);

impl serde::Serialize for TunnelName {
  fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&self.0)
  }
}
impl<'de> serde::de::Deserialize<'de> for TunnelName {
  fn deserialize<D>(deserializer: D) -> Result<Self, <D as Deserializer<'de>>::Error>
  where
    D: Deserializer<'de>,
  {
    let s: String = serde::Deserialize::deserialize(deserializer)?;
    Ok(TunnelName::new(s))
  }
}

impl TunnelName {
  pub fn new<T: std::convert::Into<String>>(t: T) -> TunnelName {
    TunnelName(t.into().into())
  }

  pub fn raw(&self) -> &str {
    &self.0
  }
}

impl Into<String> for TunnelName {
  fn into(self) -> String {
    self.0.as_ref().clone()
  }
}

impl std::fmt::Debug for TunnelName {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Snocat").field("Id", &self.0).finish()
  }
}

pub struct QuinnTunnel<S: quinn::crypto::Session> {
  connection: quinn::generic::Connection<S>,
  side: TunnelSide,
  incoming: Arc<tokio::sync::Mutex<TunnelIncoming>>,
}

impl<S: quinn::crypto::Session> QuinnTunnel<S> {
  pub fn into_inner(
    self,
  ) -> (
    quinn::generic::Connection<S>,
    TunnelSide,
    Arc<tokio::sync::Mutex<TunnelIncoming>>,
  ) {
    (self.connection, self.side, self.incoming)
  }
}
impl<S> Sided for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn side(&self) -> TunnelSide {
    self.side
  }
}

impl<S> TunnelUplink for QuinnTunnel<S>
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

  fn addr(&self) -> TunnelAddressInfo {
    TunnelAddressInfo::Socket(self.connection.remote_address())
  }
}

impl<S> Tunnel for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn downlink<'a>(&'a self) -> BoxFuture<'a, Option<Box<dyn TunnelDownlink + Send + Unpin>>> {
    self
      .incoming
      .clone()
      .lock_owned()
      .map(|x| Some(Box::new(x) as Box<_>))
      .boxed()
  }
}

pub fn from_quinn_endpoint<S>(
  new_connection: quinn::generic::NewConnection<S>,
  side: TunnelSide,
) -> QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  let quinn::generic::NewConnection {
    connection,
    bi_streams,
    ..
  } = new_connection;
  let stream_tunnels = bi_streams
    .map_ok(|(send, recv)| {
      TunnelIncomingType::BiStream(WrappedStream::Boxed(Box::new(recv), Box::new(send)))
    })
    .map_err(Into::into)
    .boxed();
  QuinnTunnel {
    connection,
    side,
    incoming: Arc::new(tokio::sync::Mutex::new(TunnelIncoming {
      inner: stream_tunnels,
      side,
    })),
  }
}

pub struct DuplexTunnel {
  channel_to_remote: UnboundedSender<WrappedStream>,
  side: TunnelSide,
  incoming: Arc<tokio::sync::Mutex<TunnelIncoming>>,
}

impl Sided for DuplexTunnel {
  fn side(&self) -> TunnelSide {
    self.side
  }
}

impl TunnelUplink for DuplexTunnel {
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

impl Tunnel for DuplexTunnel {
  fn downlink<'a>(&'a self) -> BoxFuture<'a, Option<Box<dyn TunnelDownlink + Send + Unpin>>> {
    self
      .incoming
      .clone()
      .lock_owned()
      .map(|x| Some(Box::new(x) as Box<_>))
      .boxed()
  }
}

/// Two entangled ([Tunnel], [TunnelIncoming]) pairs
/// Each pair maps its [Tunnel] to the opposite member's entangled [TunnelIncoming]
pub struct EntangledTunnels {
  pub listener: DuplexTunnel,
  pub connector: DuplexTunnel,
}

impl Into<(DuplexTunnel, DuplexTunnel)> for EntangledTunnels {
  fn into(self) -> (DuplexTunnel, DuplexTunnel) {
    (self.listener, self.connector)
  }
}

/// Produces two entangled ([Tunnel], [TunnelIncoming]) pairs
/// Each pair maps its [Tunnel] to the opposite member's entangled [TunnelIncoming]
pub fn duplex() -> EntangledTunnels {
  fn duplex_for(
    up: UnboundedSender<WrappedStream>,
    down: UnboundedReceiver<WrappedStream>,
    side: TunnelSide,
  ) -> DuplexTunnel {
    use tokio_stream::wrappers::UnboundedReceiverStream;
    let down = UnboundedReceiverStream::new(down);
    let incoming_inner = down.map(TunnelIncomingType::BiStream).map(Ok).boxed();
    let incoming = TunnelIncoming {
      inner: incoming_inner,
      side,
    };
    DuplexTunnel {
      channel_to_remote: up,
      side,
      incoming: Arc::new(tokio::sync::Mutex::new(incoming)),
    }
  }
  let (left_up, right_down) = mpsc::unbounded_channel::<WrappedStream>();
  let (right_up, left_down) = mpsc::unbounded_channel::<WrappedStream>();
  let (listener, connector) = (
    duplex_for(left_up, left_down, TunnelSide::Listen),
    duplex_for(right_up, right_down, TunnelSide::Connect),
  );
  EntangledTunnels {
    listener,
    connector,
  }
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum TunnelError {
  #[error("Connection closed")]
  ConnectionClosed,
  #[error("Connection closed by application")]
  ApplicationClosed,
  #[error("Connection timed out")]
  TimedOut,
  #[error("Transport error encountered")]
  TransportError,
  #[error("Connection closed locally")]
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

#[derive(Debug, Copy, Clone)]
pub enum TunnelSide {
  Connect,
  Listen,
}

#[derive(Debug, Clone)]
pub enum TunnelAddressInfo {
  Unidentified,
  Socket(SocketAddr),
  Port(u16),
}

impl std::string::ToString for TunnelAddressInfo {
  fn to_string(&self) -> String {
    match self {
      Self::Unidentified => String::from("Unidentified"),
      Self::Socket(socket_addr) => socket_addr.to_string(),
      Self::Port(port) => port.to_string(),
    }
  }
}

pub trait Sided {
  fn side(&self) -> TunnelSide;
}

pub trait TunnelUplink: Sided {
  fn addr(&self) -> TunnelAddressInfo {
    TunnelAddressInfo::Unidentified
  }

  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream, TunnelError>>;
}

pub trait TunnelDownlink: Sided {
  fn as_stream<'a>(&'a mut self) -> BoxStream<'a, Result<TunnelIncomingType, TunnelError>>;
}

impl<TDownlink: std::ops::Deref> Sided for TDownlink
where
  TDownlink::Target: TunnelDownlink,
{
  fn side(&self) -> TunnelSide {
    self.deref().side()
  }
}

impl<TDownlink: std::ops::Deref + std::ops::DerefMut> TunnelDownlink for TDownlink
where
  TDownlink::Target: TunnelDownlink,
{
  fn as_stream<'a>(&'a mut self) -> BoxStream<'a, Result<TunnelIncomingType, TunnelError>> {
    self.deref_mut().as_stream()
  }
}

pub trait Tunnel: TunnelUplink + Send + Sync + Unpin {
  fn downlink<'a>(&'a self) -> BoxFuture<'a, Option<Box<dyn TunnelDownlink + Send + Unpin>>>;
}

pub enum TunnelIncomingType {
  BiStream(WrappedStream),
}

pub struct TunnelIncoming {
  inner: BoxStream<'static, Result<TunnelIncomingType, TunnelError>>,
  side: TunnelSide,
}

impl TunnelIncoming {
  pub fn side(&self) -> TunnelSide {
    self.side
  }

  pub fn streams(self) -> BoxStream<'static, Result<TunnelIncomingType, TunnelError>> {
    self.inner
  }

  pub fn streams_ref<'a>(&'a mut self) -> BoxStream<'a, Result<TunnelIncomingType, TunnelError>> {
    self.inner.borrow_mut().boxed()
  }
}

impl Sided for TunnelIncoming {
  fn side(&self) -> TunnelSide {
    self.side
  }
}

impl TunnelDownlink for TunnelIncoming {
  fn as_stream<'a>(&'a mut self) -> BoxStream<'a, Result<TunnelIncomingType, TunnelError>> {
    self.inner.borrow_mut().boxed()
  }
}

#[cfg(test)]
mod tests {
  use super::EntangledTunnels;
  use crate::common::protocol::tunnel::TunnelUplink;
  use futures::{AsyncReadExt, AsyncWriteExt, TryStreamExt};
  use std::sync::Arc;
  use tokio::io::AsyncWrite;

  #[tokio::test]
  async fn duplex_tunnel() {
    use super::Tunnel;
    use futures::StreamExt;
    let (a_tun, b_tun) = super::duplex().into();

    let fut = async move {
      a_tun.open_link().await.unwrap();
      let (a_inc, b_inc) = futures::future::join(a_tun.downlink(), b_tun.downlink()).await;
      let (mut a_inc, mut b_inc) = (a_inc.unwrap(), b_inc.unwrap());
      drop(a_tun); // Dropping the A tunnel ends the incoming streams for B
      let count_of_b: u32 = b_inc
        .as_stream()
        .fold(0, async move |memo, _stream| memo + 1)
        .await;
      assert_eq!(count_of_b, 1);
      b_tun.open_link().await.unwrap();
      drop(b_tun); // Dropping the B tunnel ends the incoming streams for A
      let count_of_a: u32 = a_inc
        .as_stream()
        .fold(0, async move |memo, _stream| memo + 1)
        .await;
      assert_eq!(count_of_a, 1);
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), fut)
      .await
      .expect("DuplexTunnel test may be failing due to an await deadlock");
  }

  #[tokio::test]
  async fn duplex_tunnel_concurrency() {
    use super::{Tunnel, TunnelIncomingType};
    use crate::util::tunnel_stream::{TunnelStream, WrappedStream};
    use futures::{future, FutureExt, StreamExt};
    use std::time::Duration;
    use tokio::{sync::Mutex, time::timeout};
    let EntangledTunnels {
      listener: server,
      connector: client,
    } = super::duplex();

    // For this test, the server will listen for two incoming streams,
    // echo the content down new outgoing streams, and stop listening.
    //
    // Note that DuplexTunnel's "Incoming", unlike a Quinn connection,
    // provides bistreams in the order they are opened by the tunnels,
    // not the order in which they have their first data sent on them.
    let fut_server = async move {
      let server_ref = &server;
      server
        .downlink()
        .await
        .unwrap()
        .as_stream()
        .take(2)
        .try_filter_map(|x| {
          future::ready(match x {
            TunnelIncomingType::BiStream(stream) => Ok(Some(stream)),
          })
        })
        .try_for_each_concurrent(None, async move |stream: WrappedStream| {
          let (mut incoming_downlink, _incoming_uplink) = tokio::io::split(stream);
          let (_outgoing_downlink, mut outgoing_uplink) =
            tokio::io::split(server_ref.open_link().await.unwrap());
          crate::util::proxy_tokio_stream(&mut incoming_downlink, &mut outgoing_uplink)
            .await
            .unwrap();
          Ok(())
        })
        .await
        .unwrap();
    };
    // The client will attempt to make 3 streams, and will expect that the third will produce no result
    let fut_client = async move {
      let client_ref = &client; // Explicit move
      let mut downlink = client.downlink().await.unwrap();
      let inc_streams = Arc::new(Mutex::new(downlink.as_stream()));
      const CLIENT_TASK_DURATION: Duration = Duration::from_secs(5);

      // Async Barriers are used to ensure that we are making new
      // streams in order and able to fulfill them them out-of-order
      use tokio::sync::Barrier;
      let step_1 = Barrier::new(2);
      let step_2 = Barrier::new(2);
      let step_3 = Barrier::new(3);
      let step_4 = Barrier::new(3);

      let client_a = {
        let (tun, inc_streams) = (client_ref, Arc::clone(&inc_streams));
        let (step_1, step_2, step_3, step_4) = (&step_1, &step_2, &step_3, &step_4);
        let task = async move {
          let test_data_a = vec![1, 2, 3, 4];
          use std::io::BufWriter;
          let mut s: Box<dyn TunnelStream> = Box::new(tun.open_link().await.unwrap());
          s.write_all(test_data_a.as_slice()).await.unwrap();
          AsyncWriteExt::flush(&mut s).await.unwrap();
          // Wait until B finishes starting a stream and sending on it, then receive a stream
          println!("a1");
          step_1.wait().await;
          let inc = inc_streams
            .lock()
            .await
            .try_next()
            .await
            .expect("Server must not close before sending a stream")
            .expect("Server must produce one stream per stream sent");
          let mut downlink = match inc {
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
        let (tun, inc_streams) = (client_ref, Arc::clone(&inc_streams));
        let (step_1, step_2, step_3, step_4) = (&step_1, &step_2, &step_3, &step_4);
        let task = async move {
          let test_data_b = vec![4, 3, 2];
          use std::io::BufWriter;
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
            .try_next()
            .await
            .expect("Server closed before responding to stream")
            .expect("Server must produce one stream per stream sent");
          let mut downlink = match inc {
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
          let last = inc_streams.lock().await.try_next().await.unwrap();
          assert!(matches!(last, None));
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
