//! Bindings for instantiation and control via C ABI

use crate::common::authentication::TunnelInfo;
use crate::ffi::errors::FfiError;
use crate::server::deferred::SnocatClientIdentifier;
use crate::util::delegation::DelegationError;
use crate::util::tunnel_stream::TunnelStream;
use crate::{
  common::authentication::{self},
  server::{
    self,
    deferred::{ConcurrentDeferredTunnelServer, TunnelManager},
    PortRangeAllocator, TcpTunnelManager,
  },
  util::{
    self,
    delegation::{self, DelegationPool},
    vtdroppable::VTDroppable,
  },
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
pub mod ffi_delegation;
use ffi_delegation::{
  FfiDelegation, FfiDelegationContext, FfiDelegationContextInner, FfiDelegationError,
  FfiDelegationResult, FfiDelegationSet,
};

mod eventing;
use crate::ffi::allocators::{get_bytebuffer_raw, RawByteBuffer};
use eventing::{EventCompletionState, EventHandle, EventRunner, EventingError};

pub struct Reactor {
  rt: Arc<tokio::runtime::Runtime>,
  /// Delegations wrap remote tasks (completion/failure) and map them to futures
  /// Each delegation is identified by handle and contains either a sender or a mapped-send-closure
  /// The `delegations` mapping has items added when being delegated to the remote, and removed
  /// when being fulfilled by the remote calling `snocat_report_async_update`
  delegations: Arc<FfiDelegationSet>,
  events: Arc<EventRunner>,
}
impl Reactor {
  pub fn start(
    report_task_completion_callback: extern "C" fn(
      handle: u64,
      state: EventCompletionState,
      json_loc: *const u8,
      json_byte_len: u32,
    ) -> (),
  ) -> Result<Self, anyhow::Error> {
    let rt = tokio::runtime::Builder::new()
      .threaded_scheduler()
      .thread_name("tokio-reactor-worker")
      .enable_all()
      .build()?;
    Ok(Reactor {
      delegations: Arc::new(FfiDelegationSet::new()),
      events: Arc::new(EventRunner::new(
        rt.handle().clone(),
        report_task_completion_callback,
      )),
      rt: Arc::new(rt),
    })
  }

  pub async fn delegate_ffi<
    T: serde::de::DeserializeOwned + Send + 'static,
    E: serde::de::DeserializeOwned + Send + 'static,
    Dispatch: (FnOnce(u64) -> ()) + Send + 'static,
  >(
    &self,
    dispatch_ffi: Dispatch,
  ) -> Result<Result<T, E>, FfiDelegationError> {
    self
      .delegations
      .delegate_ffi_simple(dispatch_ffi)
      .boxed()
      .await
  }

  pub fn delegate_ffi_contextual<
    'a,
    'b: 'a,
    T: serde::de::DeserializeOwned + Send + 'static,
    E: serde::de::DeserializeOwned + Send + 'static,
    C: Any + Send + 'static,
    Dispatch: (FnOnce(u64) -> ()) + Send + 'static,
  >(
    &'b self,
    dispatch_ffi: Dispatch,
    context: C,
  ) -> BoxFuture<'a, Result<(Result<T, E>, C), FfiDelegationError>> {
    self
      .delegations
      .delegate_ffi_contextual(dispatch_ffi, context)
      .boxed()
  }
}

impl Drop for Reactor {
  // Note that this runs on the thread closing the reactor, within the remote thread's context
  fn drop(&mut self) -> () {
    use ::std::ops::DerefMut;
    let delegation_count = self.delegations.len();
    tracing::event!(
      tracing::Level::INFO,
      delegation_count,
      "Shutting down reactor with {} outstanding delegations",
      delegation_count
    );
    // Simply dropping the Runtime instance is enough to trigger graceful shutdown
    // Dropping the delegation handle map will drop the oneshots, firing errors on open FFI oneshots
  }
}

#[repr(C)]
pub enum CompletionState {
  Complete = 0,
  Failed = 1,
}

lazy_static! {
  static ref REACTOR: std::sync::Mutex<Option<Arc<Reactor>>> = std::sync::Mutex::new(None);
}

fn get_current_reactor() -> Arc<Reactor> {
  let reactor_ref = REACTOR.lock().expect("Reactor Read Lock poisoned");
  Arc::clone(reactor_ref.as_ref().expect("Reactor must be initialized"))
}

#[no_mangle]
pub extern "C" fn snocat_reactor_start(
  report_event_completion_callback: extern "C" fn(
    event_handle: u64,
    state: EventCompletionState,
    json_loc: *const u8,
    json_byte_len: u32,
  ) -> (),
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    println!(
      "Starting reactor with callback {:?}",
      &report_event_completion_callback as *const _
    );
    let mut lock = REACTOR.lock().expect("Reactor Write Lock poisoned");
    match &mut *lock {
      Some(_) => return Err(anyhow::Error::msg("Reactor already started and active").into()),
      None => *lock = Some(Arc::new(Reactor::start(report_event_completion_callback)?)),
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
      .delegations
      .fulfill_blocking(event_handle, state, json_str)
      .map_err(Into::into)
  })
}

struct ServerHandle<T: TunnelManager>(Option<Box<ConcurrentDeferredTunnelServer<T>>>, u32);
impl<T: TunnelManager> Drop for ServerHandle<T> {
  fn drop(&mut self) {
    println!(
      "Calling Server Handle destructor for handle sub-id \"{}\"",
      &self.1
    );
  }
}

