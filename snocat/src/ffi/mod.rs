//! Bindings for instantiation and control via C ABI

use crate::ffi::errors::FfiError;
use crate::server::deferred::SnocatClientIdentifier;
use crate::util::tunnel_stream::TunnelStream;
use crate::{
  common::authentication::{self, TunnelInfo},
  server::{
    self,
    deferred::{ConcurrentDeferredTunnelServer, TunnelManager},
    PortRangeAllocator, TcpTunnelManager,
  },
  util::{self, vtdroppable::VTDroppable},
  DroppableCallback,
};
use anyhow::Context as AnyhowContext;
use ffi_support::{
  define_bytebuffer_destructor, define_handle_map_deleter, define_string_destructor,
  implement_into_ffi_by_json, rust_string_to_c, ByteBuffer, ConcurrentHandleMap, ExternError,
  FfiStr, Handle, HandleError, IntoFfi,
};
use futures::future::{self, BoxFuture, Either, Future, FutureExt};
use futures::{stream, AsyncReadExt, AsyncWriteExt, TryFutureExt, TryStream};
use futures_io::AsyncBufRead;
use lazy_static::lazy_static;
use std::marker::PhantomData;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{any::Any, slice};
use tokio::sync::{oneshot, Mutex};
use tokio::{sync::oneshot::error::RecvError, task};

pub mod dto;
pub mod errors;
pub mod proto;

mod allocators;
pub mod foreign_drop;
use foreign_drop::ForeignDropCallback;
pub mod delegation;
use delegation::{
  CompletionState, Delegation, DelegationError, DelegationResult, DelegationSet,
  RemoteError as FfiRemoteError,
};

pub mod eventing;
use eventing::{EventCompletionState, EventHandle, EventRunner, EventingError};

mod reactor;
pub use reactor::Reactor;

lazy_static! {
  static ref REACTOR: std::sync::Mutex<Option<Arc<Reactor>>> = std::sync::Mutex::new(None);
}

fn get_current_reactor() -> Arc<Reactor> {
  let reactor_ref = REACTOR.lock().expect("Reactor Read Lock poisoned");
  Arc::clone(reactor_ref.as_ref().expect("Reactor must be initialized"))
}

#[no_mangle]
pub extern "C" fn snocat_register_drop_callback(
  dropper: Option<foreign_drop::ForeignDropCallback>,
) -> () {
  let dropper = dropper.expect("Null foreign_drop_callback registered!");
  foreign_drop::register_foreign_drop_callback(dropper);
}

#[no_mangle]
pub extern "C" fn snocat_reactor_start(
  report_event_completion_callback: unsafe extern "C" fn(
    event_handle: u64,
    buffer: *const u8,
    buffer_len: u32,
  ) -> (),
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
      .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("quinn=warn,quinn_proto=warn,debug"));
    let collector = tracing_subscriber::fmt()
      .pretty()
      .with_env_filter(env_filter)
      // .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
      .finish();
    tracing::subscriber::set_global_default(collector).expect("Logger init must succeed");
    let report_event_completion_callback: eventing::ReportEventCompletionCb =
      report_event_completion_callback.into();
    tracing::info!("HELLO FROM TRACE");
    println!(
      "Starting reactor with event completion callback {:?}",
      &report_event_completion_callback,
    );
    let mut lock = REACTOR.lock().expect("Reactor Write Lock poisoned");
    match &mut *lock {
      Some(_) => return Err(anyhow::Error::msg("Reactor already started and active").into()),
      None => {
        *lock = Some(Arc::new(Reactor::start(Arc::new(
          report_event_completion_callback,
        ))?))
      }
    };
    Ok(())
  })
}

#[no_mangle]
pub extern "C" fn snocat_report_async_update(
  event_handle: u64,
  result: *const u8,
  result_length: u32,
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    let result = Vec::from(unsafe { slice::from_raw_parts(result, result_length as usize) });
    get_current_reactor()
      .fulfill_blocking(event_handle, result)
      .map_err(Into::into)
  })
}

struct ServerHandle(
  Arc<ConcurrentDeferredTunnelServer<Arc<dyn TunnelManager>>>,
  tokio::task::JoinHandle<()>,
);
impl Drop for ServerHandle {
  fn drop(&mut self) {
    // println!("Calling Server Handle destructor");
  }
}

