// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! Sources both listen- and connection-based tunnels

use futures::stream::{BoxStream, Stream, StreamExt, TryStreamExt};
use std::{net::SocketAddr, pin::Pin, task::Poll};

use super::protocol::tunnel::{from_quinn_endpoint, BoxedTunnelPair, TunnelSide};

pub struct QuinnListenEndpoint<Session: quinn::crypto::Session> {
  bind_addr: SocketAddr,
  quinn_config: quinn::generic::ServerConfig<Session>,
  endpoint: quinn::generic::Endpoint<Session>,
  incoming: BoxStream<'static, quinn::generic::NewConnection<Session>>,
}

impl<Session: quinn::crypto::Session + 'static> QuinnListenEndpoint<Session> {
  pub fn bind(
    bind_addr: SocketAddr,
    quinn_config: quinn::generic::ServerConfig<Session>,
  ) -> Result<Self, quinn::EndpointError> {
    let mut builder = quinn::generic::Endpoint::builder();
    builder.listen(quinn_config.clone());
    let (endpoint, incoming) = builder.bind(&bind_addr)?;
    let incoming = incoming
      .filter_map(|connecting| async move { connecting.await.ok() })
      .boxed();
    Ok(Self {
      bind_addr,
      quinn_config,
      endpoint,
      incoming,
    })
  }
}

impl<Session> Stream for QuinnListenEndpoint<Session>
where
  Session: quinn::crypto::Session + 'static,
  Self: Unpin,
{
  type Item = BoxedTunnelPair<'static>;

  fn poll_next(
    mut self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Option<Self::Item>> {
    let res = futures::ready!(Stream::poll_next(Pin::new(&mut self.incoming), cx));
    match res {
      None => Poll::Ready(None),
      Some(new_connection) => {
        let (tunnel, incoming) = from_quinn_endpoint(new_connection, TunnelSide::Listen);
        Poll::Ready(Some((Box::new(tunnel), incoming)))
      }
    }
  }
}

// /// Merges outputs from a runtime-editable set of endpoint ports
// pub struct QuinnDynamicEndpointSet { }
