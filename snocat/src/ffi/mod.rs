//! Bindings for instantiation and control via C ABI

use lazy_static::lazy_static;
use ffi_support::{ConcurrentHandleMap, define_bytebuffer_destructor, define_handle_map_deleter, define_string_destructor, implement_into_ffi_by_json, HandleError, FfiStr, ExternError, IntoFfi};
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
use std::net::{IpAddr, Ipv4Addr};

pub mod errors;
pub mod dto;

define_bytebuffer_destructor!(snocat_free_buffer);
define_string_destructor!(snocat_free_string);

struct ServerHandle<T : TunnelManager>(Option<Box<ConcurrentDeferredTunnelServer<T>>>, u32);
impl<T : TunnelManager> Drop for ServerHandle<T> {
  fn drop(&mut self) {
    println!(
      "Pretending to stop server handle \"{}\"",
      &self.1
    );
  }
}

lazy_static! {
  static ref SERVER_HANDLES: ConcurrentHandleMap<ServerHandle<Box<dyn TunnelManager>>> = ConcurrentHandleMap::new();
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
pub extern "C" fn snocat_server_start(
  config_json: FfiStr,
  error: &mut ExternError
) -> u64 {
  ffi_support::call_with_result::<ServerHandle<_>, errors::FfiError, _>(error, || {
    use std::convert::TryInto;
    let config = serde_json::from_str::<dto::ServerConfig>(config_json.as_str())?;
    let config: quinn::ServerConfig = config.try_into()?;
    println!("HELLO WORLD FROM C API");
    // let server = ConcurrentDeferredTunnelServer::new(TcpTunnelManager::new(
    //   Range::new(8000, 8010),
    //   IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
    //   todo!()
    // ));
    Ok(ServerHandle(None, 0))
  })
}
