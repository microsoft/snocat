use futures::future::{BoxFuture, FutureExt, TryFutureExt};
use std::sync::Arc;

use crate::common::protocol::{
  request_handler::{RequestClientHandler, RequestHandlingError},
  traits::{ServiceRegistry, TunnelRegistry},
  Client, Request, Response, RouteAddress, Router, RoutingError,
};
use crate::{
  common::protocol::ClientError,
  util::tunnel_stream::{TunnelStream, WrappedStream},
};

pub struct ModularServer {
  service_registry: Arc<dyn ServiceRegistry + Send + Sync + 'static>,
  tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
  router: Arc<dyn Router + Send + Sync + 'static>,
  request_handler: Arc<RequestClientHandler>,
}

impl ModularServer {
  pub fn new(
    service_registry: Arc<dyn ServiceRegistry + Send + Sync + 'static>,
    tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
    router: Arc<dyn Router + Send + Sync + 'static>,
  ) -> Self {
    Self {
      request_handler: Arc::new(RequestClientHandler::new(
        Arc::clone(&tunnel_registry),
        Arc::clone(&router),
      )),
      service_registry,
      tunnel_registry,
      router,
    }
  }
}

impl ModularServer
where
  Self: 'static,
{
  pub fn run(self: Arc<Self>) -> tokio::task::JoinHandle<Result<(), ()>> {
    let _this = Arc::clone(&self);
    todo!();
  }

  pub fn requests<'a>(&'a self) -> &Arc<RequestClientHandler> {
    &self.request_handler
  }
}
