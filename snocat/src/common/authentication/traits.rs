use crate::ext::future::TryFutureExtExt;
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#[warn(unused_imports)]
use crate::{
  common::protocol::tunnel::{
    Tunnel, TunnelAddressInfo, TunnelError, TunnelIncomingType, TunnelName, TunnelSide,
  },
  util::{cancellation::CancellationListener, tunnel_stream::TunnelStream},
};
use futures::{future::BoxFuture, FutureExt, TryFutureExt, TryStreamExt};
use std::{
  collections::HashMap,
  fmt::Debug,
  marker::{PhantomData, Unpin},
  sync::Arc,
};

#[derive(Debug, Clone)]
pub struct TunnelInfo {
  pub side: TunnelSide,
  pub addr: TunnelAddressInfo,
}

/// Some errors within the authentication layer are considered fatal to the authenticator
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum AuthenticationHandlingError<TInner> {
  #[error("Authentication dependency failure: {0} - {1}")]
  DependencyFailure(String, TInner),
  #[error(transparent)]
  ApplicationError(TInner),
  #[error("Authentication thread join failed")]
  JoinError(
    #[from]
    #[backtrace]
    tokio::task::JoinError,
  ),
}

impl<TInner> AuthenticationHandlingError<TInner> {
  pub fn map_err<TNew, F>(self, f: F) -> AuthenticationHandlingError<TNew>
  where
    F: FnOnce(TInner) -> TNew,
  {
    match self {
      Self::DependencyFailure(message, inner) => {
        AuthenticationHandlingError::DependencyFailure(message, f(inner))
      }
      Self::ApplicationError(inner) => AuthenticationHandlingError::ApplicationError(f(inner)),
      Self::JoinError(e) => AuthenticationHandlingError::JoinError(e),
    }
  }

  pub fn err_into<TNew>(self) -> AuthenticationHandlingError<TNew>
  where
    TInner: Into<TNew>,
  {
    self.map_err(Into::into)
  }
}

/// Errors explaining why authentication was refused
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
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
pub enum AuthenticationError<TInner> {
  #[error(transparent)]
  Handling(#[from] AuthenticationHandlingError<TInner>),
  #[error(transparent)]
  Remote(#[from] RemoteAuthenticationError),
}

impl<TInner> AuthenticationError<TInner> {
  pub fn to_nested_result<T>(
    res: Result<T, Self>,
  ) -> Result<Result<T, RemoteAuthenticationError>, AuthenticationHandlingError<TInner>> {
    match res {
      Ok(res) => Ok(Ok(res)),
      Err(AuthenticationError::Handling(e)) => Err(e),
      Err(AuthenticationError::Remote(e)) => Ok(Err(e)),
    }
  }

  pub fn from_nested_result<T>(
    res: Result<Result<T, RemoteAuthenticationError>, AuthenticationHandlingError<TInner>>,
  ) -> Result<T, Self> {
    match res {
      Ok(Ok(res)) => Ok(res),
      Err(e) => Err(AuthenticationError::Handling(e)),
      Ok(Err(e)) => Err(AuthenticationError::Remote(e)),
    }
  }

  pub fn map_err<TNew, F>(self, f: F) -> AuthenticationError<TNew>
  where
    F: FnOnce(TInner) -> TNew,
  {
    match self {
      Self::Handling(handling) => AuthenticationError::Handling(handling.map_err(f)),
      Self::Remote(remote) => AuthenticationError::Remote(remote),
    }
  }

  pub fn err_into<TNew>(self) -> AuthenticationError<TNew>
  where
    TInner: Into<TNew>,
  {
    self.map_err(Into::into)
  }
}

impl<TInner> From<std::convert::Infallible> for AuthenticationError<TInner> {
  fn from(_: std::convert::Infallible) -> Self {
    unreachable!()
  }
}

pub type AuthenticationAttributes = HashMap<String, Vec<u8>>;

pub trait AuthenticationHandler: std::fmt::Debug + Send + Sync {
  type Error: Send;

  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a CancellationListener,
  ) -> BoxFuture<'a, Result<(TunnelName, AuthenticationAttributes), AuthenticationError<Self::Error>>>;
}

#[derive(Copy, Clone)]
pub struct MappedAuthenticationHandler<F, TInner> {
  inner: TInner,
  f: F,
}

impl<F, TInner> Debug for MappedAuthenticationHandler<F, TInner>
where
  TInner: Debug,
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MappedAuthenticationHandler")
      .field("inner", &self.inner)
      .finish_non_exhaustive()
  }
}

impl<F, TInner, TOutput> AuthenticationHandler for MappedAuthenticationHandler<F, TInner>
where
  F: (Fn(<TInner as AuthenticationHandler>::Error) -> TOutput) + Send + Sync,
  TInner: AuthenticationHandler,
  TOutput: Send,
{
  type Error = TOutput;

  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a CancellationListener,
  ) -> BoxFuture<'a, Result<(TunnelName, AuthenticationAttributes), AuthenticationError<Self::Error>>>
  {
    self
      .inner
      .authenticate(channel, tunnel_info, shutdown_notifier)
      .map(move |r| r.map_err(|e| e.map_err(|ei| (&self.f)(ei))))
      .boxed()
  }
}

#[derive(Copy, Clone)]
#[repr(transparent)]
pub struct MappedErrIntoAuthenticationHandler<TInner, TOutput> {
  inner: TInner,
  phantom_output: PhantomData<std::sync::Arc<std::sync::Mutex<TOutput>>>,
}

impl<TInner, TOutput> Debug for MappedErrIntoAuthenticationHandler<TInner, TOutput>
where
  TInner: Debug,
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MappedErrIntoAuthenticationHandler")
      .field("inner", &self.inner)
      .finish_non_exhaustive()
  }
}

