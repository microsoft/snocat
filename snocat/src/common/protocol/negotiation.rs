use futures::future::{BoxFuture, FutureExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::util::tunnel_stream::WrappedStream;

use super::RouteAddress;

const SNOCAT_NEGOTIATION_MAGIC: &[u8; 4] = &[0x4e, 0x59, 0x41, 0x4e]; // UTF-8 "NYAN"

#[derive(Debug, Clone)]
pub enum NegotiationError {
  ReadError,
  WriteError,
  ProtocolViolation,
  Refused,
  UnsupportedProtocolVersion,
  UnsupportedServiceVersion,
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
      // Send and receive negotiation magic header
      crate::util::framed::write_frame(&mut link, SNOCAT_NEGOTIATION_MAGIC)
        .await
        .map_err(|_| NegotiationError::ProtocolViolation)?;
      link
        .write_u8(0)
        .await
        .map_err(|_| NegotiationError::WriteError)?;
      link
        .flush()
        .await
        .map_err(|_| NegotiationError::WriteError)?;

      // Verify remote protocol magic matches our expectation
      {
        let mut remote_magic = [0u8; 4];
        let remote_magic_len = link
          .read_exact(&mut remote_magic)
          .await
          .map_err(|_| NegotiationError::ProtocolViolation)?;
        if remote_magic_len < remote_magic.len() || &remote_magic != SNOCAT_NEGOTIATION_MAGIC {
          return Err(NegotiationError::ProtocolViolation);
        }
      }

      // TODO: support multiple versions of negotiation mechanism
      let remote_protocol_version = link
        .read_u8()
        .await
        .map_err(|_| NegotiationError::ProtocolViolation)?;
      if remote_protocol_version > 0 {
        // We don't support anything beyond this basic protocol yet
        return Err(NegotiationError::UnsupportedProtocolVersion);
      }

      // TODO: support protocol versioning (May require Service / Client cooperation)

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
  service_registry: ServiceRegistry,
}

impl<ServiceRegistry> NegotiationServer<ServiceRegistry>
where
  ServiceRegistry: Send,
{
  // pub fn dispatch();
  // pub fn negotiate();
}

#[cfg(test)]
mod tests {
  /// Test that negotiation between client and server sends an address successfully
  #[tokio::test]
  async fn negotiate() {}
}
