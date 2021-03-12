use std::sync::Arc;

use futures::{
  future::{BoxFuture, FutureExt},
  Future,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::util::tunnel_stream::{TunnelStream, WrappedStream};

use super::{traits::ServiceRegistry, RouteAddress, Service};

/// Identifies the SNOCAT protocol over a stream
pub const SNOCAT_NEGOTIATION_MAGIC: &[u8; 4] = &[0x4e, 0x59, 0x41, 0x4e]; // UTF-8 "NYAN"

#[derive(Debug, Clone)]
pub enum NegotiationError {
  ReadError,
  WriteError,
  ProtocolViolation,
  Refused,
  UnsupportedProtocolVersion,
  UnsupportedServiceVersion,
}

/// Write future to send our magic and version to the remote,
/// returning an error if writes are refused by the stream.
async fn write_magic_and_version<S: AsyncWrite + Send + Unpin>(
  mut stream: S,
  protocol_version: u8,
) -> Result<S, NegotiationError> {
  crate::util::framed::write_frame(&mut stream, SNOCAT_NEGOTIATION_MAGIC)
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
      return Err(NegotiationError::ProtocolViolation);
    }
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
  .boxed()
}

pub struct NegotiationClient;

impl NegotiationClient {
  pub fn handle(
    self,
    addr: RouteAddress,
    mut link: WrappedStream,
  ) -> BoxFuture<'static, Result<WrappedStream, NegotiationError>> {
    const LOCAL_PROTOCOL_VERSION: u8 = 0;
    async move {
      // Absolute most-basic negotiation protocol - sends the address in a frame and waits for 0u8-or-fail

      let remote_protocol_version = protocol_magic(&mut link, LOCAL_PROTOCOL_VERSION).await?;
      // TODO: Consider adding a confirmation for negotiation protocol acceptance here

      // TODO: support multiple versions of negotiation mechanism
      if remote_protocol_version > 0 {
        // We don't support anything beyond this basic protocol yet
        return Err(NegotiationError::UnsupportedProtocolVersion);
      }

      // TODO: support service/client protocol versioning (May require Service/Client cooperation)

      // Write address to the remote, and see if the requested protocol is supported
      crate::util::framed::write_frame(&mut link, &addr.into_bytes())
        .await
        .map_err(|_| NegotiationError::WriteError)?;

      // Await acceptance of address by a service, or refusal if none are compatible
      let accepted = link
        .read_u8()
        .await
        .map_err(|_| NegotiationError::ReadError)?;
      if accepted > 0 {
        // For v0, this byte doesn't carry any useful info beyond accepted or not
        Err(NegotiationError::Refused)
      } else {
        Ok(link)
      }
    }
    .boxed()
  }
}

pub struct NegotiationServer<ServiceRegistry> {
  service_registry: Arc<ServiceRegistry>,
}

pub type ArcService = Arc<dyn Service + Send + Sync + 'static>;

impl<R> NegotiationServer<R> {}

impl<R> NegotiationServer<R>
where
  R: ServiceRegistry + Send + Sync + 'static,
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
  ) -> BoxFuture<'a, Result<(S, RouteAddress, ArcService), NegotiationError>> {
    const CURRENT_PROTOCOL_VERSION: u8 = 0u8;
    let service_registry = Arc::clone(&self.service_registry);
    async move {
      let remote_version = protocol_magic(&mut link, CURRENT_PROTOCOL_VERSION).await?;
      // TODO: Consider adding a confirmation for negotiation protocol acceptance here

      if remote_version > 0 {
        // This should map to multiple supported versions, where possible
        return Err(NegotiationError::UnsupportedProtocolVersion);
      }

      let addr = crate::util::framed::read_frame_vec(&mut link)
        .await
        .map_err(|_| NegotiationError::ProtocolViolation)?; // Address must be sent as a frame in v0

      let addr = String::from_utf8(addr).map_err(|_| NegotiationError::ProtocolViolation)?; // Addresses must be valid UTF-8

      let found = service_registry.find_service(&addr);

      match found {
        None => {
          // Write refusal
          // v0 calls for a non-zero u8 to be written to the stream to refuse an address
          link
            .write_u8(1)
            .await
            .map_err(|_| NegotiationError::WriteError)?;
          Err(NegotiationError::Refused)
        }
        Some(service) => {
          // Write acceptance
          // v0 calls for a 0u8 to be written to the stream to accept an address
          link
            .write_u8(0)
            .await
            .map_err(|_| NegotiationError::WriteError)?;
          Ok((link, addr, service))
        }
      }
    }
    .boxed()
  }
}

#[cfg(test)]
mod tests {
  /// Test that negotiation between client and server sends an address successfully
  #[tokio::test]
  async fn negotiate() {}
}
