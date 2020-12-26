//! Bindings for instantiation and control via C ABI

use crate::server::deferred::SnocatClientIdentifier;
use crate::{
  common::authentication::{self, DelegatedAuthenticationHandler},
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
  FfiStr, HandleError, IntoFfi,
};
use futures::future::{BoxFuture, Future, FutureExt};
use futures::AsyncWriteExt;
use futures_io::AsyncBufRead;
use lazy_static::lazy_static;
use pin_project::pin_project;
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

mod allocators {
  use ffi_support::{rust_string_to_c, FfiStr};
  const ALLOCATION_TRACING: bool = true;

  // define_bytebuffer_destructor!(snocat_free_buffer);
  // define_string_destructor!(snocat_free_string);
  // console-tracing versions of data destructors and allocators for development
  #[no_mangle]
  pub extern "C" fn snocat_free_buffer(v: ::ffi_support::ByteBuffer) {
    ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
      if ALLOCATION_TRACING {
        println!(" - byte[{:?}]", v.as_slice().len());
      }
      v.destroy()
    })
  }

  #[no_mangle]
  pub unsafe extern "C" fn snocat_free_string(s: *mut std::os::raw::c_char) {
    ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
      if ALLOCATION_TRACING {
        println!(
          " - STR {:?} : {:?}",
          &s,
          if s.is_null() {
            ""
          } else {
            let sffi = FfiStr::from_raw(s);
            sffi.as_str()
          }
        );
      }
      if !s.is_null() {
        ::ffi_support::destroy_c_string(s)
      }
    });
  }
  // Allow remote allocation of data

  /// Allocate a buffer of the requested size, and return an object with a pointer to it; (size, data)
  #[no_mangle]
  pub extern "C" fn snocat_alloc_buffer(byte_length: u32) -> ::ffi_support::ByteBuffer {
    ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
      let buf = ::ffi_support::ByteBuffer::new_with_size(byte_length as usize);
      if ALLOCATION_TRACING {
        println!(" + byte[{:?}]", byte_length);
      }
      buf
    })
  }

  /// Allocate a buffer of the provided data, and return an object with a pointer to it; (size, data)
  #[no_mangle]
  pub extern "C" fn snocat_alloc_buffer_from(
    start: *const u8,
    byte_length: u32,
  ) -> ::ffi_support::ByteBuffer {
    ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
      let mut buf = ::ffi_support::ByteBuffer::new_with_size(byte_length as usize);
      if byte_length > 0 {
        let source = unsafe { &*std::ptr::slice_from_raw_parts::<u8>(start, byte_length as usize) };
        buf.as_mut_slice().copy_from_slice(source);
      }
      if ALLOCATION_TRACING {
        println!(" + byte[{:?}]", byte_length);
      }
      buf
    })
  }

  #[no_mangle]
  pub unsafe extern "C" fn snocat_alloc_string(s: FfiStr) {
    ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
      let rs: *mut std::os::raw::c_char = rust_string_to_c(s.as_str());
      if ALLOCATION_TRACING {
        println!(" + STR {:?} : {:?}", &rs, s.as_str());
      }
      rs
    });
  }
}

pub struct FfiDelegation {
  fulfill: oneshot::Sender<Result<String, String>>,
}

pub struct Reactor {
  rt: tokio::runtime::Runtime,
  delegations: ConcurrentHandleMap<FfiDelegation>,
  report_task_completion_callback:
    extern "C" fn(handle: u64, state: CompletionState, json_loc: *mut u8, json_byte_len: u32) -> (),
}
impl Reactor {
  pub fn start(
    report_task_completion_callback: extern "C" fn(
      handle: u64,
      state: CompletionState,
      json_loc: *mut u8,
      json_byte_len: u32,
    ) -> (),
  ) -> Result<Self, anyhow::Error> {
    let rt = tokio::runtime::Builder::new()
      .threaded_scheduler()
      .thread_name("tokio-reactor-worker")
      .build()?;
    Ok(Reactor {
      rt,
      delegations: ConcurrentHandleMap::new(),
      report_task_completion_callback,
    })
  }

  // TODO: ... one that fulfills a request made remotely and one that fulfills a request made by Rust
  // TODO: This handles dispatch to the appropriate method by linker instead of by manual json-dispatch
  pub fn delegate_result_from_ffi<T: serde::de::DeserializeOwned>(
    &self,
  ) -> BoxFuture<Result<T, anyhow::Error>> {
    todo!("What should this even handle?")
  }

  // pub fn delegate_to_ffi<T: serde::ser::Serialize>(&self
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
  static ref REACTOR: std::sync::RwLock<Option<Reactor>> = std::sync::RwLock::new(None);
}

