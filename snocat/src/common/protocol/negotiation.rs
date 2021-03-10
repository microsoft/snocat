use futures::future::BoxFuture;

use crate::util::tunnel_stream::WrappedStream;

use super::RouteAddress;

#[derive(Debug, Clone)]
pub enum NegotiationError {
  // TODO: find error classes
}

pub struct NegotiationClient;

impl NegotiationClient {
  pub fn handle(
    self,
    addr: RouteAddress,
    link: WrappedStream<'static>,
  ) -> BoxFuture<Result<(), NegotiationError>> {
    todo!()
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