lazy_static! {
  static ref SERVER_HANDLES: ConcurrentHandleMap<ServerHandle> = ConcurrentHandleMap::new();
}
unsafe impl IntoFfi for ServerHandle {
  type Value = u64;

  fn ffi_default() -> Self::Value {
    ffi_support::Handle::ffi_default()
  }

  fn into_ffi_value(self) -> Self::Value {
    SERVER_HANDLES.insert(self).into_u64()
  }
}
define_handle_map_deleter!(SERVER_HANDLES, snocat_free_server_handle);

#[no_mangle]
pub extern "C" fn snocat_server_start(
  config_json: FfiStr,
  min_port: u16,
  max_port: u16,
  authenticator_handle: u64,
  error: &mut ExternError,
) -> u64 {
  ::ffi_support::call_with_result::<ServerHandle, errors::FfiError, _>(error, || {
    let bind_port_range = std::ops::RangeInclusive::new(min_port, max_port);
    let authenticator = AUTHENTICATOR_HANDLES
      .remove_u64(authenticator_handle)
      .context("Authenticator handle invalid")?
      .context("Authenticator handle not present in map")?;
    use std::convert::TryInto;
    let config = serde_json::from_str::<dto::ServerConfig>(config_json.as_str())?;
    let quinn_config: quinn::ServerConfig = config.quinn_config.clone().try_into()?;
    let tunnel_manager = TcpTunnelManager::new(
      bind_port_range,
      IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
      Box::new(authenticator),
    );
    println!("Incoming config:\n{:#?}", &config);
    let reactor = get_current_reactor();
    let runtime = reactor.get_runtime_ref();
    let incoming = runtime.enter::<_, Result<_, anyhow::Error>>(|| {
      let (_endpoint, incoming) = {
        let mut endpoint = quinn::Endpoint::builder();
        endpoint.listen(quinn_config);
        endpoint
          .bind(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            9090,
          ))
          .context("Port binding failure")?
      };
      tracing::info!("Bound port {}", 9090);
      Ok(incoming)
    })?;
    let server: Arc<ConcurrentDeferredTunnelServer<Arc<dyn TunnelManager>>> = Arc::new(
      ConcurrentDeferredTunnelServer::new(Arc::new(tunnel_manager)),
    );
    let server_cloned = Arc::clone(&server);
    let server_task = runtime.spawn(async move {
      let server = server_cloned;

      // ConcurrentDeferredTunnelServer::handle_incoming(&*server_cloned, todo!(), todo!()).await;
      use futures::stream::StreamExt;
      use futures::stream::TryStreamExt;
      let (_trigger_shutdown, shutdown_notifier) = triggered::trigger();
      let connections: stream::BoxStream<'_, quinn::NewConnection> = incoming
        .take_until(shutdown_notifier.clone())
        .map(|x| -> anyhow::Result<_> { Ok(x) })
        .and_then(async move |connecting| {
          // When a new connection arrives, establish the connection formally, and pass it on
          let tunnel = connecting.await?; // Performs TLS handshake and migration
                                          // TODO: Protocol header can occur here, or as part of the later "binding" phase
                                          // It can also be built as an isomorphic middleware intercepting a TryStream of NewConnection
          Ok(tunnel)
        })
        .inspect_err(|e| {
          tracing::error!("Connection failure during stream pickup: {:#?}", e);
        })
        .filter_map(async move |x| x.ok()) // only keep the successful connections
        .boxed();

      let events = server
        .handle_incoming(connections, shutdown_notifier.clone())
        .fuse();

      events
        .for_each(async move |ev| {
          tracing::trace!(event = ?ev);
        })
        .await;
    });
    Ok(ServerHandle(server, server_task))
  })
}

#[no_mangle]
pub extern "C" fn snocat_server_stop(server_handle: u64, error: &mut ExternError) -> () {
  ::ffi_support::call_with_result::<(), errors::FfiError, _>(error, || {
    let _server = SERVER_HANDLES
      .remove_u64(server_handle)
      .context("Server handle invalid")?
      .context("Server handle not present in map")?;
    Ok(())
  })
}

