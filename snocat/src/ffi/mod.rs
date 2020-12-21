//! Bindings for instantiation and control via C ABI

use lazy_static::lazy_static;
use ffi_support::{ConcurrentHandleMap, define_bytebuffer_destructor, define_handle_map_deleter, define_string_destructor, HandleError};
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
pub extern "C" fn snocat_read_string(len: *mut u32) -> *const u8 {
  let content = "Hello world from Rusty strings!".as_bytes();
  unsafe {
    *len = content.len() as u32;
  }
  content.as_ptr()
}

#[no_mangle]
pub extern "C" fn snocat_server_start() -> *mut Box<String> {
  println!("HELLO WORLD FROM C API");
  // let x: Box<dyn TunnelManager> = todo!();

  todo!()
}

#[repr(C)]
pub struct RawQuinnTransportConfig {
  pub idle_timeout_ms: u32,
  pub keep_alive_interval_ms: u32,
}

#[no_mangle]
pub extern "C" fn snocat_create_server_config_basic(
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

// #[no_mangle]
// pub extern "C" fn snocat_server_stop(handle: *mut Box<String>) -> () {
//   let inner = unsafe { CPtr::from_c_ptr(handle) };
//   println!(
//     "HELLO WORLD FROM C API - Pretending to stop handle \"{}\"",
//     &*inner
//   );
// }

// struct ServerHandle<T>(Box<ConcurrentDeferredTunnelServer<T>>);
struct ServerHandle<T : TunnelManager>(Option<Box<ConcurrentDeferredTunnelServer<T>>>, u32);
impl<T : TunnelManager> Drop for ServerHandle<T> {
  fn drop(&mut self) {
    println!(
      "HELLO WORLD FROM C API - Pretending to stop server handle \"{}\"",
      &self.1
    );
  }
}

// lazy_static! {
//   static ref SERVER_HANDLES: ConcurrentHandleMap<ServerHandle<Box<dyn TunnelManager>>> = ConcurrentHandleMap::new();
// }
// define_handle_map_deleter!(SERVER_HANDLES, snocat_free_server_handle);

define_bytebuffer_destructor!(snocat_free_buffer);
define_string_destructor!(snocat_free_string);

struct DropPrinter(String);
impl DropPrinter {
  pub fn of(s: &str) -> DropPrinter {
    DropPrinter(String::from(s))
  }
}
impl Drop for DropPrinter {
  fn drop(&mut self) {
    println!("DropPrinter: {}", &self.0)
  }
}

lazy_static! {
  static ref TEST_HANDLES: ConcurrentHandleMap<VTDroppable> = ConcurrentHandleMap::new();
}

#[no_mangle]
pub extern "C" fn create_thing(val: i32) -> u64 {
  // println!("Pretending to allocate something");
  TEST_HANDLES.insert(
    VTDroppable::get_raw(
      Box::new(
        DropPrinter::of(
          &format!("Printer for {}", val)))))
    .into_u64()
}

#[no_mangle]
pub extern "C" fn print_thing(handle: u64) -> () {
  // println!("Pretending to allocate something");
  let x: Result<String, HandleError> = TEST_HANDLES.get_u64(handle,
                       |x| x.try_as_typed_ref::<Box<DropPrinter>>()
                         .map_or(Ok(String::from("<invalid handle>")), |x| Ok(x.as_ref().0.clone())));
  let x = x.unwrap();
  println!("Displaying handle {} with value {:?}", handle, x);
}

#[no_mangle]
pub extern "C" fn take_ownership_of_thing(handle: u64) -> () {
  println!("Pretending to take ownership of something then free it");
  match TEST_HANDLES.remove_u64(handle) {
    Ok(Some(v)) => {
      println!("Took ownership of {:?} at handle {} and freed it", &v.as_typed_ref::<Box<DropPrinter>>().0, handle);
      drop(v);
    }
    Ok(None) => {
      println!("TakeOwnership: No value at handle {}", handle);
    }
    Err(e) => {
      println!("TakeOwnership: Freeing Error {:?} with handle {}", e, handle);
    }
  }

  let droppable = VTDroppable::get_raw(DropPrinter::of("Paradrop"));
  let inner = droppable.extract_typed::<DropPrinter>();
  println!("Extracted printer with contents {}", &inner.0);
}

#[no_mangle]
pub extern "C" fn free_thing(handle: u64) -> () {
  // println!("Pretending to free something");
  match TEST_HANDLES.remove_u64(handle) {
    Ok(Some(v)) => {
      println!("Freed value {:?} at handle {}; {} remain", &v.as_typed_ref::<Box<DropPrinter>>().0, handle, TEST_HANDLES.len());
    }
    Ok(None) => {
      println!("No value at handle {}", handle);
    }
    Err(e) => {
      println!("Freeing Error {:?} with handle {}", e, handle);
    }
  }
}