impl<TInner, TOutput> AuthenticationHandler for MappedErrIntoAuthenticationHandler<TInner, TOutput>
where
  TInner: AuthenticationHandler,
  TOutput: Send,
  TInner::Error: Into<TOutput>,
{
  type Error = TOutput;

  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a CancellationListener,
  ) -> BoxFuture<'a, Result<(TunnelName, AuthenticationAttributes), AuthenticationError<Self::Error>>>
  {
    self
      .inner
      .authenticate(channel, tunnel_info, shutdown_notifier)
      .map(|r| r.map_err(AuthenticationError::err_into))
      .boxed()
  }
}

pub trait AuthenticationHandlerExt: AuthenticationHandler {
  fn map_err<TNew, F>(self, f: F) -> MappedAuthenticationHandler<F, Self>
  where
    F: (Fn(<Self as AuthenticationHandler>::Error) -> TNew) + Send + Sync,
    Self: Sized,
  {
    MappedAuthenticationHandler { f, inner: self }
  }

  fn err_into<TNew>(self) -> MappedErrIntoAuthenticationHandler<Self, TNew>
  where
    Self::Error: Into<TNew>,
    Self: Sized,
  {
    MappedErrIntoAuthenticationHandler {
      inner: self,
      phantom_output: PhantomData,
    }
  }
}

impl<T: AuthenticationHandler> AuthenticationHandlerExt for T {}

impl<T: AuthenticationHandler + ?Sized> AuthenticationHandler for Box<T> {
  type Error = T::Error;

  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a CancellationListener,
  ) -> BoxFuture<'a, Result<(TunnelName, AuthenticationAttributes), AuthenticationError<Self::Error>>>
  {
    self
      .as_ref()
      .authenticate(channel, tunnel_info, shutdown_notifier)
  }
}

impl<T: AuthenticationHandler + ?Sized> AuthenticationHandler for Arc<T> {
  type Error = T::Error;

  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a CancellationListener,
  ) -> BoxFuture<'a, Result<(TunnelName, AuthenticationAttributes), AuthenticationError<Self::Error>>>
  {
    self
      .as_ref()
      .authenticate(channel, tunnel_info, shutdown_notifier)
  }
}

/// Convert a [TunnelError] to its equivalent [AuthenticationError]
fn tunnel_error_to_remote_auth_error(e: TunnelError) -> RemoteAuthenticationError {
  match e {
    TunnelError::ApplicationClosed => RemoteAuthenticationError::LinkClosedLocally,
    TunnelError::LocallyClosed => RemoteAuthenticationError::LinkClosedLocally,
    TunnelError::ConnectionClosed => RemoteAuthenticationError::LinkClosedRemotely,
    TunnelError::TimedOut => RemoteAuthenticationError::TimedOut,
    TunnelError::TransportError => RemoteAuthenticationError::TransportError,
  }
}

pub fn perform_authentication<'a, T: AuthenticationHandler + ?Sized>(
  handler: &'a T,
  tunnel: &'a (dyn Tunnel + Send + Sync + 'a),
  shutdown_notifier: &'a CancellationListener,
) -> BoxFuture<'a, Result<(TunnelName, AuthenticationAttributes), AuthenticationError<T::Error>>>
where
  T::Error: std::fmt::Debug + Send,
{
  use tracing::{debug, debug_span, warn, Instrument};
  let tunnel_info = TunnelInfo {
    side: tunnel.side(),
    addr: tunnel.addr(),
  };
  let tracing_span_authentication =
    debug_span!("authentication", side=?tunnel_info.side, addr=?tunnel_info.addr);

  // Obtains a channel with the peer which will be used for authentication
  let establishment = {
    let side = tunnel_info.side;
    async move {
      // Create a future which times out when the shutdown notifier is called; we'll use it for timeouts
      let shutdown_notifier = async move { shutdown_notifier.clone().cancelled().await };
      // Request an authenticator channel in the way appropriate for the current side
      // Authenticator channels are a single stream, and must maintain stream cohesion until authentication completes
      let auth_channel: Result<_, AuthenticationError<_>> = match side {
        TunnelSide::Listen => {
          // Open an uplink stream to the destination or time out
          tunnel.open_link()
            .instrument(debug_span!("open_link"))
            .try_poll_until_or_else(shutdown_notifier, || Err(TunnelError::TimedOut))
            .map_err(tunnel_error_to_remote_auth_error).map_err(AuthenticationError::from)
            .await
        },
        TunnelSide::Connect => {
          // Fetch the next downlink stream and use it or time out; Downlink streams are guaranteed to arive in-order-of-request
          tunnel
            .downlink()
            .await
            .ok_or(RemoteAuthenticationError::IncomingStreamsClosed)?
            .as_stream()
            .map_err(tunnel_error_to_remote_auth_error)
            .try_next()
            .try_poll_until_or_else(shutdown_notifier, || Err(RemoteAuthenticationError::TimedOut))
            .instrument(debug_span!("accept_link"))
            .and_then(|s: Option<TunnelIncomingType>| futures::future::ready(match s {
              Some(TunnelIncomingType::BiStream(stream)) => Ok(stream),
              _ => Err(RemoteAuthenticationError::IncomingStreamsClosed),
            }))
            .map_err(AuthenticationError::from)
            .await
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
    }.instrument(debug_span!("establishment"))
  };

  async move {
    let establishment: Result<_, AuthenticationError<_>> = establishment.await;
    let auth_channel = establishment?;
    handler
      .authenticate(Box::new(auth_channel), tunnel_info, shutdown_notifier)
      .instrument(debug_span!("authenticator"))
      .await
  }
  .instrument(tracing_span_authentication)
  .boxed()
}
