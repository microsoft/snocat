// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use std::sync::Arc;

use futures::{
  future::{BoxFuture, FutureExt},
  Future,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing_futures::Instrument;

use crate::util::tunnel_stream::TunnelStream;

use super::{traits::ServiceRegistry, tunnel::TunnelId, RouteAddress, Service};

/// Identifies the SNOCAT protocol over a stream
pub const SNOCAT_NEGOTIATION_MAGIC: &[u8; 4] = &[0x4e, 0x59, 0x41, 0x4e]; // UTF-8 "NYAN"

#[derive(thiserror::Error, Debug)]
pub enum NegotiationError {
  #[error("Stream read failed")]
  ReadError,
  #[error("Stream write failed")]
  WriteError,
  #[error("Protocol violated by remote")]
  ProtocolViolation,
  #[error("Protocol refused")]
  Refused,
  #[error("Protocol version not supported")]
  UnsupportedProtocolVersion,
  #[error("Service version not supported")]
  UnsupportedServiceVersion,
  #[error(transparent)]
  ApplicationError(#[from] anyhow::Error),
  #[error(transparent)]
  FatalError(anyhow::Error),
}

/// Write future to send our magic and version to the remote,
/// returning an error if writes are refused by the stream.
async fn write_magic_and_version<S: AsyncWrite + Send + Unpin>(
  mut stream: S,
  protocol_version: u8,
) -> Result<S, NegotiationError> {
  stream
    .write_all(SNOCAT_NEGOTIATION_MAGIC)
    .await
    .map_err(|_| NegotiationError::WriteError)?;
  stream
    .write_u8(protocol_version)
    .await
    .map_err(|_| NegotiationError::WriteError)?;
  stream
    .flush()
    .await
    .map_err(|_| NegotiationError::WriteError)?;
  Result::<S, NegotiationError>::Ok(stream)
}

// Note: Protocol v0 is symmetric until negotiation handshake completes
fn protocol_magic<'a, S: TunnelStream + Send + 'a>(
  stream: &'a mut S,
  protocol_version: u8,
) -> BoxFuture<'a, Result<u8, NegotiationError>> {
  let (mut read, write) = tokio::io::split(stream);
  // Write future to send our magic and version to the remote,
  // returning an error if writes are refused by the stream.
  let send_magic = write_magic_and_version(write, protocol_version);
  // Read future to get the magic from the remote, returning an error on magic mismatch
  let read_magic = async {
    let mut remote_magic = [0u8; 4];
    let remote_magic_len = read
      .read_exact(&mut remote_magic)
      .await
      .map_err(|_| NegotiationError::ProtocolViolation)?;
    if remote_magic_len < remote_magic.len() || &remote_magic != SNOCAT_NEGOTIATION_MAGIC {
      tracing::trace!("magic mismatch");
      return Err(NegotiationError::ProtocolViolation);
    }
    tracing::trace!("magic matched expectation");
    Result::<_, NegotiationError>::Ok(read)
  };

  async move {
    let (read, write) = futures::future::try_join(read_magic, send_magic).await?;
    let stream = read.unsplit(write);
    let remote_version = stream
      .read_u8()
      .await
      .map_err(|_| NegotiationError::ReadError)?;
    Ok(remote_version)
  }
  .instrument(tracing::trace_span!(stringify!(protocol_magic)))
  .boxed()
}

pub struct NegotiationClient;

impl NegotiationClient {
  pub fn new() -> Self {
    Self {}
  }

  pub fn handle<S>(
    self,
    addr: RouteAddress,
    mut link: S,
  ) -> BoxFuture<'static, Result<S, NegotiationError>>
  where
    S: TunnelStream + Send + 'static,
  {
    const LOCAL_PROTOCOL_VERSION: u8 = 0;
    async move {
      // Absolute most-basic negotiation protocol - sends the address in a frame and waits for 0u8-or-fail

      tracing::trace!("performing negotiation protocol handshake");
      let remote_version = protocol_magic(&mut link, LOCAL_PROTOCOL_VERSION).await?;
      // TODO: Consider adding a confirmation for negotiation protocol acceptance here

      // TODO: support multiple versions of negotiation mechanism
      if remote_version > 0 {
        // We don't support anything beyond this basic protocol yet
        tracing::trace!(
          version = remote_version,
          "unsupported remote protocol version"
        );
        return Err(NegotiationError::UnsupportedProtocolVersion);
      }

      // TODO: support service/client protocol versioning (May require Service/Client cooperation)

      tracing::trace!("writing address");
      // Write address to the remote, and see if the requested protocol is supported
      crate::util::framed::write_frame(&mut link, &addr.into_bytes())
        .await
        .map_err(|_| NegotiationError::WriteError)?;

      tracing::trace!("awaiting remote protocol service acceptance");
      // Await acceptance of address by a service, or refusal if none are compatible
      let accepted = link
        .read_u8()
        .await
        .map_err(|_| NegotiationError::ReadError)?;
      if accepted > 0 {
        // For v0, this byte doesn't carry any useful info beyond accepted or not
        tracing::trace!(
          code = accepted,
          "address refused by remote protocol services"
        );
        Err(NegotiationError::Refused)
      } else {
        tracing::trace!("address accepted by remote protocol services");
        Ok(link)
      }
    }
    .instrument(tracing::trace_span!("Client"))
    .boxed()
  }
}

pub struct NegotiationService<ServiceRegistry: ?Sized> {
  service_registry: Arc<ServiceRegistry>,
}

pub type ArcService = Arc<dyn Service + Send + Sync + 'static>;

impl<R: ?Sized> NegotiationService<R> {
  pub fn new(service_registry: Arc<R>) -> Self {
    Self { service_registry }
  }
}

impl<R> NegotiationService<R>
where
  R: ServiceRegistry + Send + Sync + ?Sized + 'static,
{
  /// Performs negotiation, returning the stream if successful
  ///
  /// If the negotiation task is dropped, the stream is dropped in an indeterminate state.
  /// In scenarios involving an owned stream, this will drop the stream, otherwise the
  /// other end of the stream may be at an unknown point in the protocol. As such, any
  /// timeout mechanism here must not expect to resume the stream after a ref drop.
  pub fn negotiate<'a, S: TunnelStream + Send + 'a>(
    &self,
    mut link: S,
    tunnel_id: TunnelId,
  ) -> BoxFuture<'a, Result<(S, RouteAddress, ArcService), NegotiationError>> {
    const CURRENT_PROTOCOL_VERSION: u8 = 0u8;
    let service_registry = Arc::clone(&self.service_registry);
    async move {
      tracing::trace!("performing negotiation protocol handshake");
      let remote_version = protocol_magic(&mut link, CURRENT_PROTOCOL_VERSION).await?;
      // TODO: Consider adding a confirmation for negotiation protocol acceptance here

      if remote_version > 0 {
        // This should map to multiple supported versions, where possible
        tracing::trace!(
          version = remote_version,
          "unsupported remote protocol version"
        );
        return Err(NegotiationError::UnsupportedProtocolVersion);
      }

      let addr = crate::util::framed::read_frame_vec(&mut link)
        .await
        .map_err(|_| NegotiationError::ProtocolViolation)?; // Address must be sent as a frame in v0

      let addr = String::from_utf8(addr).map_err(|_| NegotiationError::ProtocolViolation)?; // Addresses must be valid UTF-8

      tracing::trace!("searching service registry for address handlers");
      let found = service_registry.find_service(&addr, &tunnel_id);

      match found {
        None => {
          // Write refusal
          // v0 calls for a non-zero u8 to be written to the stream to refuse an address
          tracing::trace!("refusing address");
          link
            .write_u8(1)
            .await
            .map_err(|_| NegotiationError::WriteError)?;
          Err(NegotiationError::Refused)
        }
        Some(service) => {
          // Write acceptance
          // v0 calls for a 0u8 to be written to the stream to accept an address
          tracing::trace!("accepting address");
          link
            .write_u8(0)
            .await
            .map_err(|_| NegotiationError::WriteError)?;
          Ok((link, addr, service))
        }
      }
    }
    .instrument(tracing::trace_span!("Service"))
    .boxed()
  }
}

