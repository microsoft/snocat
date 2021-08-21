// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use snocat::{
  common::protocol::{
    address::RouteAddressParseError,
    proxy_tcp::TcpStreamTarget,
    service::{
      Client, ClientError, ClientResult, ProtocolInfo, Request, RouteAddressBuilder, Router,
      RoutingError,
    },
    tunnel::{registry::TunnelRegistry, Tunnel, TunnelId},
    RouteAddress, Service, ServiceError,
  },
  server::PortRangeAllocator,
};
use std::{
  backtrace::Backtrace,
  marker::PhantomData,
  net::IpAddr,
  sync::{Arc, Weak},
};
use tokio_util::sync::CancellationToken;

use futures::future::{BoxFuture, FutureExt};
use std::net::SocketAddr;
use tracing_futures::Instrument;

use crate::util::tunnel_stream::TunnelStream;
use snocat::util::framed::{read_framed_json, write_framed_json};
use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::TcpListener};

type PortGrantedNotificationType = Option<Vec<SocketAddr>>;

#[derive(Debug, Clone)]
pub struct DemandProxyClient<'stream, TStream> {
  pub proxied_subject: String,
  stream: PhantomData<TStream>,
  stream_life: PhantomData<&'stream ()>,
}

impl<'stream, TStream> DemandProxyClient<'stream, TStream> {
  pub fn new(proxied_subject: String) -> Self {
    Self {
      proxied_subject,
      stream: PhantomData,
      stream_life: PhantomData,
    }
  }
}

impl<'stream, TStream> ProtocolInfo for DemandProxyClient<'stream, TStream> {
  fn protocol_name() -> &'static str
  where
    Self: Sized,
  {
    DEMAND_PROXY_PROTOCOL_NAME
  }
}

#[derive(thiserror::Error, Debug)]
pub enum DemandProxyAddressError {
  #[error("RFC6763 DNS-Based Service Discovery is not supported by DemandProxyClient")]
  RFC6763NotSupported,
  #[error(transparent)]
  UnparseableOutput(#[from] RouteAddressParseError),
}

impl<'stream, TStream> RouteAddressBuilder for DemandProxyClient<'stream, TStream> {
  type Params = TcpStreamTarget;
  type BuildError = DemandProxyAddressError;

  fn build_addr(args: Self::Params) -> Result<RouteAddress, Self::BuildError>
  where
    Self: Sized,
  {
    let (host, port) = <(String, Option<u16>)>::from(args);
    Ok(
      format!(
        "/{}/0.0.1/{}/{}",
        Self::protocol_name(),
        host,
        port.ok_or(DemandProxyAddressError::RFC6763NotSupported)?
      )
      .parse()?,
    )
  }
}

impl<'stream, TStream> Client<'stream, TStream> for DemandProxyClient<'stream, TStream>
where
  TStream: TunnelStream + 'stream,
{
  type Response = (
    Vec<SocketAddr>,
    BoxFuture<'stream, Result<(), ClientError<Self::Error>>>,
  );

  type Error = Option<Backtrace>;

  type Future = BoxFuture<'stream, ClientResult<'stream, Self, TStream>>;

  fn handle(self, addr: RouteAddress, mut tunnel: TStream) -> Self::Future {
    let span = tracing::span!(tracing::Level::DEBUG, "demand_proxy_client", target=?addr);
    let fut = async move {
      tracing::info!("Sending subject to service");
      write_framed_json(&mut tunnel, &self.proxied_subject, None)
        .await
        .map_err(|e| {
          tracing::debug!(error=?e);
          ClientError::IllegalResponse(Some(Backtrace::capture()))
        })?;
      tracing::info!(
        target = "demand_proxy_waiting",
        "Awaiting stream from service"
      );
      let entry: PortGrantedNotificationType =
        read_framed_json(&mut tunnel, None).await.map_err(|e| {
          tracing::debug!(error=?e);
          ClientError::IllegalResponse(Some(Backtrace::capture()))
        })?;
      let entry = entry.ok_or_else(|| {
        tracing::info!("Service refused to bind a socket for our request");
        ClientError::Refused
      })?;
      tracing::info!(remote_addr=?entry, "Provided stream information by service; waiting for remote closure");
      let fut_continuation = async move {
        tunnel.read_exact(&mut [0u8; 8]).await.map_err(|e| {
          tracing::debug!(error = (&e as &dyn std::error::Error));
          ClientError::IllegalResponse(Some(Backtrace::capture()))
        })?;
        tracing::info!(target = "demand_proxy_close", "Closing stream");
        Ok(())
      };
      Ok((entry, fut_continuation.boxed()))
    };
    fut.instrument(span).fuse().boxed()
  }
}

// TODO: This service is obsolete- listen on the server side for connection events instead
pub struct DemandProxyService<TTunnel, TTunnelRegistry, TRouter> {
  tunnel_registry: Weak<TTunnelRegistry>,
  router: Weak<TRouter>,
  port_range_allocator: PortRangeAllocator,
  bind_addrs: Arc<Vec<IpAddr>>,
  tunnel_phantom: std::marker::PhantomData<TTunnel>,
}

impl<TTunnel, TTunnelRegistry, TRouter> std::fmt::Debug
  for DemandProxyService<TTunnel, TTunnelRegistry, TRouter>
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DemandProxy server").finish_non_exhaustive()
  }
}

