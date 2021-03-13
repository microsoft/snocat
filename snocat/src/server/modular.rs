// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use futures::{
  future::{BoxFuture, FutureExt, TryFutureExt},
  Stream,
};
use std::sync::Arc;
use triggered::Listener;

use crate::common::protocol::{
  request_handler::{RequestClientHandler, RequestHandlingError},
  traits::{ServiceRegistry, TunnelRegistry},
  tunnel::BoxedTunnelPair,
  Client, Request, Response, RouteAddress, Router, RoutingError,
};
use crate::{
  common::protocol::ClientError,
  util::tunnel_stream::{TunnelStream, WrappedStream},
};

pub struct ModularServer<TunnelSource> {
  service_registry: Arc<dyn ServiceRegistry + Send + Sync + 'static>,
  tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
  router: Arc<dyn Router + Send + Sync + 'static>,
  request_handler: Arc<RequestClientHandler>,
  tunnel_source: Arc<TunnelSource>,
}

impl<TunnelSource> ModularServer<TunnelSource> {
  pub fn requests<'a>(&'a self) -> &Arc<RequestClientHandler> {
    &self.request_handler
  }
}

impl<TunnelSource> ModularServer<TunnelSource>
where
  Self: 'static,
  TunnelSource: Stream<Item = BoxedTunnelPair<'static>> + Send + Sync + Unpin + 'static,
{
  pub fn new(
    service_registry: Arc<dyn ServiceRegistry + Send + Sync + 'static>,
    tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
    router: Arc<dyn Router + Send + Sync + 'static>,
    tunnel_source: Arc<TunnelSource>,
  ) -> Self {
    Self {
      request_handler: Arc::new(RequestClientHandler::new(
        Arc::clone(&tunnel_registry),
        Arc::clone(&router),
      )),
      service_registry,
      tunnel_registry,
      router,
      tunnel_source,
    }
  }

  pub fn run(
    self: Arc<Self>,
    _shutdown_request_listener: Listener,
  ) -> tokio::task::JoinHandle<Result<(), ()>> {
    let _this = Arc::clone(&self);

    todo!();
  }
}