// FFI-Remote Authentication
lazy_static! {
  static ref AUTHENTICATOR_HANDLES: ConcurrentHandleMap<FfiDelegatedAuthenticationHandler> =
    ConcurrentHandleMap::new();
}
define_handle_map_deleter!(AUTHENTICATOR_HANDLES, snocat_free_authenticator_handle);

DroppableCallback!(
  AuthenticationHandlerStartSessionCb,
  fn(session_handle: u64) -> ()
);

pub struct FfiDelegatedAuthenticationHandler {
  reactor: Arc<Reactor>,
  // We share this starter among callers, so we'll store it in an Arc
  auth_start_fn: Arc<AuthenticationHandlerStartSessionCb>,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Clone, Debug)]
pub struct AuthenticatorAcceptance {
  id: SnocatClientIdentifier,
}

impl From<proto::AuthenticatorAcceptance> for AuthenticatorAcceptance {
  fn from(item: proto::AuthenticatorAcceptance) -> Self {
    AuthenticatorAcceptance {
      id: SnocatClientIdentifier::new(item.connection_id),
    }
  }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Clone, Debug)]
pub struct AuthenticatorDenial {
  reason_code: u32,
  message: String,
}

impl From<proto::AuthenticatorDenial> for AuthenticatorDenial {
  fn from(item: proto::AuthenticatorDenial) -> Self {
    AuthenticatorDenial {
      reason_code: item.reason_code,
      message: item.reason,
    }
  }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Clone, Debug)]
#[serde(tag = "type")]
pub enum AuthenticatorSessionResult {
  Allow(AuthenticatorAcceptance),
  Deny(AuthenticatorDenial),
}

impl From<proto::AuthenticatorSessionResult> for AuthenticatorSessionResult {
  fn from(item: proto::AuthenticatorSessionResult) -> Self {
    use proto::authenticator_session_result::Result as Res;
    match item.result.expect("Must contain valid result") {
      Res::Acceptance(acceptance) => AuthenticatorSessionResult::Allow(acceptance.into()),
      Res::Denial(denial) => AuthenticatorSessionResult::Deny(denial.into()),
    }
  }
}

type AuthenticationSessionContext = (Box<dyn TunnelStream + Send + Unpin>, TunnelInfo);

#[no_mangle]
pub extern "C" fn snocat_authenticator_create(
  start_session_cb: Option<unsafe extern "C" fn(session_handle: u64) -> ()>,
  error: &mut ExternError,
) -> u64 {
  ::ffi_support::call_with_result::<Handle, errors::FfiError, _>(error, || {
    tracing::event!(
      tracing::Level::DEBUG,
      target = stringify!(snocat_authenticator_create),
      start_callback = ?start_session_cb,
    );
    let start_session_cb = start_session_cb
      .map(Into::into)
      .ok_or(anyhow::Error::msg("Session start callback was null"))?;
    let reactor = get_current_reactor();
    let handler = FfiDelegatedAuthenticationHandler::new(reactor, start_session_cb);
    Ok(AUTHENTICATOR_HANDLES.insert(handler))
  })
}

impl FfiDelegatedAuthenticationHandler {
  pub fn new(reactor: Arc<Reactor>, start_session: AuthenticationHandlerStartSessionCb) -> Self {
    Self {
      reactor,
      auth_start_fn: Arc::new(start_session),
    }
  }

