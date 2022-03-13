// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use std::{fmt::Debug, sync::Arc};

use futures::{
  future::{BoxFuture, FutureExt},
  Future,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing_futures::Instrument;

use crate::util::tunnel_stream::TunnelStream;

use super::{
  traits::{MappedService, ServiceRegistry},
  tunnel::{Tunnel, TunnelId},
  RouteAddress, ServiceError,
};

/// Identifies the SNOCAT protocol over a stream
pub const SNOCAT_NEGOTIATION_MAGIC: &[u8; 4] = &[0x4e, 0x59, 0x41, 0x4e]; // UTF-8 "NYAN"

#[derive(thiserror::Error, Debug)]
pub enum NegotiationError<ApplicationError> {
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
  #[error("Negotiation application error: {0:?}")]
  ApplicationError(ApplicationError),
  #[error("Negotiation fatal error: {0:?}")]
  FatalError(ApplicationError),
}

impl<ApplicationError> NegotiationError<ApplicationError> {
  pub fn map_err<F, TErr>(self, f: F) -> NegotiationError<F::Output>
  where
    F: FnOnce(ApplicationError) -> TErr,
  {
    match self {
      NegotiationError::ReadError => NegotiationError::ReadError,
      NegotiationError::WriteError => NegotiationError::WriteError,
      NegotiationError::ProtocolViolation => NegotiationError::ProtocolViolation,
      NegotiationError::Refused => NegotiationError::Refused,
      NegotiationError::UnsupportedProtocolVersion => NegotiationError::UnsupportedProtocolVersion,
      NegotiationError::UnsupportedServiceVersion => NegotiationError::UnsupportedServiceVersion,
      NegotiationError::ApplicationError(e) => NegotiationError::ApplicationError(f(e)),
      NegotiationError::FatalError(e) => NegotiationError::FatalError(f(e)),
    }
  }

  pub fn err_into<TNewErr: From<ApplicationError>>(self) -> NegotiationError<TNewErr> {
    self.map_err(TNewErr::from)
  }
}

impl<SourceError: Into<OutError>, OutError> From<NegotiationError<SourceError>>
  for ServiceError<OutError>
{
  fn from(e: NegotiationError<SourceError>) -> Self {
    match e {
      NegotiationError::ReadError => ServiceError::UnexpectedEnd,
      NegotiationError::WriteError => ServiceError::UnexpectedEnd,
      NegotiationError::ProtocolViolation => ServiceError::IllegalResponse,
      NegotiationError::Refused => ServiceError::Refused,
      NegotiationError::UnsupportedProtocolVersion => ServiceError::Refused,
      NegotiationError::UnsupportedServiceVersion => ServiceError::Refused,
      NegotiationError::ApplicationError(e) => ServiceError::InternalError(e.into()),
      NegotiationError::FatalError(e) => ServiceError::InternalError(e.into()),
    }
  }
}

/// Write future to send our magic and version to the remote,
/// returning an error if writes are refused by the stream.
#[tracing::instrument(level = tracing::Level::TRACE, err, skip(stream))]
async fn write_magic_and_version<S: AsyncWrite + Send + Unpin, AE: Debug>(
  mut stream: S,
  protocol_version: u8,
) -> Result<S, NegotiationError<AE>> {
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
  Result::<S, NegotiationError<AE>>::Ok(stream)
}

// Note: Protocol v0 is symmetric until negotiation handshake completes
fn protocol_magic<'a, S: TunnelStream + Send + 'a, AE: Debug + 'a>(
  stream: S,
  protocol_version: u8,
) -> impl Future<Output = Result<u8, NegotiationError<AE>>> + 'a {
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
    Result::<_, NegotiationError<AE>>::Ok(read)
  };

  async move {
    let (read, write) = futures::future::try_join(read_magic, send_magic).await?;
    let mut stream = read.unsplit(write);
    let remote_version = stream
      .read_u8()
      .await
      .map_err(|_| NegotiationError::ReadError)?;
    Ok(remote_version)
  }
  .instrument(tracing::trace_span!(
    stringify!(protocol_magic),
    ?protocol_version
  ))
}

pub struct NegotiationClient;

impl NegotiationClient {
  pub fn new() -> Self {
    Self {}
  }

