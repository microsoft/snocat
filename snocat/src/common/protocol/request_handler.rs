use futures::future::{BoxFuture, FutureExt, TryFutureExt};
use std::sync::Arc;

use crate::common::protocol::{
  traits::{ServiceRegistry, TunnelRegistry},
  Client, Request, Response, RouteAddress, Router, RoutingError,
};
use crate::{
  common::protocol::ClientError,
  util::tunnel_stream::{TunnelStream, WrappedStream},
};

pub struct RequestClientHandler {
  tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
  router: Arc<dyn Router + Send + Sync + 'static>,
}

#[derive(Debug)]
pub enum RequestHandlingError {
  RouteNotFound(Request),
  ProtocolClientError(ClientError),
}

impl RequestClientHandler {
  pub fn new(
    tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
    router: Arc<dyn Router + Send + Sync + 'static>,
  ) -> Self {
    Self {
      tunnel_registry,
      router,
    }
  }

  pub fn handle<TProtocolClient: Client + Send + Sync + 'static>(
    self: Arc<Self>,
    address: RouteAddress,
    client: TProtocolClient,
  ) -> BoxFuture<'static, Result<TProtocolClient::Response, RequestHandlingError>> {
    // TODO: if Router no longer requires a full Request object, this can avoid boxing
    let request = Request {
      address,
      protocol_client: Box::new(client),
    };
    self
      .handle_dynamic(request)
      .map_ok(|response: Response| {
        *response
          .into_inner()
          .downcast::<TProtocolClient::Response>()
          .expect("Contained response type must match that of the protocol client that produced it")
      })
      .boxed()
  }

  pub fn handle_dynamic(
    self: Arc<Self>,
    request: Request,
  ) -> BoxFuture<'static, Result<Response, RequestHandlingError>> {
    let router = Arc::clone(&self.router);
    let tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static> =
      Arc::clone(&self.tunnel_registry);
    async move {
      // Note: Type-annotated because rust-analyzer fails to resolve typings here on its own
      let (resolved_address, tunnel): (RouteAddress, Box<dyn TunnelStream + Send + 'static>) =
        match router.route(&request, tunnel_registry).await {
          Err(RoutingError::NoMatchingTunnel) => {
            return Err(RequestHandlingError::RouteNotFound(request));
          }
          Ok((resolved_address, tunnel)) => (resolved_address, tunnel),
        };

      let response: Response = request
        .protocol_client
        .handle_dynamic(resolved_address, tunnel)
        .await
        .map_err(RequestHandlingError::ProtocolClientError)?;

      Result::<Response, RequestHandlingError>::Ok(response)
    }
    .boxed()
  }
}
