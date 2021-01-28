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
use futures::future::{BoxFuture, Either, Future, FutureExt};
use futures::{AsyncReadExt, AsyncWriteExt};
use futures_io::AsyncBufRead;
use lazy_static::lazy_static;
use std::any::Any;
use std::marker::PhantomData;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::oneshot::error::RecvError;
use tokio::sync::{oneshot, Mutex};

pub mod dto;
pub mod errors;

mod allocators;
pub mod foreign_drop;
use foreign_drop::ForeignDropCallback;
pub mod delegation;
use delegation::{
  CompletionState, Delegation, DelegationError, DelegationResult, DelegationSet,
  RemoteError as FfiRemoteError,
};

pub mod eventing;
use crate::ffi::allocators::{get_bytebuffer_raw, RawByteBuffer};
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
    state: EventCompletionState,
    json_loc: *const u8,
    json_byte_len: u32,
  ) -> (),
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    let report_event_completion_callback: eventing::ReportEventCompletionCb =
      report_event_completion_callback.into();
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
  state: CompletionState,
  json: FfiStr,
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    let json_str = json.into_string();
    get_current_reactor()
      .fulfill_blocking(event_handle, state, json_str)
      .map_err(Into::into)
  })
}

#[repr(transparent)]
struct ServerHandle(Box<ConcurrentDeferredTunnelServer<Arc<dyn TunnelManager>>>);
impl Drop for ServerHandle {
  fn drop(&mut self) {
    println!("Calling Server Handle destructor");
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
    let config: quinn::ServerConfig = config.quinn_config.try_into()?;
    println!("Incoming config:\n{:#?}", config);
    let server: ConcurrentDeferredTunnelServer<Arc<dyn TunnelManager>> =
      ConcurrentDeferredTunnelServer::new(Arc::new(TcpTunnelManager::new(
        bind_port_range,
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        Box::new(authenticator),
      )));
    Ok(ServerHandle(Box::new(server)))
  })
}

#[no_mangle]
pub extern "C" fn snocat_server_stop(server_handle: u64, error: &mut ExternError) -> () {
  ::ffi_support::call_with_result::<(), errors::FfiError, _>(error, || {
    let _ = server_handle;
    todo!()
  })
}

// FFI-Remote Authentication
lazy_static! {
  static ref AUTHENTICATOR_HANDLES: ConcurrentHandleMap<FfiDelegatedAuthenticationHandler> =
    ConcurrentHandleMap::new();
}

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

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Clone, Debug)]
pub struct AuthenticatorDenial {
  reason_code: u32,
  message: String,
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
      // Delegate a new "session" to the FFI, storing the channel as context where we can access it
      let (res, ctx) = reactor.delegate_ffi_contextual::<
        AuthenticatorAcceptance,
        AuthenticatorDenial,
        Arc<tokio::sync::Mutex<Option<AuthenticationSessionContext>>>,
        _,
      >(
        move |session_id| {
          tracing::event!(
            tracing::Level::DEBUG,
            target = "request_ffi_authentication",
            session_handle = ?session_id,
            ?peer_address,
          );
          // Ffi call out to C api
          auth_start_fn.invoke(session_id);
          tracing::event!(
            tracing::Level::DEBUG,
            target = "ffi_authentication_requested",
            session_handle = ?session_id,
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

      match res {
        Ok(acceptance) => Ok(acceptance.id),
        Err(denial) => {
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

#[no_mangle]
pub extern "C" fn snocat_authenticator_session_complete(
  session_handle: u64,
  // ID to use for event completion notifications
  completion_event_id: EventHandle<(), ()>,
  completion_state: CompletionState,
  parameter_json: FfiStr,
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    let reactor_ref = get_current_reactor();
    let parameter_json = parameter_json.into_string();
    let events = reactor_ref.get_events_ref();
    events.fire_evented_handle(completion_event_id, async move {
      reactor_ref
        .fulfill(session_handle, completion_state, parameter_json)
        .await
        .map_err(|_| ())
    })?;

    Ok(())
  })
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Clone, Debug)]
pub struct SessionReadSuccess {
  pub buffer: RawByteBuffer,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Clone, Debug)]
pub struct SessionWriteSuccess {}

#[no_mangle]
pub extern "C" fn snocat_authenticator_session_read_channel(
  session_handle: u64,
  result_event_handle: EventHandle<SessionReadSuccess, ()>,
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
      res.map(|buffer| SessionReadSuccess {
        // ByteBuffers do not clean up their data contents when dropped
        // From this point on, the memory is owned by- and must be destroyed by- the remote side
        buffer: get_bytebuffer_raw(ByteBuffer::from_vec(buffer)),
      })
    })?;
    Ok(())
  })
}

#[no_mangle]
pub extern "C" fn snocat_authenticator_session_write_channel(
  session_handle: u64,
  result_event_handle: EventHandle<SessionWriteSuccess, ()>,
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
      res.map(|_| SessionWriteSuccess {})
    })?;
    Ok(())
  })
}

#[cfg(test)]
lazy_static! {
  static ref REACTOR_TEST_EVENTS: std::sync::Mutex<std::collections::VecDeque<(u64, EventCompletionState, String)>> =
    std::sync::Mutex::new(Default::default());
}

#[cfg(test)]
mod tests {
  use crate::ffi::{
    eventing::EventCompletionState, snocat_reactor_start, snocat_report_async_update,
    CompletionState, REACTOR, REACTOR_TEST_EVENTS,
  };
  use ffi_support::FfiStr;
  use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
  };

  use super::{
    delegation::RemoteError,
    foreign_drop::{self, register_foreign_drop_callback, ForeignDropCallback},
  };

  unsafe extern "C" fn fake_event_report_cb(
    event_handle: u64,
    state: EventCompletionState,
    json_loc: *const u8,
    _json_byte_len: u32,
  ) -> () {
    let json = FfiStr::from_raw(json_loc as *const _).into_string();
    VecDeque::push_back(
      &mut *REACTOR_TEST_EVENTS.lock().unwrap(),
      (event_handle, state, json),
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
    let res: Result<Result<Result<String, ()>, _>, _> = {
      reactor
        .delegate_ffi(move |id| {
          let mut reporting_error = Default::default();
          let json_str = std::ffi::CString::new("\"hello world\"").unwrap();
          snocat_report_async_update(
            id,
            CompletionState::Complete,
            unsafe { FfiStr::from_raw(json_str.as_ptr()) },
            &mut reporting_error,
          );

          assert!(reporting_error.get_code().is_success());
          unsafe { reporting_error.manually_release() };
        })
        .await
    };
    println!("FFI returned result: {:#?}", res);
  }

  #[tokio::test]
  async fn test_ffi_delegation_context() {
    let reactor = get_test_reactor();
    let reactor_arc_clone = Arc::clone(&reactor);
    let res: Result<Result<(Result<String, ()>, _), _>, _> = {
      reactor
        .delegate_ffi_contextual::<String, (), Arc<String>, _>(
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

            let mut reporting_error = Default::default();
            let json_str = std::ffi::CString::new("\"hello world\"").unwrap();
            snocat_report_async_update(
              id,
              CompletionState::Complete,
              unsafe { FfiStr::from_raw(json_str.as_ptr()) },
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
  }
}