#[cfg(test)]
mod tests {
  use std::{
    sync::{Arc, Weak},
    time::Duration,
  };
  use tokio::time::timeout;

  use super::{ArcService, NegotiationClient, NegotiationError, NegotiationService};
  use crate::common::protocol::{
    traits::ServiceRegistry,
    tunnel::{Tunnel, TunnelId},
    Service,
  };
  use crate::util::tunnel_stream::TunnelStream;

  struct TestServiceRegistry {
    services: Vec<ArcService>,
  }

  impl ServiceRegistry for TestServiceRegistry {
    fn find_service(
      self: std::sync::Arc<Self>,
      addr: &crate::common::protocol::RouteAddress,
      tunnel_id: &TunnelId,
    ) -> Option<std::sync::Arc<dyn crate::common::protocol::Service + Send + Sync + 'static>> {
      self
        .services
        .iter()
        .find(|s| s.accepts(addr, tunnel_id))
        .map(Arc::clone)
    }
  }

  struct NoOpServiceAcceptAll;

  impl Service for NoOpServiceAcceptAll {
    fn accepts(
      &self,
      _addr: &crate::common::protocol::RouteAddress,
      _tunnel_id: &TunnelId,
    ) -> bool {
      true
    }

    fn handle(
      &'_ self,
      _addr: crate::common::protocol::RouteAddress,
      _stream: Box<dyn crate::util::tunnel_stream::TunnelStream + Send + 'static>,
      _tunnel_id: TunnelId,
    ) -> futures::future::BoxFuture<'_, Result<(), crate::common::protocol::ServiceError>> {
      use futures::FutureExt;
      futures::future::ready(Ok(())).boxed()
    }
  }

  /// Test that negotiation between client and server sends an address successfully
  #[tokio::test]
  async fn negotiate() {
    let collector = tracing_subscriber::fmt()
      .pretty()
      .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
      .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
      .finish();
    tracing::subscriber::set_global_default(collector).expect("Logger init must succeed");
    const TEST_ADDR: &str = "/test/addr";
    let service_registry = TestServiceRegistry {
      services: vec![Arc::new(NoOpServiceAcceptAll)],
    };
    let service = NegotiationService::new(Arc::new(service_registry));
    let client = NegotiationClient::new();
    use crate::common::util::tunnel_stream::WrappedStream;
    let (client_stream, server_stream) = WrappedStream::duplex(8192);

    let client_future = async move {
      let _stream = client.handle(TEST_ADDR.into(), client_stream).await?;
      Result::<_, NegotiationError>::Ok(())
    };

    let server_future = async move {
      // server
      let (_stream, addr, service) = service
        .negotiate(server_stream, TunnelId::new(1u64))
        .await?;
      Result::<_, NegotiationError>::Ok((addr, service))
    };
    let fut = futures::future::try_join(client_future, server_future);
    let fut = timeout(Duration::from_secs(5), fut);
    let ((), (addr, _service)) = fut.await.expect("Must not time out").unwrap();
    assert_eq!(addr.as_str(), TEST_ADDR);
  }
}
