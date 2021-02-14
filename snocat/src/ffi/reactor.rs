use futures::future::{BoxFuture, FutureExt};
use futures_io::AsyncBufRead;
use lazy_static::lazy_static;
use std::{
  any::Any,
  marker::PhantomData,
  net::{IpAddr, Ipv4Addr, SocketAddr},
  ops::Deref,
  pin::Pin,
  sync::Arc,
  task::{Context, Poll},
};
use tokio::sync::{oneshot, oneshot::error::RecvError, Mutex};

use crate::DroppableCallback;

use crate::ffi::{
  delegation::{
    self, CompletionState, Delegation, DelegationError, DelegationResult, DelegationSet,
    RemoteError as FfiRemoteError,
  },
  errors,
};

use crate::ffi::{
  allocators::{self, get_bytebuffer_raw, RawByteBuffer},
  eventing::{self, EventCompletionState, EventHandle, EventRunner, EventingError},
};

pub struct Reactor {
  rt: Arc<tokio::runtime::Runtime>,
  /// Delegations wrap remote tasks (completion/failure) and map them to futures
  /// Each delegation is identified by handle and contains either a sender or a mapped-send-closure
  /// The `delegations` mapping has items added when being delegated to the remote, and removed
  /// when being fulfilled by the remote calling `snocat_report_async_update`
  delegations: Arc<DelegationSet>,
  events: Arc<EventRunner>,
  report_task_completion_callback: Arc<eventing::ReportEventCompletionCb>,
}

impl Reactor {
  pub fn start(
    report_task_completion_callback: Arc<eventing::ReportEventCompletionCb>,
  ) -> Result<Self, anyhow::Error> {
    let rt = tokio::runtime::Builder::new()
      .threaded_scheduler()
      .thread_name("tokio-reactor-worker")
      .enable_all()
      .build()?;
    Ok(Reactor {
      delegations: Arc::new(DelegationSet::new()),
      events: Arc::new(EventRunner::new(
        rt.handle().clone(),
        Arc::clone(&report_task_completion_callback),
      )),
      report_task_completion_callback,
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
  ) -> Result<Result<Result<T, E>, FfiRemoteError>, DelegationError> {
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
    C: Any + Send + 'static,
    Dispatch: (FnOnce(u64) -> ()) + Send + 'static,
  >(
    &'b self,
    dispatch_ffi: Dispatch,
    context: C,
  ) -> BoxFuture<'a, Result<Result<(T, C), FfiRemoteError>, DelegationError>> {
    self
      .delegations
      .delegate_ffi_contextual(dispatch_ffi, context)
      .boxed()
  }

  pub fn fulfill_blocking(
    &self,
    task_id: u64,
    completion_state: CompletionState,
    json: String,
  ) -> Result<(), anyhow::Error> {
    self
      .delegations
      .fulfill_blocking(task_id, completion_state, json)
  }

  pub fn fulfill(
    &self,
    task_id: u64,
    completion_state: CompletionState,
    json: String,
  ) -> BoxFuture<Result<(), anyhow::Error>> {
    self
      .delegations
      .fulfill(task_id, completion_state, json)
      .boxed()
  }

  pub fn get_delegations(&self) -> &Arc<delegation::DelegationSet> {
    &self.delegations
  }

  pub fn get_events_ref(&self) -> Arc<eventing::EventRunner> {
    Arc::clone(&self.events)
  }

  pub fn get_runtime_ref(&self) -> Arc<tokio::runtime::Runtime> {
    Arc::clone(&self.rt)
  }

  pub fn runtime_handle(&self) -> &tokio::runtime::Handle {
    self.rt.handle()
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
