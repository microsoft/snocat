// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use super::{delegation::CompletionState, ConcurrentHandleMap};
use futures::Future;
use prost::Message;
use std::{convert::TryInto, sync::Arc};

pub mod proto;

#[repr(u32)]
pub enum EventCompletionState {
  Complete = 0,
  Panicked = 1,
  Cancelled = 2,
  DispatchFailed = 3,
}

#[derive(Debug)]
pub enum EventingError {
  DispatcherDropped,
  DeserializationFailed(anyhow::Error),
  DispatchFailed,
}

impl std::fmt::Display for EventingError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    std::fmt::Debug::fmt(self, f)
  }
}

impl std::error::Error for EventingError {}

#[repr(transparent)]
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub struct EventHandle<T>(u64, std::marker::PhantomData<T>);

impl<T> EventHandle<T> {
  pub fn new(event_id: u64) -> Self {
    Self(event_id, std::marker::PhantomData)
  }

  pub fn raw(&self) -> u64 {
    self.0
  }
}

impl<T> Into<u64> for EventHandle<T> {
  fn into(self) -> u64 {
    self.raw()
  }
}

crate::DroppableCallback!(
  ReportEventCompletionCb,
  fn(event_handle: u64, buffer: *const u8, buffer_len: u32) -> ()
);

impl ReportEventCompletionCb {
  fn invoke_safe(&self, event_handle: u64, buffer: &[u8]) -> () {
    self.invoke(event_handle, buffer.as_ptr(), buffer.len() as u32)
  }
}

/// An `EventRunner` tracks Rust Futures as promises across an FFI
/// The remote calls a local future-providing function with a chosen, arbitrary handle,
/// and the local state machine will post back to the remote upon completion or failure.
pub struct EventRunner {
  rt: tokio::runtime::Handle,
  report_task_completion_callback: Arc<ReportEventCompletionCb>,
}

impl EventRunner {
  pub fn new(
    rt: tokio::runtime::Handle,
    report_task_completion_callback: Arc<ReportEventCompletionCb>,
  ) -> Self {
    Self {
      rt,
      report_task_completion_callback,
    }
  }

  pub fn fire_evented<
    T: Message + Default + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
  >(
    &self,
    event_id: u64,
    event_dispatch: Fut,
  ) -> Result<(), EventingError> {
    let report = Arc::clone(&self.report_task_completion_callback);
    let event_task = self.rt.spawn(async move {
      let res = event_dispatch.await;

      use proto::event_result as evres;
      use crate::util::messenger::{Messenger, AnyProto};
      // TODO: Make T require AnyProtoIdentified to provide a type_url
      let packed = match AnyProto::new("https://localhost/evented_result_todo", res).try_into() {
        Ok(any_proto) => evres::Result::Completed(evres::Completion { value: Some(any_proto) }),
        Err(e) => {
          tracing::error!(target = "ffi_event_serialization_failure", ?event_id, outward = true, error = ?e);
          evres::Result::DispatchFailed(evres::DispatchFailure { })
        }
      };
      let output = proto::EventResult {
        result: Some(packed),
      };
      let buffer = output.encode_length_delimited_vec()
        .expect("Result serialization must be infallible");
      report.invoke_safe(event_id, &buffer);
    });

    let monitor = self.monitor(event_id, event_task);
    let _ = self.rt.spawn(monitor);
    Ok(())
  }

  fn monitor(
    &self,
    event_id: u64,
    spawned_task: tokio::task::JoinHandle<()>,
  ) -> impl Future<Output = ()> {
    let report = Arc::clone(&self.report_task_completion_callback);
    async move {
      if let Err(e) = spawned_task.await {
        use proto::event_result as evres;
        let res = if e.is_panic() {
          tracing::error!(target = "ffi_panic", ?event_id, outward = true);
          proto::EventResult {
            result: Some(evres::Result::Panicked(evres::Panic {})),
          }
        } else if e.is_cancelled() {
          tracing::error!(target = "ffi_event_cancelled", ?event_id, outward = true, error = ?e);
          proto::EventResult {
            result: Some(evres::Result::Cancelled(evres::Cancellation {})),
          }
        } else {
          tracing::error!(target = "ffi_event_failure", ?event_id, outward = true, error = ?e);
          proto::EventResult {
            result: Some(evres::Result::DispatchFailed(evres::DispatchFailure {})),
          }
        };

        // Inform the remote that the call failed
        use crate::util::messenger::Messenger;
        let buffer = res
          .encode_length_delimited_vec()
          .expect("Packing protobuf must not fail");
        report.invoke_safe(event_id, &buffer);
      }
    }
  }

  pub fn fire_evented_handle<
    T: Message + Default + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
  >(
    &self,
    event_id: EventHandle<T>,
    event_dispatch: Fut,
  ) -> Result<(), EventingError> {
    self.fire_evented(event_id.into(), event_dispatch)
  }
}