const DEMAND_PROXY_PROTOCOL_NAME: &'static str = "proxyme";
const DEMAND_PROXY_VERSION: &'static str = "0.0.1";

impl<TTunnel, TTunnelRegistry, TRouter> DemandProxyService<TTunnel, TTunnelRegistry, TRouter>
where
  TTunnel: Tunnel + Send + Sync + 'static,
  TTunnelRegistry: TunnelRegistry<TTunnel> + Send + Sync + 'static,
  TRouter: Router + Send + Sync + 'static,
  TRouter::Stream: TunnelStream + Send,
  TRouter::Error: Send,
{
  pub fn new(
    tunnel_registry: Weak<TTunnelRegistry>,
    router: Weak<TRouter>,
    port_range_allocator: PortRangeAllocator,
    mut bind_addrs: Vec<IpAddr>,
  ) -> Self {
    Self::handle_dual_stack_addrs(&mut bind_addrs);
    Self {
      tunnel_registry,
      router,
      port_range_allocator,
      bind_addrs: Arc::new(bind_addrs),
      tunnel_phantom: std::marker::PhantomData,
    }
  }

  async fn run_tcp_listener(
    target_addr: Arc<(Option<String>, u16)>,
    tcp_listener: TcpListener,
    weak_tunnel: Weak<TTunnel>,
    router: Weak<TRouter>,
    stop_accepting: CancellationToken,
  ) -> Result<(), ServiceError<TRouter::Error>> {
    use futures::stream::{StreamExt, TryStreamExt};
    let tcp_listener = tokio_stream::wrappers::TcpListenerStream::new(tcp_listener);
    tcp_listener
      .map_err(|_io_error| ServiceError::UnexpectedEnd)
      .take_until({
        let stop_accepting = stop_accepting.clone();
        Box::new(stop_accepting).cancelled()
        // async move { stop_accepting.cancelled().await }
      })
      .try_for_each_concurrent(None, move |tcp_stream| {
        let target_addr = target_addr.clone();
        let weak_tunnel = weak_tunnel.clone();
        let router = router.clone();
        async move {
          // TODO: Link is unused; probably an artifact of direct routing leftovers- see what's needed
          // Probably need to come up with a way to specify a tunnel for outgoing requests
          // when `route`'d, now that I think of it
          let link = {
            let tunnel = weak_tunnel
              .upgrade()
              .ok_or(ServiceError::DependencyFailure)?;
            tunnel
              .open_link()
              .await
              .or(Err(ServiceError::UnexpectedEnd))?
          };
          use snocat::common::protocol::proxy_tcp::{DnsTarget, TcpStreamClient};
          let (tcp_recv, tcp_send) = tokio::io::split(tcp_stream);
          let client = TcpStreamClient::new(tcp_recv, tcp_send);
          let target: TcpStreamTarget = DnsTarget::PreferHigher {
            host: target_addr
              .0
              .as_ref()
              .map(|s| s.as_str())
              .unwrap_or("localhost")
              .to_string(),
            port: target_addr.1,
          }
          .into();
          let req = Request::new(client, target).map_err(|_e| ServiceError::AddressError)?;
          router
            .upgrade()
            .ok_or(ServiceError::DependencyFailure)?
            .route(req)
            .await
            .map_err(|res| match res {
              RoutingError::RouteNotFound(_) => {
                unreachable!("Direct requests cannot fail to find a route")
              }
              RoutingError::RouteUnavailable(_) => ServiceError::DependencyFailure,
              RoutingError::RouterError(_) => ServiceError::DependencyFailure,
              RoutingError::LinkOpenFailure(_) => ServiceError::DependencyFailure,
              RoutingError::InvalidAddress => ServiceError::AddressError,
            })?
            .await
            .map_err(|res| match res {
              ClientError::InvalidAddress => ServiceError::AddressError,
              ClientError::Refused => ServiceError::Refused,
              ClientError::UnexpectedEnd => ServiceError::UnexpectedEnd,
              ClientError::IllegalResponse(_) => ServiceError::IllegalResponse,
            })?;

          Ok(())
        }
      })
      .await?;
    Ok(())
  }

  /// Handle forwarding concurrently across all streams requested by the TCP ports bound for each listener
  async fn run_tcp_listeners(
    bindings: Vec<TcpListener>,
    target_addr: (Option<String>, u16),
    weak_tunnel: Weak<TTunnel>,
    router: Weak<TRouter>,
    no_new_requests_listener: CancellationToken,
  ) -> Result<(), ServiceError<TRouter::Error>> {
    let span =
      tracing::span!(tracing::Level::DEBUG, "demand_proxy_forwarding", target = ?target_addr);
    use futures::stream::TryStreamExt;
    let parsed_addr = Arc::new(target_addr);
    let fut = futures::stream::iter(
      bindings
        .into_iter()
        .map(move |listener| {
          (
            listener,
            weak_tunnel.clone(),
            router.clone(),
            no_new_requests_listener.clone(),
            Arc::clone(&parsed_addr),
          )
        })
        .map(Result::<_, ServiceError<_>>::Ok),
    )
    .try_for_each_concurrent(
      None,
      |(listener, weak_tunnel, request_client_handler, no_new_requests_listener, parsed_addr)| {
        Self::run_tcp_listener(
          parsed_addr,
          listener,
          weak_tunnel,
          request_client_handler,
          no_new_requests_listener,
        )
      },
    )
    .instrument(span);
    // Run as a tokio::task to ensure scheduling across tunnels
    tokio::task::spawn(fut)
      // Treat JoinErrors as just another ServiceError
      .map(|res| match res {
        Err(_join_error) => Err(ServiceError::DependencyFailure),
        Ok(x) => x,
      })
      .await
  }
}