#[no_mangle]
pub extern "C" fn snocat_reactor_start(
  report_task_completion_callback: extern "C" fn(
    handle: u64,
    state: CompletionState,
    json_loc: *mut u8,
    json_byte_len: u32,
  ) -> (),
  error: &mut ExternError,
) -> () {
  ::ffi_support::call_with_result::<_, errors::FfiError, _>(error, || {
    println!(
      "Starting reactor with callback {:?}",
      &report_task_completion_callback as *const _
    );
    let mut lock = REACTOR.write().expect("Reactor Write Lock poisoned");
    match &mut *lock {
      Some(_) => return Err(anyhow::Error::msg("Reactor already started and active").into()),
      None => *lock = Some(Reactor::start(report_task_completion_callback)?),
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
    let reactor_ref = REACTOR.read().expect("Reactor Read Lock poisoned");
    let reactor_ref = reactor_ref.as_ref().expect("Reactor must be initialized");
    let delegation = reactor_ref.delegations.remove_u64(event_handle)?;
    if let Some(delegation) = delegation {
      let json_str = json.into_string();
      if let Err(_) = delegation.fulfill.send(match state {
        CompletionState::Complete => Ok(json_str),
        CompletionState::Failed => Err(json_str),
      }) {
        return Err(anyhow::Error::msg("Delegation handle was already consumed?").into());
      }
      Ok(())
    } else {
      Err(anyhow::Error::msg("Delegation handle missing?").into())
    }
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
  ffi_support::call_with_result::<ServerHandle<_>, errors::FfiError, _>(error, || {
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
  ffi_support::call_with_result::<(), errors::FfiError, _>(error, || {
    let _ = server_handle;
    todo!()
  })
}

// FFI-Remote Authentication
lazy_static! {
  static ref AUTHENTICATOR_SESSION_HANDLES: ConcurrentHandleMap<Arc<tokio::sync::Mutex<FfiAuthenticationState>>> =
    ConcurrentHandleMap::new();
}

struct FfiAuthenticationState {
  peer_address: SocketAddr,
  channel: Arc<tokio::sync::Mutex<(quinn::SendStream, quinn::RecvStream)>>,
  closer: oneshot::Sender<Result<SnocatClientIdentifier, anyhow::Error>>,
}

pub struct FfiDelegatedAuthenticationHandler {
  reactor: Arc<Reactor>,
  delegation_pool: Arc<Mutex<DelegationPool>>,
}

impl FfiDelegatedAuthenticationHandler {
  pub fn new(reactor: Arc<Reactor>) -> Self {
    Self {
      reactor,
      delegation_pool: Arc::new(tokio::sync::Mutex::new(DelegationPool::new())),
    }
  }

  fn authenticate_arc_channel<'a>(
    &'a self,
    channel: Arc<tokio::sync::Mutex<(quinn::SendStream, quinn::RecvStream)>>,
    tunnel: &'a quinn::NewConnection,
    _shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, anyhow::Result<SnocatClientIdentifier>> {
    async move {
      let peer_addr = tunnel.connection.remote_address();

      let delegation_recv: Result<Result<SnocatClientIdentifier, _>, _> = self
        .delegation_pool
        .lock()
        .await
        .delegate(async move |dispatcher| {
          self
            .request_ffi_authentication(peer_addr, channel, dispatcher)
            .await
        })
        .await
        .await;
      match delegation_recv {
        Err(delegation_error) => Err(delegation_error).context("Task Delegation error"),
        Ok(Err(authentication_error)) => Err(authentication_error).context("Authentication denied"),
        Ok(Ok(id)) => Ok(id),
      }
    }
    .boxed()
  }

  async fn request_ffi_authentication(
    &self,
    peer_address: SocketAddr,
    auth_channel: Arc<tokio::sync::Mutex<(quinn::SendStream, quinn::RecvStream)>>,
    dispatcher: oneshot::Sender<Result<SnocatClientIdentifier, anyhow::Error>>,
  ) {
    let auth_state = FfiAuthenticationState {
      peer_address,
      channel: auth_channel,
      closer: dispatcher,
    };
    let state_handle =
      AUTHENTICATOR_SESSION_HANDLES.insert(Arc::new(tokio::sync::Mutex::new(auth_state)));
    todo!("{:?}", state_handle)
  }
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
    tunnel: &'a mut quinn::NewConnection,
    shutdown_notifier: &'a triggered::Listener,
  ) -> BoxFuture<'a, anyhow::Result<SnocatClientIdentifier>> {
    async move {
      let auth_channel = tunnel.connection.open_bi().await?;
      let auth_channel = Arc::new(tokio::sync::Mutex::new(auth_channel));
      let res = self
        .authenticate_arc_channel(auth_channel.clone(), &tunnel, shutdown_notifier)
        .await;
      let mut auth_channel = auth_channel.lock().await;
      let closed = auth_channel
        .0
        .close()
        .await
        .context("Failure closing authentication channel");
      match (res, closed) {
        (Ok(id), Err(e)) => {
          tracing::warn!(
            "Failure in closing of client authentication channel {:?}",
            e
          );
          Ok(id) // Failure here means the channel was already closed by the other party
        }
        (Err(e), _) => Err(e),
        (Ok(id), _) => Ok(id),
      }
    }
    .boxed()
  }
}
