//! Bindings for instantiation and control via C ABI

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
  },
};
use futures::future::BoxFuture;
use futures::future::{Future, FutureExt};
use pin_project::pin_project;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::oneshot::error::RecvError;
use tokio::sync::{oneshot, Mutex};

pub trait CPtr {
  fn into_c_ptr(self: Box<Self>) -> *mut Box<Self>;
  unsafe fn from_c_ptr(ptr: *mut Box<Self>) -> Box<Self>;
}

impl<T: Sized> CPtr for T {
  fn into_c_ptr(self: Box<T>) -> *mut Box<T> {
    Box::into_raw(Box::new(self))
  }
  unsafe fn from_c_ptr(ptr: *mut Box<T>) -> Box<T> {
    *Box::from_raw(ptr)
  }
}

impl CPtr for dyn TunnelManager {
  fn into_c_ptr(self: Box<dyn TunnelManager>) -> *mut Box<dyn TunnelManager> {
    Box::into_raw(Box::new(self))
  }
  unsafe fn from_c_ptr(ptr: *mut Box<dyn TunnelManager>) -> Box<dyn TunnelManager> {
    *Box::from_raw(ptr)
  }
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct FFIHandle<T>(pub u64, PhantomData<T>);
impl<T> ::std::ops::Deref for FFIHandle<T> {
  type Target = u64;
  #[inline]
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

#[no_mangle]
pub extern "C" fn axl_read_string(len: *mut u32) -> *const u8 {
  let content = "Hello world from Rusty strings!".as_bytes();
  unsafe {
    *len = content.len() as u32;
  }
  content.as_ptr()
}

#[no_mangle]
pub extern "C" fn axl_server_start() -> *mut Box<String> {
  println!("HELLO WORLD FROM C API");
  // let x: Box<dyn TunnelManager> = todo!();
  let x: Box<String> = Box::new(String::from("Hello, I'm allocating"));
  let y = x.into_c_ptr();
  // let z = unsafe { CPtr::from_c_ptr(y) };

  // y
  todo!()
}

#[repr(C)]
pub struct RawQuinnTransportConfig {
  pub idle_timeout_ms: u32,
  pub keep_alive_interval_ms: u32,
}

#[no_mangle]
pub extern "C" fn axl_create_server_config_basic(
  allow_migration: bool,
  certificate_path: *const i8,
  transport_config: RawQuinnTransportConfig,
) -> *mut quinn::ServerConfig {
  if certificate_path.is_null() {
    return std::ptr::null_mut();
  }
  let cert_path_str = unsafe { std::ffi::CStr::from_ptr(certificate_path) };
  let cert_path_str = match cert_path_str.to_str() {
    Ok(s) if s.len() > std::u16::MAX as usize => {
      return std::ptr::null_mut();
    }
    Err(_e) => {
      return std::ptr::null_mut();
    }
    Ok(s) => s,
  };

  std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn axl_server_stop(handle: *mut Box<String>) -> () {
  let inner = unsafe { CPtr::from_c_ptr(handle) };
  println!(
    "HELLO WORLD FROM C API - Pretending to stop handle \"{}\"",
    &*inner
  );
}