impl<TTunnel, TTunnelRegistry, TRouter> DemandProxyService<TTunnel, TTunnelRegistry, TRouter> {
  /// Removes incidents where one "unspecified" / dual-stack-mode IP will steal from others on the host
  fn handle_dual_stack_addrs(bind_addrs: &mut Vec<IpAddr>) {
    match bind_addrs.iter().find(|addr| addr.is_unspecified()) {
      Some(unspec) => {
        let unspec = unspec.clone();
        bind_addrs.clear();
        bind_addrs.push(unspec);
      }
      None => (),
    }
  }

  fn parse_address(addr: &RouteAddress) -> Result<(Option<&str>, u16), ()> {
    let mut segments = addr.iter_segments();
    // Verify the first segment is the protocol name
    segments.next().ok_or(()).and_then(|segment| {
      (segment == DEMAND_PROXY_PROTOCOL_NAME)
        .then(|| ())
        .ok_or(())
    })?;
    // Verify the second segment is the protocol version; for v0.0.1 we check strict equality
    segments
      .next()
      .ok_or(())
      .and_then(|segment| (segment == DEMAND_PROXY_VERSION).then(|| ()).ok_or(()))?;

    let (host, port_segment) = {
      let first = segments.next().ok_or(())?;
      if let Some(second) = segments.next() {
        (Some(first), second)
      } else {
        (None, first)
      }
    };

    if segments.next().is_some() {
      // Extra trailing segment, bail
      return Err(());
    }

    let port = port_segment.parse::<u16>().map_err(|_| ())?;
    Ok((host, port))
  }
}

impl<TTunnel, TTunnelRegistry, TRouter> Service
  for DemandProxyService<TTunnel, TTunnelRegistry, TRouter>