  fn authenticate_ffi<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, anyhow::Result<SnocatClientIdentifier>> {
    let reactor = get_current_reactor();
    // Copy the extern-C-function off so we don't need to capture `self`
    let auth_start_fn = Arc::clone(&self.auth_start_fn);
    // Copy the address off for tracing before we move `tunnel_info` into the delegation context
    let peer_address = tunnel_info.remote_address;
    // Our context is an async-mutex in an arc; we use an option to allow us to "steal it" back after completion
    let session_context = Arc::new(tokio::sync::Mutex::new(Some((channel, tunnel_info))));

    async move {
      // delegate a new "session" to the FFI, storing the channel as context where we can access it
      let (res, ctx) = reactor.delegate_ffi_contextual::<
        proto::AuthenticatorSessionResult,
        Arc<tokio::sync::Mutex<Option<AuthenticationSessionContext>>>,
        _,
      >(
        move |session_delegation_handle| {
          tracing::event!(
            tracing::Level::DEBUG,
            target = "request_ffi_authentication",
            session_handle = ?session_delegation_handle,
            ?peer_address,
          );
          // Ffi call out to C api
          auth_start_fn.invoke(session_delegation_handle);
          tracing::event!(
            tracing::Level::DEBUG,
            target = "ffi_authentication_requested",
            session_handle = ?session_delegation_handle,
            ?peer_address,
          );
        },
        session_context
      ).await.context("FfiDelegation error in authentication request")??;

      // Clear the channel from the context, invalidating it for other holders
      // We don't bother checking if the context was still present in the mutex, because the
      // authentication process is free to choose to close the context however it wants, and
      // thus may have already taken it from the context for lifetime extension.
      let _ = std::mem::replace(&mut *ctx.lock().await, None);
      drop(ctx); // We're done with the channel anyway

      match res.into() {
        AuthenticatorSessionResult::Allow(acceptance) => Ok(acceptance.id),
        AuthenticatorSessionResult::Deny(denial) => {
          // TODO: "authentication failure" should be a success type rather than an error result
          Err(anyhow::Error::msg(denial.message))
        }
      }
    }
    .boxed()
  }

  #[no_mangle]
  pub extern "C" fn test_extern_in_impl() {}
}

impl std::fmt::Debug for FfiDelegatedAuthenticationHandler {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "({} with {} open requests)",
      std::any::type_name::<Self>(),
      self.reactor.get_delegations().len()
    )
  }
}

impl authentication::AuthenticationHandler for FfiDelegatedAuthenticationHandler {
  fn authenticate<'a>(
    &'a self,
    channel: Box<dyn TunnelStream + Send + Unpin>,
    tunnel_info: TunnelInfo,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, anyhow::Result<SnocatClientIdentifier>> {
    async move {
      let res = self
        .authenticate_ffi(channel, tunnel_info, shutdown_notifier)
        .await;
      res
    }
    .boxed()
  }
}

#[derive(PartialEq, Clone, Debug)]
pub struct SessionReadSuccess {
  pub buffer: Box<[u8]>,
}
impl Into<proto::authenticator_session_read_result::Success> for SessionReadSuccess {
  fn into(self) -> proto::authenticator_session_read_result::Success {
    proto::authenticator_session_read_result::Success {
      buffer: self.buffer.into_vec(),
    }
  }
}

impl Into<proto::AuthenticatorSessionReadResult> for Result<SessionReadSuccess, ()> {
  fn into(self) -> proto::AuthenticatorSessionReadResult {
    use proto::{
      authenticator_session_read_result as rres, AuthenticatorSessionIoError as IOError,
      AuthenticatorSessionReadResult as ReadResult,
    };
    match self {
      Ok(success) => ReadResult {
        result: Some(rres::Result::Success(success.into())),
      },
      Err(()) => ReadResult {
        result: Some(rres::Result::Error(proto::AuthenticatorSessionIoError {})),
      },
    }
  }
}

#[derive(PartialEq, Clone, Debug)]
pub struct SessionWriteSuccess {}
impl Into<proto::AuthenticatorSessionWriteResult> for Result<SessionWriteSuccess, ()> {
  fn into(self) -> proto::AuthenticatorSessionWriteResult {
    use proto::{
      authenticator_session_write_result as wres, AuthenticatorSessionIoError as IOError,
      AuthenticatorSessionWriteResult as WriteResult,
    };
    match self {
      Ok(_success) => WriteResult {
        result: Some(wres::Result::Success(wres::Success {})),
      },
      Err(()) => WriteResult {
        result: Some(wres::Result::Error(proto::AuthenticatorSessionIoError {})),
      },
    }
  }
}