  pub fn negotiate<'stream, S, AE: Debug + 'stream>(
    self,
    addr: RouteAddress,
    mut link: S,
  ) -> impl Future<Output = Result<S, NegotiationError<AE>>> + 'stream
  where
    S: TunnelStream + Send + 'stream,
    for<'a> &'a mut S: TunnelStream + Send + 'a,
  {
    const LOCAL_PROTOCOL_VERSION: u8 = 0;
    let negotiation_span = tracing::trace_span!("protocol_negotiation_client", addr=?addr);
    async move {
      // Absolute most-basic negotiation protocol - sends the address in a frame and waits for 0u8-or-fail

      tracing::trace!("performing negotiation protocol handshake");
      let remote_version = protocol_magic::<&mut S, AE>(&mut link, LOCAL_PROTOCOL_VERSION).await?;
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
    .instrument(negotiation_span)
  }
}

pub struct NegotiationService<ServiceRegistry: ?Sized> {
  service_registry: Arc<ServiceRegistry>,
}

pub type ArcService<TServiceError> = Arc<dyn MappedService<TServiceError> + Send + Sync + 'static>;

impl<R: ?Sized> NegotiationService<R> {
  pub fn new(service_registry: Arc<R>) -> Self {
    Self { service_registry }
  }
}

impl<R> NegotiationService<R>
where
  R: ServiceRegistry + Send + Sync + ?Sized,
{
  /// Performs negotiation, returning the stream if successful
  ///
  /// If the negotiation task is dropped, the stream is dropped in an indeterminate state.
  /// In scenarios involving an owned stream, this will drop the stream, otherwise the
  /// other end of the stream may be at an unknown point in the protocol. As such, any
  /// timeout mechanism here must not expect to resume the stream after a ref drop.
  pub fn negotiate<'stream, S, TTunnel>(
    &self,
    mut link: S,
    tunnel: TTunnel,
  ) -> BoxFuture<
    'stream,
    Result<
      (S, RouteAddress, ArcService<<R as ServiceRegistry>::Error>),
      NegotiationError<anyhow::Error>,
    >,
  >
  where
    R: 'stream,
    S: TunnelStream + Send + 'stream,
    for<'a> &'a mut S: TunnelStream + Send + 'a,
    TTunnel: Tunnel + 'static,
  {
    const CURRENT_PROTOCOL_VERSION: u8 = 0u8;
    let service_registry = Arc::clone(&self.service_registry);
    let tunnel_id = *tunnel.id();
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

      let addr: RouteAddress = crate::util::framed::read_frame(&mut link, Some(2048))
        .await
        .map_err(|_| NegotiationError::ProtocolViolation) // Address must be sent as a frame in v0
        // Addresses must be valid UTF-8
        .and_then(|raw| String::from_utf8(raw).map_err(|_| NegotiationError::ProtocolViolation))
        // Addresses must be legal SlashAddrs
        .and_then(|raw| raw.parse().map_err(|_| NegotiationError::ProtocolViolation))?;

      tracing::trace!("searching service registry for address handlers");
      let found = service_registry.find_service(&addr, &tunnel_id);

      match found {
        None => {
          // Write refusal
          // v0 calls for a non-zero u8 to be written to the stream to refuse an address
          tracing::trace!(?addr, "refusing address");
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
    .instrument(tracing::trace_span!("protocol_negotiation_service", source_tunnel=?tunnel_id))
    .boxed()
  }
}

#[cfg(test)]
mod tests {
  use futures::{FutureExt, TryStreamExt};
  use std::{sync::Arc, time::Duration};
  use tokio::time::timeout;

  use super::{ArcService, NegotiationClient, NegotiationError, NegotiationService};
  use crate::common::protocol::{
    traits::{MappedService, ServiceRegistry},
    tunnel::{
      duplex::EntangledTunnels, ArcTunnel, Tunnel, TunnelDownlink, TunnelId, TunnelIncomingType,
      TunnelUplink,
    },
    Service,
  };

  struct TestServiceRegistry {
    services: Vec<ArcService<<Self as ServiceRegistry>::Error>>,
  }

  impl ServiceRegistry for TestServiceRegistry {
    type Error = anyhow::Error;

    fn find_service(
      self: std::sync::Arc<Self>,
      addr: &crate::common::protocol::RouteAddress,
      tunnel_id: &TunnelId,
    ) -> Option<std::sync::Arc<dyn MappedService<Self::Error> + Send + Sync + 'static>> {
      self
        .services
        .iter()
        .find(|s| s.accepts_mapped(addr, tunnel_id))
        .map(Arc::clone)
    }
  }

  struct NoOpServiceAcceptAll;

  impl Service for NoOpServiceAcceptAll {
    type Error = anyhow::Error;

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
      _tunnel: ArcTunnel,
    ) -> futures::future::BoxFuture<
      '_,
      Result<(), crate::common::protocol::ServiceError<Self::Error>>,
    > {
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
    tracing::subscriber::with_default(collector, || async move {
      const TEST_ADDR: &str = "/test/addr";
      let service_registry = TestServiceRegistry {
        services: vec![Arc::new(NoOpServiceAcceptAll)],
      };
      let EntangledTunnels {
        connector,
        listener,
      } = super::super::tunnel::duplex::channel();

      let service = NegotiationService::new(Arc::new(service_registry));
      let client = NegotiationClient::new();

      let client_future = async move {
        let client_stream = connector
          .open_link()
          .await
          .expect("Must open client stream");
        let _stream = client
          .negotiate(
            TEST_ADDR.parse().expect("Illegal test address"),
            client_stream,
          )
          .await?;
        Result::<_, NegotiationError<anyhow::Error>>::Ok(())
      };

      let server_future = async move {
        // server
        let server_stream = listener
          .downlink()
          .await
          .expect("Must successfully fetch server downlink")
          .as_stream()
          .try_next()
          .await
          .expect("Must fetch next connection");
        let server_stream = match server_stream {
          Some(TunnelIncomingType::BiStream(s)) => s,
          Some(other) => panic!("Non-bistream opened to the test server"),
          None => panic!("No stream was opened to the test server"),
        };
        let (_stream, addr, service) = service.negotiate(server_stream, listener).await?;
        Result::<_, NegotiationError<anyhow::Error>>::Ok((addr, service))
      };
      let fut = futures::future::try_join(client_future, server_future);
      let fut = timeout(Duration::from_secs(5), fut);
      let ((), (addr, _service)) = fut.await.expect("Must not time out").unwrap();
      assert_eq!(&addr.to_string(), TEST_ADDR);
    })
    .await;
  }
}
