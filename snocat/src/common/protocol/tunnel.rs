use std::{net::SocketAddr, pin::Pin};

use crate::util::tunnel_stream::WrappedStream;
use futures::{
  future::{BoxFuture, Either},
  stream::{BoxStream, Stream, StreamFuture},
  StreamExt,
};
use quinn::{crypto::Session, generic::RecvStream, ApplicationClose, SendStream};
use tokio::io::{AsyncRead, AsyncWrite};

type BoxedTunnel<'a> = Box<dyn Tunnel + Send + Sync + Unpin + 'a>;
type BoxedTunnelPair<'a> = (BoxedTunnel<'a>, TunnelIncoming);

pub struct QuinnTunnel<S: quinn::crypto::Session> {
  connection: quinn::generic::Connection<S>,
}

impl<S> Tunnel for QuinnTunnel<S>
where
  S: quinn::crypto::Session + 'static,
{
  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream<'static>, TunnelError>> {
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

/// Produces two entangled [Tunnel] Pairs
/// Each pair maps its [Tunnel] to the opposite member's entangled [TunnelIncoming]
pub fn duplex() -> (BoxedTunnelPair<'static>, BoxedTunnelPair<'static>) {
  todo!()
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

pub trait Tunnel: Send + Sync + Unpin {
  fn info(&self) -> TunnelInfo {
    TunnelInfo::Unidentified
  }

  fn open_link(&self) -> BoxFuture<'static, Result<WrappedStream<'static>, TunnelError>>;
}

pub enum TunnelIncomingType {
  BiStream(WrappedStream<'static>),
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
  #[tokio::test]
  async fn duplex_tunnel() {
    use futures::StreamExt;
    let ((a_tun, a_inc), (b_tun, b_inc)) = super::duplex();

    let _link_to_b = a_tun.open_link().await;
    let count_of_b: u32 = super::TunnelIncoming::streams(b_inc)
      .fold(0, async move |memo, _stream| memo + 1)
      .await;
    assert_eq!(count_of_b, 1);
    let _link_to_a = b_tun.open_link().await;
    let count_of_a: u32 = super::TunnelIncoming::streams(a_inc)
      .fold(0, async move |memo, _stream| memo + 1)
      .await;
    assert_eq!(count_of_a, 1);
  }
}