#[no_mangle]
pub extern "C" fn snocat_authenticator_session_read_channel(
  session_handle: u64,
  result_event_handle: EventHandle<proto::AuthenticatorSessionReadResult>,
  len: u32,
  // TODO: Implement timeout support
  _timeout_milliseconds: u32,
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    let reactor_ref = get_current_reactor();
    let events = reactor_ref.get_events_ref();
    events.fire_evented_handle(result_event_handle, async move {
      let res = reactor_ref
        .get_delegations()
        .with_context_optarx::<AuthenticationSessionContext, Result<Vec<u8>, ()>, _, _>(
          session_handle,
          async move |mut ctx| {
            let mut buf = vec![0u8; len as usize];
            use tokio::io::AsyncReadExt;
            let read_res = AsyncReadExt::read_exact(&mut ctx.as_mut().0, &mut buf)
              .await
              .context("Read failure");
            match read_res {
              Ok(_) => Ok(buf),
              Err(_) => Err(()),
            }
          },
        )
        .await
        .map_err(|_| ());

      let res = match res {
        Ok(Ok(x)) => Ok(x),
        _ => Err(()),
      };
      // Allocate a byte buffer and release ownership of it to the FFI, if res is Ok
      res
        .map(|buffer: Vec<u8>| SessionReadSuccess {
          // ByteBuffers do not clean up their data contents when dropped
          // From this point on, the memory is owned by- and must be destroyed by- the remote side
          buffer: buffer.into_boxed_slice(),
        })
        .into()
    })?;
    Ok(())
  })
}

#[no_mangle]
pub extern "C" fn snocat_authenticator_session_write_channel(
  session_handle: u64,
  result_event_handle: EventHandle<proto::AuthenticatorSessionWriteResult>,
  buffer: *const u8,
  len: u32,
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    // Make a copy of the memory before we go asynchronous, so the remote can clean up after itself
    let buffer = unsafe { std::slice::from_raw_parts(buffer, len as usize) }.to_vec();
    let reactor_ref = get_current_reactor();
    let events = reactor_ref.get_events_ref();
    events.fire_evented_handle(result_event_handle, async move {
      let res = reactor_ref
        .get_delegations()
        .with_context_optarx::<AuthenticationSessionContext, Result<(), ()>, _, _>(
          session_handle,
          async move |mut ctx| {
            use tokio::io::AsyncWriteExt;
            let write_res = AsyncWriteExt::write_all(&mut ctx.as_mut().0, &buffer)
              .await
              .context("Write failure");
            match write_res {
              Ok(_) => Ok(()),
              Err(_) => Err(()),
            }
          },
        )
        .await
        .map_err(|_| ());

      // Collapse remote and local errors into unit errors
      let res = match res {
        Ok(Ok(x)) => Ok(x),
        _ => Err(()),
      };
      // Allocate a byte buffer and release ownership of it to the FFI, if res is Ok
      res.map(|_| SessionWriteSuccess {}).into()
    })?;
    Ok(())
  })
}

#[cfg(test)]
lazy_static! {
  static ref REACTOR_TEST_EVENTS: std::sync::Mutex<std::collections::VecDeque<(u64, eventing::proto::EventResult)>> =
    std::sync::Mutex::new(Default::default());
}

#[cfg(test)]
mod tests {
  use super::{eventing, proto};
  use crate::ffi::{
    eventing::EventCompletionState, snocat_reactor_start, snocat_report_async_update,
    CompletionState, REACTOR, REACTOR_TEST_EVENTS,
  };
  use crate::util::messenger::*;
  use ffi_support::FfiStr;
  use std::convert::TryInto;
  use std::{
    collections::VecDeque,
    slice,
    sync::{Arc, Mutex},
  };

  use super::{
    delegation::{self, RemoteError},
    foreign_drop::{self, register_foreign_drop_callback, ForeignDropCallback},
  };

  unsafe extern "C" fn fake_event_report_cb(
    event_handle: u64,
    buffer: *const u8,
    buffer_len: u32,
  ) -> () {
    use ::prost::Message;
    let event = eventing::proto::EventResult::decode_length_delimited(std::slice::from_raw_parts(
      buffer,
      buffer_len as usize,
    ))
    .expect("Events pushed to callback must be valid");
    VecDeque::push_back(
      &mut *REACTOR_TEST_EVENTS.lock().unwrap(),
      (event_handle, event),
    );
  }

  unsafe extern "C" fn fake_drop_callback(item: *const ()) -> () {
    eprintln!("(faking drop of {:?})", item);
  }