lazy_static! {
  static ref SERVER_HANDLES: ConcurrentHandleMap<ServerHandle<Box<dyn TunnelManager>>> =
    ConcurrentHandleMap::new();
}
unsafe impl IntoFfi for ServerHandle<Box<dyn TunnelManager>> {
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
pub extern "C" fn snocat_server_start(config_json: FfiStr, error: &mut ExternError) -> u64 {
  ::ffi_support::call_with_result::<ServerHandle<_>, errors::FfiError, _>(error, || {
    use std::convert::TryInto;
    println!("Incoming config:\n{}", config_json.as_str());
    let config = serde_json::from_str::<dto::ServerConfig>(config_json.as_str())?;
    let config: quinn::ServerConfig = config.quinn_config.try_into()?;
    println!("HELLO WORLD FROM C API with cfg:\n{:#?}", config);
    // let server = ConcurrentDeferredTunnelServer::new(TcpTunnelManager::new(
    //   Range::new(8000, 8010),
    //   IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
    //   todo!()
    // ));
    Ok(ServerHandle(None, 0))
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
  // TODO: Remove once FfiDelegation is in use for auth and channels can be read
  static ref AUTHENTICATOR_SESSION_HANDLES: ConcurrentHandleMap<FfiAuthenticationState> =
    ConcurrentHandleMap::new();
}

struct FfiAuthenticationState {
  peer_address: SocketAddr,
  channel: Box<dyn TunnelStream + Send + Unpin>,
  closer:
    tokio::sync::Mutex<Option<oneshot::Sender<Result<SnocatClientIdentifier, anyhow::Error>>>>,
}

pub struct FfiDelegatedAuthenticationHandler {
  reactor: Arc<Reactor>,
  delegation_pool: Arc<Mutex<DelegationPool>>,
  auth_start_fn: extern "C" fn(session_handle: u64) -> (),
}

#[no_mangle]
pub extern "C" fn snocat_bind_authenticator(error: &mut ExternError) -> u64 {
  ::ffi_support::call_with_result::<ServerHandle<_>, errors::FfiError, _>(error, || todo!())
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

impl FfiDelegatedAuthenticationHandler {
  pub fn new(
    reactor: Arc<Reactor>,
    start_session: extern "C" fn(session_handle: u64) -> (),
  ) -> Self {
    Self {
      reactor,
      delegation_pool: Arc::new(tokio::sync::Mutex::new(DelegationPool::new())),
      auth_start_fn: start_session,
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
    let auth_start_fn = self.auth_start_fn;
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
          auth_start_fn(session_id);
          tracing::event!(
            tracing::Level::DEBUG,
            target = "ffi_authentication_requested",
            session_handle = ?session_id,
            ?peer_address,
          );
        },
        session_context
      ).await.context("FfiDelegation error in authentication request")?;

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
}

#[no_mangle]
pub extern "C" fn snocat_authenticator_session_complete(
  session_handle: u64,
  // ID to use for event completion notifications
  completion_event_id: EventHandle<(), ()>,
  accept: bool,
  parameter_json: FfiStr,
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    let reactor_ref = get_current_reactor();
    let parameter_json = parameter_json.into_string();
    let events = Arc::clone(&reactor_ref.events);
    events.fire_evented_handle(completion_event_id, async move {
      reactor_ref
        .delegations
        .fulfill(
          session_handle,
          if accept {
            CompletionState::Complete
          } else {
            CompletionState::Failed
          },
          parameter_json,
        )
        .await
        .map_err(|_| ())
    })?;

    Ok(())
  })
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Clone, Debug)]
pub struct SessionReadSuccess {
  buffer: RawByteBuffer,
}

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
    let events = Arc::clone(&reactor_ref.events);
    events.fire_evented_handle(result_event_handle, async move {
      let res = reactor_ref
        .delegations
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
  _session_handle: u64,
  _len: u32,
  _buffer: *const u8,
  _error: &mut ExternError,
) -> u64 {
  todo!("Implement channel-based writing")
}

impl std::fmt::Debug for FfiDelegatedAuthenticationHandler {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "({} with {} open requests)",
      std::any::type_name::<Self>(),
      self.reactor.delegations.len()
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

  extern "C" fn fake_event_report_cb(
    event_handle: u64,
    state: EventCompletionState,
    json_loc: *const u8,
    _json_byte_len: u32,
  ) -> () {
    let json = unsafe { FfiStr::from_raw(json_loc as *const _) }.into_string();
    VecDeque::push_back(
      &mut *REACTOR_TEST_EVENTS.lock().unwrap(),
      (event_handle, state, json),
    );
  }

  fn get_test_reactor() -> Arc<super::Reactor> {
    if REACTOR.lock().unwrap().is_none() {
      let mut start_err = Default::default();
      snocat_reactor_start(fake_event_report_cb, &mut start_err);
      assert!(start_err.get_code().is_success());
      unsafe { start_err.manually_release() };
    }
    let reactor = REACTOR.lock().unwrap();
    let reactor = reactor.as_ref().expect("Test reactor must be initialized");
    Arc::clone(&reactor)
  }

  #[test]
  fn test_reactor_spinup() {
    get_test_reactor();
  }

  #[tokio::test]
  async fn test_ffi_delegation() {
    let reactor = get_test_reactor();
    let res: Result<Result<String, ()>, _> = {
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
    let res: Result<(Result<String, ()>, _), _> = {
      reactor
        .delegate_ffi_contextual::<String, (), Arc<String>, _>(
          move |id| {
            let ctxres = reactor_arc_clone
              .rt
              .handle()
              .block_on(
                reactor_arc_clone
                  .delegations
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

    println!("FFI returned result: {:#?}", res);
  }
}