where
  TTunnel: Tunnel + Send + Sync + 'static,
  TTunnelRegistry: TunnelRegistry<TTunnel> + Send + Sync + 'static,
  TRouter: Router + Send + Sync + 'static,
  TRouter::Stream: TunnelStream + Send,
  TRouter::Error: Send,
{
  type Error = TRouter::Error;

  fn accepts(&self, addr: &RouteAddress, _tunnel_id: &TunnelId) -> bool {
    Self::parse_address(addr).is_ok()
  }

  fn handle(
    &'_ self,
    addr: RouteAddress,
    mut stream: Box<dyn TunnelStream + Send + 'static>,
    tunnel_id: TunnelId,
  ) -> BoxFuture<'_, Result<(), ServiceError<Self::Error>>> {
    tracing::debug!(
      "Demand proxy proxy connection request received with addr {}; building span...",
      addr
    );
    let tunnel_registry = Weak::clone(&self.tunnel_registry);
    let port_range_allocator = self.port_range_allocator.clone();
    let bind_addrs = Arc::clone(&self.bind_addrs);
    let parsed_addr = {
      let (host, port) = match Self::parse_address(&addr).or(Err(ServiceError::AddressError)) {
        Ok(x) => x,
        Err(e) => return futures::future::ready(Err(e)).boxed(),
      };
      (host.map(String::from), port)
    };
    let span = tracing::span!(tracing::Level::DEBUG, "demand_proxy", target = ?addr);
    let fut = async move {
      tracing::debug!("Demand Proxy active");
      let _subject: String = read_framed_json(&mut stream, Some(256))
        .await
        .map_err(|e| {
          tracing::debug!(error=?e, "Remote failed to provide a subject for the proxy demand");
          ServiceError::UnexpectedEnd
        })?;
      tracing::trace!("Discarding proxy demand subject as we do not yet use it");
      let weak_tunnel = {
        let tunnel_registry = tunnel_registry
          .upgrade()
          .ok_or(ServiceError::DependencyFailure)?;
        let tunnel = tunnel_registry
          .lookup_by_id(tunnel_id)
          .await
          .map_err(|_registry_error| ServiceError::AddressError)
          .and_then(|x| x.ok_or(ServiceError::DependencyFailure))?;
        if let Some(hatch) = tunnel.tunnel {
          Ok(Arc::downgrade(&hatch))
        } else {
          tracing::warn!(tunnel_id = ?tunnel.id, "Attempted to route to tunnel not available in the local registry");
          Err(ServiceError::AddressError)
        }?
      };
      let port = match port_range_allocator.allocate().await {
        Ok(port) => {
          // Notify the client of their allocated port
          write_framed_json(
            &mut stream,
            PortGrantedNotificationType::Some(
              bind_addrs
                .iter()
                .map(|addr| SocketAddr::new(addr.clone(), port.port()))
                .collect(),
            ),
            None,
          )
          .await
          .map_err(|_| ServiceError::IllegalResponse)?;
          port
        }
        Err(_port_allocation_error) => {
          // Notify the client that we couldn't allocate it a port
          write_framed_json(&mut stream, PortGrantedNotificationType::None, None)
            .await
            .map_err(|_| ServiceError::IllegalResponse)?;
          return Err(ServiceError::DependencyFailure);
        }
      };

      // Note: bindings run asynchronously, but sequentially; no concurrency is present here
      let bindings = {
        use futures::stream::{StreamExt, TryStreamExt};
        let port_number = port.port();
        futures::stream::iter(bind_addrs.iter())
          .then(|bind_ip| {
            let bind_addr = SocketAddr::new(*bind_ip, port_number);
            TcpListener::bind(bind_addr)
          })
          .try_collect::<Vec<_>>()
          .await
      }
      .map_err(|e| {
        ServiceError::InternalFailure(anyhow::Error::new(e).context("Binding port for client"))
      });

      let bindings = match bindings {
        Ok(bindings) => bindings,
        Err(e) => {
          // Notify the client that we couldn't allocate it a port
          tracing::warn!(
            "Port allocation failed for port {} on {:?}",
            port.port(),
            bind_addrs
          );
          write_framed_json(&mut stream, PortGrantedNotificationType::None, None)
            .await
            .map_err(|_| ServiceError::IllegalResponse)?;
          return Err(e);
        }
      };

      let no_new_requests = CancellationToken::new();

      let wait_for_client_close = {
        let no_new_requests = no_new_requests.clone();
        async move {
          tracing::trace!("Awaiting client closure");
          // By waiting for a read that never arrives, we can see when the stream closes
          let mut buf = [0u8; 8];
          stream.read_exact(&mut buf).map(|_| ()).await;
          tracing::trace!(content=?&buf, "Client closure requested, triggering...");
          // Then we trigger the "no new requests" event, to signify that the client is done
          no_new_requests.cancel();
          // After this, we could theoretically give a grace period to shut down remaining connections
          stream
        }
      }
      .instrument(tracing::trace_span!("close_waiter"))
      .boxed();

      // Handle forwarding concurrently across all streams requested by the TCP ports bound for each listener
      let tcp_listener_task = Self::run_tcp_listeners(
        bindings,
        parsed_addr,
        weak_tunnel,
        self.router.clone(),
        no_new_requests,
      );

      let (mut stream, listener_result) =
        futures::future::join(wait_for_client_close, tcp_listener_task).await;

      listener_result?;

      // Clean up the port allocation, ensuring it lived long enough to get here
      drop(port);

      stream
        .write_all(&mut [0u8; 8])
        .await
        .map_err(|_| ServiceError::UnexpectedEnd)?;
      Ok(())
    };

    fut.instrument(span).boxed()
  }
}
