// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#[warn(unused_imports)]
use crate::{
  common::protocol::tunnel::{
    Tunnel, TunnelAddressInfo, TunnelError, TunnelIncomingType, TunnelName, TunnelSide,
  },
  util::tunnel_stream::TunnelStream,
};
use futures::{future::BoxFuture, FutureExt, TryStreamExt};
use std::marker::Unpin;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct TunnelInfo {
  pub side: TunnelSide,
  pub addr: TunnelAddressInfo,
}

/// Some errors within the authentication layer are considered fatal to the authenticator
#[derive(thiserror::Error, Debug)]
pub enum AuthenticationHandlingError {
  #[error("A dependency for authentication failed")]
  DependencyFailure(String, anyhow::Error),
  #[error(transparent)]
  ApplicationError(#[from] anyhow::Error),
  #[error(transparent)]
  FatalApplicationError(anyhow::Error),
}

/// Errors explaining why authentication was refused
#[derive(Error, Debug)]
pub enum RemoteAuthenticationError {
  // The remote failed to authenticate, but followed protocol
  #[error("Remote authentication refused")]
  Refused,
  // Yes, this is technically a local authentication error,
  // but it's not a fault with the authentication layer,
  // it's a reason the remote was not authenticated.
  #[error("Remote authentication link closed locally")]
  LinkClosedLocally,
  // The remote closed their [TunnelIncoming] and can't accept a new link
  #[error("Remote authentication link closed by the remote")]
  LinkClosedRemotely,
  // The remote closed their connection, so our [TunnelIncoming] reached end-of-stream.
  #[error("Connection closed by remote")]
  IncomingStreamsClosed,
  // The remote connection timed out at the transport level, or took too long to authenticate.
  #[error("Remote connection timed out")]
  TimedOut,
  // Still need to figure out where this one applies, maybe more specificity can be achieved.
  #[error("Transport error encountered authenticating remote")]
  TransportError,
  // Occurs when an auth protocol is not followed by the remote
  #[error("Remote authentication protocol violation: {0}")]
  ProtocolViolation(String),
}

#[derive(thiserror::Error, Debug)]
pub enum AuthenticationError {
  #[error(transparent)]
  Handling(#[from] AuthenticationHandlingError),
  #[error(transparent)]
  Remote(#[from] RemoteAuthenticationError),
}

impl AuthenticationError {
  pub fn to_nested_result<T>(
    res: Result<T, Self>,
  ) -> Result<Result<T, RemoteAuthenticationError>, AuthenticationHandlingError> {
    match res {
      Ok(res) => Ok(Ok(res)),
      Err(AuthenticationError::Handling(e)) => Err(e),
      Err(AuthenticationError::Remote(e)) => Ok(Err(e)),
    }
  }

  pub fn from_nested_result<T>(
    res: Result<Result<T, RemoteAuthenticationError>, AuthenticationHandlingError>,
  ) -> Result<T, Self> {
    match res {
      Ok(Ok(res)) => Ok(res),
      Err(e) => Err(AuthenticationError::Handling(e)),
      Ok(Err(e)) => Err(AuthenticationError::Remote(e)),
    }
  }
}

pub trait AuthenticationHandler: std::fmt::Debug + Send + Sync {
  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a CancellationToken,
  ) -> BoxFuture<'a, Result<TunnelName, AuthenticationError>>;
}

impl<T: AuthenticationHandler + ?Sized> AuthenticationHandler for Box<T> {
  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a CancellationToken,
  ) -> BoxFuture<'a, Result<TunnelName, AuthenticationError>> {
    self
      .as_ref()
      .authenticate(channel, tunnel_info, shutdown_notifier)
  }
}

pub fn perform_authentication<'a>(
  handler: &'a (impl AuthenticationHandler + ?Sized),
  tunnel: &'a (dyn Tunnel + Send + Sync + 'a),
  shutdown_notifier: &'a CancellationToken,
) -> BoxFuture<'a, Result<TunnelName, AuthenticationError>> {
  use tracing::{debug, span, warn, Instrument, Level};
  let tunnel_info = TunnelInfo {
    side: tunnel.side(),
    addr: tunnel.addr(),
  };
  let tracing_span_establishment = span!(Level::DEBUG, "establishment", side=?tunnel_info.side);
  let tracing_span_authentication =
    span!(Level::DEBUG, "authentication", side=?tunnel_info.side, addr=?tunnel_info.addr);
  let establishment = {
    let side = tunnel_info.side;
    async move {
      let auth_channel: Result<_, AuthenticationError> = match side {
        TunnelSide::Listen => {
          let link: Result<_, TunnelError> = tunnel.open_link()
            .instrument(span!(Level::DEBUG, "open_link"))
            .await;
          link.map_err(|e| match e {
            TunnelError::ApplicationClosed => RemoteAuthenticationError::LinkClosedLocally,
            TunnelError::LocallyClosed => RemoteAuthenticationError::LinkClosedLocally,
            TunnelError::ConnectionClosed => RemoteAuthenticationError::LinkClosedRemotely,
            TunnelError::TimedOut => RemoteAuthenticationError::TimedOut,
            TunnelError::TransportError => RemoteAuthenticationError::TransportError,
          }.into())
        },
        TunnelSide::Connect => {
          let next: Result<Option<_>, TunnelError> = tunnel
            .downlink()
            .await
            .ok_or(RemoteAuthenticationError::IncomingStreamsClosed)?
            .as_stream()
            .try_next()
            .instrument(span!(Level::DEBUG, "accept_link"))
            .await;

          match next {
            Ok(Some(TunnelIncomingType::BiStream(stream))) => Ok(stream),
            _ => Err(RemoteAuthenticationError::IncomingStreamsClosed.into()),
          }
        }
      };

      auth_channel.map_err(|e| match e {
        AuthenticationError::Handling(local_err) => {
          warn!(error=?local_err, "AuthenticationError reported during tunnel establishment phase");
          local_err.into()
        },
        AuthenticationError::Remote(remote_err) => {
          debug!(error=?remote_err, "Remote authentication failure reported in tunnel establishment phase");
          remote_err.into()
        }
      })
    }.instrument(tracing_span_establishment)
  };

  async move {
    let establishment: Result<_, AuthenticationError> = establishment.await;
    let auth_channel = establishment?;
    handler
      .authenticate(Box::new(auth_channel), tunnel_info, shutdown_notifier)
      .instrument(span!(Level::DEBUG, "authenticator"))
      .await
  }
  .instrument(tracing_span_authentication)
  .boxed()
}