  fn get_test_reactor() -> Arc<super::Reactor> {
    register_foreign_drop_callback(fake_drop_callback as ForeignDropCallback);
    let mut lock = REACTOR.lock().expect("Reactor Write Lock poisoned");
    let fake_event_report_cb = (fake_event_report_cb
      as <super::eventing::ReportEventCompletionCb as foreign_drop::ForeignDrop>::Raw)
      .into();
    // We still register with the global state because otherwise our binding functions can't refer to the reactor
    match &mut *lock {
      Some(r) => Arc::clone(r),
      None => {
        *lock = Some(Arc::new(
          super::Reactor::start(Arc::new(fake_event_report_cb)).unwrap(),
        ));
        Arc::clone(lock.as_ref().unwrap())
      }
    }
  }

  #[test]
  fn drop_resistant_reactor() {
    register_foreign_drop_callback(fake_drop_callback as ForeignDropCallback);
    let fake_event_report_cb = (fake_event_report_cb
      as <super::eventing::ReportEventCompletionCb as foreign_drop::ForeignDrop>::Raw)
      .into();
    let reactor = super::Reactor::start(Arc::new(fake_event_report_cb)).unwrap();
    drop(reactor);
  }

  #[tokio::test]
  async fn test_ffi_delegation() {
    let reactor = get_test_reactor();
    let res: Result<Result<String, _>, _> = {
      reactor
        .delegate_ffi(move |id| {
          let mut reporting_error = Default::default();

          use prost::Message;
          let buffer = (delegation::proto::DelegateResult {
            result: Some(delegation::proto::delegate_result::Result::Completed(
              delegation::proto::delegate_result::Completion {
                value: Some(
                  AnyProto::new("string", String::from("\"hello world\""))
                    .try_into()
                    .unwrap(),
                ),
              },
            )),
          })
          .encode_length_delimited_vec()
          .expect("Encoding must succeed");

          let buffer = buffer.into_boxed_slice();
          snocat_report_async_update(
            id,
            buffer.as_ptr(),
            buffer.len() as u32,
            &mut reporting_error,
          );

          assert!(reporting_error.get_code().is_success());
          unsafe { reporting_error.manually_release() };
        })
        .await
    };
    let res = res.unwrap().unwrap();
    println!("FFI returned result: {:#?}", res);
    assert_eq!(&res, "\"hello world\"");
  }

  #[tokio::test]
  async fn test_ffi_delegation_context() {
    let reactor = get_test_reactor();
    let reactor_arc_clone = Arc::clone(&reactor);
    let res: Result<Result<(String, _), _>, _> = {
      reactor
        .delegate_ffi_contextual::<String, Arc<String>, _>(
          move |id| {
            let ctxres = reactor_arc_clone
              .runtime_handle()
              .block_on(
                reactor_arc_clone
                  .get_delegations()
                  .read_context::<Arc<String>, _, _>(id, |x| String::from(x.as_ref())),
              )
              .unwrap();

            assert_eq!(ctxres, String::from("Test Context"));

            use prost::Message;
            let mut buffered_string = Vec::<u8>::new();
            String::from("\"hello world\"")
              .encode_length_delimited(&mut buffered_string)
              .unwrap();
            let buffer = (delegation::proto::DelegateResult {
              result: Some(delegation::proto::delegate_result::Result::Completed(
                delegation::proto::delegate_result::Completion {
                  value: Some(
                    AnyProto::new("string", String::from("\"hello world\""))
                      .try_into()
                      .unwrap(),
                  ),
                },
              )),
            })
            .encode_length_delimited_vec()
            .expect("Encoding must succeed");

            let mut reporting_error = Default::default();
            let buffer = buffer.into_boxed_slice();
            snocat_report_async_update(
              id,
              buffer.as_ptr(),
              buffer.len() as u32,
              &mut reporting_error,
            );

            assert!(reporting_error.get_code().is_success());
            unsafe { reporting_error.manually_release() };
          },
          Arc::new(String::from("Test Context")),
        )
        .await
    };
    let res = res.unwrap().unwrap();

    println!("FFI returned result: {:#?}", res);
    assert_eq!(&res.0, "\"hello world\"");
  }
}
