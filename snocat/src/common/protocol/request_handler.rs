// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use crate::common::protocol::negotiation::{NegotiationClient, NegotiationError};
use futures::future::{BoxFuture, FutureExt, TryFutureExt};
use std::{backtrace::Backtrace, sync::Arc};

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
  service_registry: Arc<dyn ServiceRegistry + Send + Sync + 'static>,
  router: Arc<dyn Router + Send + Sync + 'static>,
}

#[derive(thiserror::Error, Debug)]
pub enum RequestHandlingError {
  #[error("Route not found for request {0:?}")]
  RouteNotFound(Request),
  #[error("Route found but unavailable for request")]
  RouteUnavailable(Request),
  #[error("The Protocol Client failed when handling the request")]
  ProtocolClientError(#[from] ClientError),
  #[error("Protocol negotiation failed")]
  NegotiationError(#[from] NegotiationError, Backtrace),
}

impl RequestClientHandler {
  pub fn new(
    tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static>,
    service_registry: Arc<dyn ServiceRegistry + Send + Sync + 'static>,
    router: Arc<dyn Router + Send + Sync + 'static>,
  ) -> Self {
    Self {
      tunnel_registry,
      service_registry,
      router,
    }
  }

  /// Routes a request and returns its ProtocolClient::Response-typed response.
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

  /// Handles making a request through a provided link, skipping the routing phase
  pub fn handle_direct<TProtocolClient: Client + Send + Sync + 'static>(
    self: Arc<Self>,
    direct_address: RouteAddress,
    client: TProtocolClient,
    link: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<'static, Result<TProtocolClient::Response, RequestHandlingError>> {
    // TODO: if Router no longer requires a full Request object, this can avoid boxing
    let request = Request {
      address: direct_address.clone(),
      protocol_client: Box::new(client),
    };
    self
      .handle_dynamic_direct(request, direct_address, link)
      .map_ok(|response: Response| {
        *response
          .into_inner()
          .downcast::<TProtocolClient::Response>()
          .expect("Contained response type must match that of the protocol client that produced it")
      })
      .boxed()
  }

  /// Handles making a request through a provided link, skipping the routing phase,
  /// but with the possibility of returning a type that may not match the one expected.
  pub fn handle_dynamic_direct(
    self: Arc<Self>,
    request: Request,
    direct_address: RouteAddress,
    link: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<'static, Result<Response, RequestHandlingError>> {
    async move {
      tracing::trace!("Running protocol negotiation");
      let link = self.negotiate_link(&direct_address, link).await?;
      tracing::trace!("Running protocol client");
      use tracing_futures::Instrument;
      let protocol_client_span = tracing::debug_span!("protocol_client", addr=?direct_address);
      let result = request
        .protocol_client
        .handle_dynamic(direct_address, link)
        .instrument(protocol_client_span)
        .await;

      let response: Response = match result {
        Ok(response) => response,
        Err(e) => {
          tracing::debug!(error=?e, "Protocol client failure");
          return Err(e)?;
        }
      };

      Result::<Response, RequestHandlingError>::Ok(response)
    }
    .boxed()
  }

  /// Routes a request and returns its response with dynamic/"Any" typing.
  pub fn handle_dynamic(
    self: Arc<Self>,
    request: Request,
  ) -> BoxFuture<'static, Result<Response, RequestHandlingError>> {
    let router = Arc::clone(&self.router);
    let tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync + 'static> =
      Arc::clone(&self.tunnel_registry);
    async move {
      // Note: Type-annotated because rust-analyzer fails to resolve typings here on its own
      let (resolved_address, link): (RouteAddress, Box<dyn TunnelStream + Send + 'static>) =
        match router.route(&request, tunnel_registry).await {
          Err(RoutingError::NoMatchingTunnel) => {
            return Err(RequestHandlingError::RouteNotFound(request));
          }
          Err(RoutingError::LinkOpenFailure(_e)) => {
            return Err(RequestHandlingError::RouteUnavailable(request));
          }
          Ok((resolved_address, tunnel)) => (resolved_address, tunnel),
        };

      self
        .handle_dynamic_direct(request, resolved_address, link)
        .await
    }
    .boxed()
  }

  pub fn negotiate_link(
    self: Arc<Self>,
    addr: &RouteAddress,
    link: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<'static, Result<Box<dyn TunnelStream + Send + 'static>, NegotiationError>> {
    use tracing_futures::Instrument;
    let addr = addr.clone();
    let negotiation_client = NegotiationClient;
    let negotiation_span = tracing::debug_span!("negotiation", addr=?addr);
    async move {
      let link = negotiation_client.handle(addr, link).await?;
      Ok(link)
    }
    .instrument(negotiation_span)
    .boxed()
  }
}
