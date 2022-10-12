// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![forbid(unused_imports, dead_code)]
use std::sync::Arc;

use futures::{future::BoxFuture, FutureExt, StreamExt};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::{
  common::protocol::tunnel::{
    Sided, Tunnel, TunnelDownlink, TunnelError, TunnelIncoming, TunnelIncomingType, TunnelSide,
    TunnelUplink,
  },
  util::tunnel_stream::WrappedStream,
};

use super::{Baggage, TunnelId, WithTunnelId};

pub struct DuplexTunnel<B = ()> {
  id: TunnelId,
  channel_to_remote: UnboundedSender<WrappedStream>,
  side: TunnelSide,
  incoming: Arc<tokio::sync::Mutex<TunnelIncoming>>,
  baggage: Arc<B>,
}

impl<B> WithTunnelId for DuplexTunnel<B> {
  fn id(&self) -> &TunnelId {
    &self.id
  }
}

impl<B> Sided for DuplexTunnel<B> {
  fn side(&self) -> TunnelSide {
    self.side
  }
}

impl<B> TunnelUplink for DuplexTunnel<B> {
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
pub struct EntangledTunnels<BL = (), BC = ()> {
  pub listener: DuplexTunnel<BL>,
  pub connector: DuplexTunnel<BC>,
}

impl<BL, BC> Into<(DuplexTunnel<BL>, DuplexTunnel<BC>)> for EntangledTunnels<BL, BC> {
  fn into(self) -> (DuplexTunnel<BL>, DuplexTunnel<BC>) {
    (self.listener, self.connector)
  }
}

pub fn channel() -> EntangledTunnels {
  channel_with_baggage((), ())
}
/// Produces two entangled ([Tunnel], [TunnelIncoming]) pairs
/// Each pair maps its [Tunnel] to the opposite member's entangled [TunnelIncoming]
pub fn channel_with_baggage<BL, BC>(
  listener_baggage: BL,
  connector_baggage: BC,
) -> EntangledTunnels<BL, BC> {
  fn duplex_for<B>(
    id: TunnelId,
    up: UnboundedSender<WrappedStream>,
    down: UnboundedReceiver<WrappedStream>,
    side: TunnelSide,
    baggage: B,
  ) -> DuplexTunnel<B> {
    use tokio_stream::wrappers::UnboundedReceiverStream;
    let down = UnboundedReceiverStream::new(down);
    let incoming_inner = down.map(TunnelIncomingType::BiStream).map(Ok).boxed();
    let incoming = TunnelIncoming {
      id,
      inner: incoming_inner,
      side,
    };
    DuplexTunnel {
      id,
      channel_to_remote: up,
      side,
      incoming: Arc::new(tokio::sync::Mutex::new(incoming)),
      baggage: Arc::new(baggage),
    }
  }
  let (left_up, right_down) = mpsc::unbounded_channel::<WrappedStream>();
  let (right_up, left_down) = mpsc::unbounded_channel::<WrappedStream>();
  let (listener, connector) = (
    duplex_for(
      TunnelId::new(0),
      left_up,
      left_down,
      TunnelSide::Listen,
      listener_baggage,
    ),
    duplex_for(
      TunnelId::new(1),
      right_up,
      right_down,
      TunnelSide::Connect,
      connector_baggage,
    ),
  );
  EntangledTunnels {
    listener,
    connector,
  }
}

/// Allows attachment of arbitrary data to the lifetime of the tunnel object
///
/// If a value is only required until disconnection, perform cleanup with an
/// `on_closed` handle, and use a Mutex<Option<T>> to represent removed bags
///
/// It is strictly illegal to store a reference to a tunnel in a bag. Memory
/// leaks cannot be reasoned about if any tunnel can extend the lifetimes of
/// any tunnel (including itself) beyond the scope of a live Request. Module
/// handles (`TunnelRegistry`, `ServiceRegistry`, etc) must all be WeakRefs.
impl<B> Baggage for DuplexTunnel<B> {
  type Bag<'a> = Arc<B> where B: 'a;

  fn bag<'a>(&'a self) -> Self::Bag<'a> {
    self.baggage.clone()
  }
}

#[cfg(test)]
mod tests {
  use super::EntangledTunnels;
  use crate::common::protocol::tunnel::TunnelUplink;
  use futures::{AsyncReadExt, AsyncWriteExt, TryStreamExt};
  use std::sync::Arc;

  #[tokio::test]
  async fn duplex_tunnel() {
    use super::Tunnel;
    use futures::StreamExt;
    let (a_tun, b_tun) = super::channel().into();

    let fut = async move {
      a_tun.open_link().await.unwrap();
      let (a_inc, b_inc) = futures::future::join(a_tun.downlink(), b_tun.downlink()).await;
      let (mut a_inc, mut b_inc) = (a_inc.unwrap(), b_inc.unwrap());
      drop(a_tun); // Dropping the A tunnel ends the incoming streams for B
      let count_of_b: usize = b_inc.as_stream().count().await;
      assert_eq!(count_of_b, 1);
      b_tun.open_link().await.unwrap();
      drop(b_tun); // Dropping the B tunnel ends the incoming streams for A
      let count_of_a: usize = a_inc.as_stream().count().await;
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
    use futures::{future, StreamExt};
    use std::time::Duration;
    use tokio::{sync::Mutex, time::timeout};
    let EntangledTunnels {
      listener: server,
      connector: client,
    } = super::channel();

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
        .try_for_each_concurrent(None, |stream: WrappedStream| async move {
          let (mut incoming_downlink, _incoming_uplink) = tokio::io::split(stream);
          let (_outgoing_downlink, mut outgoing_uplink) =
            tokio::io::split(server_ref.open_link().await.unwrap());
          tokio::io::copy(&mut incoming_downlink, &mut outgoing_uplink)
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
