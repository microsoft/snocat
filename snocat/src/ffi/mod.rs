// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
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
