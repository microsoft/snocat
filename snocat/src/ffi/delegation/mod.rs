// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! `FFI Delegation` is a process of representing remote future-likes as [Futures](::futures::Future).
//!
//! This module includes everything necessary to create or use bindings for external
//! future-likes, and provides a means of handling the various errors that may occur.
//!
//! This module is based on a form of contextual continuation passing mediated by [oneshots](mod@::tokio::sync::oneshot).
//!
//! To represent Rust Futures as Promises across an FFI, see [Eventing](mod@crate::ffi::eventing).

use std::{
  any::Any,
  marker::PhantomData,
  net::{IpAddr, Ipv4Addr, SocketAddr},
  ops::Deref,
  pin::Pin,
  sync::Arc,
  task::{Context, Poll},
};

use anyhow::Context as AnyhowContext;
use ffi_support::{ConcurrentHandleMap, Handle, HandleError};
use futures::{
  future::{BoxFuture, Either, Future, FutureExt},
  AsyncWriteExt,
};
use lazy_static::lazy_static;
use prost::{bytes::Buf, Message};
use tokio::sync::{
  oneshot::{self, error::RecvError},
  Mutex,
};

use crate::util::{messenger::AnyProto, MappedOwnedMutexGuard};

pub mod proto;

/// `C`-compatible enum declaring which state a result occupies
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
#[repr(u32)]
pub enum CompletionState {
  Complete = 0,
  Cancelled = 1,
  Exception = 2,
}

/// Any error that occurs in the process of dispatching or receiving results for a delegation
///
/// This is in opposition to the [RemoteError] type, which represents failures occurring
/// within the remote context, under control of an external event loop.
#[derive(Debug)]
pub enum DelegationError {
  DispatcherDropped,
  DeserializationFailed(anyhow::Error),
  DispatchFailed,
  Cancelled,
  RemoteException(anyhow::Error),
}

impl std::fmt::Display for DelegationError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    std::fmt::Debug::fmt(self, f)
  }
}

impl std::error::Error for DelegationError {}

/// An error that occurs under the remote event loop, when fulfilling the promise
///
/// For errors that occur during dispatch, see [DelegationError].
#[derive(Debug)]
pub enum RemoteError {
  Cancelled,
  Exception(anyhow::Error),
}

impl std::fmt::Display for RemoteError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    std::fmt::Debug::fmt(self, f)
  }
}

impl std::error::Error for RemoteError {}

/// A dynamically-typed context allowing access by remote code which is in possession of the delegation ID
///
/// This type is held optionally within [Delegation] instances, and sent to the appropriate handler upon usage.
/// To access the context, use [DelegationSet::read_context], [DelegationSet::with_context_optarx], or
/// [DelegationSet::extract_optarx_context], depending on situation.
pub type DelegationContext = Box<dyn Any + Send + 'static>;

/// The result of a [Delegation], alongside an optional, upcasted context
///
/// May be downcast into a [TypedDelegationResult], with a proof of knowledge of the type it contains.
pub struct DelegationResult<T>(pub T, pub Option<DelegationContext>);

/// The result of a [Delegation], with a strongly typed context
///
/// May be freely upcasted into a [DelegationResult], but, in order to preserve
/// type information, such knowledge will need to be relayed via a side channel.
pub struct TypedDelegationResult<T, TContext: Any + Send + 'static>(pub T, pub Option<TContext>);

/// A [DelegationResult] from a remote FFI, or a potential [RemoteError]
///
/// See [RemoteResultRaw] for a version without a context slot.
pub type RemoteResult<T> = Result<DelegationResult<T>, RemoteError>;

/// A partial [DelegationResult] from a remote FFI, or a potential [RemoteError]
///
/// See [RemoteResult] for a version including a context slot.
pub type RemoteResultRaw<T> = Result<T, RemoteError>;

// Upcasting from a strongly-typed slot to an Any-typed slot is infallible, so we always return
impl<T, TContext: Any + Send + 'static> Into<DelegationResult<T>>
  for TypedDelegationResult<T, TContext>
{
  fn into(self) -> DelegationResult<T> {
    DelegationResult(self.0, self.1.map(|c| -> DelegationContext { Box::new(c) }))
  }
}

// Downcasting is fallible; If C doesn't match the true contents, we hand back a rebuilt instance of the original type
// Note that we don't save an `Any` trait-object if the value is None, so type-identity is only enforced while a value is present
// This is still memory-safe, however, as types will simply fail to cast if the context type was somehow swapped while in use
impl<T, TContext: Any + Send + 'static> std::convert::TryInto<TypedDelegationResult<T, TContext>>
  for DelegationResult<T>
{
  type Error = DelegationResult<T>;

  fn try_into(self) -> Result<TypedDelegationResult<T, TContext>, Self::Error> {
    use std::any::Any;
    // Translate context via downcast to the original context type
    match self.1 {
      // If the context is blank, we can skip type-casting and use a constant None
      None => Ok(TypedDelegationResult(self.0, None)),
      Some(ctx) => match ctx.downcast::<TContext>() {
        Ok(ctx) => Ok(TypedDelegationResult(self.0, Some(*ctx))),
        Err(ctx) => Err(DelegationResult(self.0, Some(ctx))),
      },
    }
  }
}

/// Semi-dynamically-typed sender mechanism for various internal implementations
enum DelegationHandler {
  /// A method which maps the types of its inputs before sending them for processing.
  ///
  /// This handler is doubly useful when the transfer protocol is bulky and should be cleared from
  /// memory before the loop cycles back to process this handler's response.
  /// Polymorphic handlers are achieved by mapping from a static transport type to a generic inner type.
  ///
  /// Note that disposal of the Box for the method must also result in disposal of any embedded Sender.
  BoxedMethod(Box<dyn (FnOnce(RemoteResult<prost_types::Any>) -> Result<(), ()>) + Send>),

  /// A oneshot which accepts just a result and forwards it on, instead of parsing before sending.
  Sender(oneshot::Sender<RemoteResult<prost_types::Any>>),
}

impl std::fmt::Debug for DelegationHandler {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self {
      DelegationHandler::BoxedMethod(_) => write!(f, "(Boxed Method Sender)"),
      DelegationHandler::Sender(_) => write!(f, "(Oneshot Sender)"),
    }
  }
}

/// A representation of an asynchronous task taking place across an FFI boundary
///
/// Contains a continuation which resolves a Future when fulfilled.
#[derive(Debug)]
pub struct Delegation {
  sender: DelegationHandler,
  context: Option<DelegationContext>,
}

impl Delegation {
  pub fn new_from_sender(fulfill: oneshot::Sender<RemoteResult<prost_types::Any>>) -> Self {
    Self {
      sender: DelegationHandler::Sender(fulfill),
      context: None,
    }
  }

  pub fn new_from_sender_contextual(
    fulfill: oneshot::Sender<RemoteResult<prost_types::Any>>,
    context: impl Any + Send + 'static,
  ) -> Self {
    Self {
      sender: DelegationHandler::Sender(fulfill),
      context: Some(Box::new(context)),
    }
  }

  fn deserialize_result<T: prost::Message + Default + Send + 'static>(
    res: prost_types::Any,
  ) -> Result<T, DelegationError> {
    AnyProto::from_any_unverified(&res)
      .map_err(|e| DelegationError::DeserializationFailed(anyhow::Error::from(e)))
      .map(|r| r.to_value())
  }

  pub fn new_from_deserialized_sender<T: prost::Message + Default + Send + 'static>(
    fulfill: oneshot::Sender<Result<RemoteResult<T>, DelegationError>>,
    context: Option<DelegationContext>,
  ) -> Self {
    let method = Box::new(|res: RemoteResult<prost_types::Any>| match res {
      Err(remote_error) => fulfill.send(Ok(Err(remote_error))).map_err(|_| ()),
      Ok(DelegationResult(remote_result, ctx)) => {
        // Map the result to a successful/failed output or a delegation failure
        // TODO: Some form of verification on the value of prost_types::Any::type_url
        let typed = Self::deserialize_result::<T>(remote_result);
        // Produce the value to send by wrapping the result in an Ok with its context
        // Note that we still map to a DelegationResult and not a TypedDelegationResult
        // because the type of the context is not relevant, required, or necessarily known.
        let value: Result<Result<DelegationResult<T>, RemoteError>, DelegationError> =
          typed.map(|remote_result| Ok(DelegationResult(remote_result, ctx)));
        // Send the value to the handler, and squash handle failures into unit errors
        fulfill.send(value).map_err(|_| ())
      }
    });
    Self {
      sender: DelegationHandler::BoxedMethod(method),
      context,
    }
  }

  pub fn send(self, result: RemoteResultRaw<prost_types::Any>) -> Result<(), ()> {
    let context = self.context;
    let with_context = result.map(|r| DelegationResult(r, context));
    match self.sender {
      DelegationHandler::Sender(handler) => handler.send(with_context).map_err(|_| ()),
      DelegationHandler::BoxedMethod(handler) => handler(with_context),
    }
  }
}

/// A mapping which tracks externally-delegated task IDs
/// and binds them to continuations via [Delegation]s
pub struct DelegationSet {
  map: Arc<ConcurrentHandleMap<Delegation>>,
}

impl DelegationSet {
  /// DelegationSets are cheap to create, but routing to the appropriate instance from bindings is complicated.
  ///
  /// Generally, you will only have one at any point in time, accessible globally under a static.
  pub fn new() -> Self {
    Self {
      map: Arc::new(ConcurrentHandleMap::new()),
    }
  }

  /// Handles delegation across a oneshot barrier, but does not register with an ID table
  fn delegate_raw<
    'a,
    T: Send + 'static,
    Dispatcher: (FnOnce(oneshot::Sender<Result<T, DelegationError>>) -> FutDispatch) + Send + 'a,
    FutDispatch: futures::future::Future<Output = Result<(), DelegationError>> + Send + 'a,
  >(
    dispatch: Dispatcher,
  ) -> impl Future<Output = Result<T, DelegationError>> + 'a {
    // Include possibility of an FfiDelegationError for if the dispatch-side handler fails
    // Most likely, this would be from a failure in deserialization of invalid results
    let (sender, receiver) = oneshot::channel::<Result<T, DelegationError>>();

    async move {
      // Fire the `dispatch` closure that must eventually result a value being sent via `dispatcher`
      dispatch(sender).await?;

      let res = receiver
        .await
        // If we hit a RecvError, it's because the source was dropped, so we can clobber the result here
        .map_err(|_| DelegationError::DispatcherDropped)?;
      // Any FfiDelegationError may have been thrown; in effect, merge the error-spaces
      res
    }
    .boxed()
  }

  /// Registers a new [Delegation] with a dispatch table, then hands that registration's ID to a blocking task
  /// Expects the task to be fulfilled via [fulfill](DelegationSet::fulfill) or [fulfill_blocking](DelegationSet::fulfill_blocking).
  fn delegate_ffi<
    'a,
    'b: 'a,
    T: prost::Message + Default + Send + 'static,
    C: Any + Send + 'static,
    TDispatch: (FnOnce(u64) -> ()) + Send + 'static,
  >(
    &'b self,
    dispatch_ffi: TDispatch,
    context: Option<C>,
  ) -> impl Future<Output = Result<Result<TypedDelegationResult<T, C>, RemoteError>, DelegationError>> + 'a
  {
    let map = Arc::clone(&self.map);
    async move {
      // Fire the `dispatch` closure that must eventually result a value being sent via `dispatcher`
      let r = Self::delegate_raw::<RemoteResult<T>, _, _>(
        async move |delegation_responder: oneshot::Sender<
          Result<RemoteResult<T>, DelegationError>,
        >|
                    -> Result<(), DelegationError> {
          // Build the sender closure, which should translate into the appropriate contextual types and send them to the oneshot
          let delegation = Delegation::new_from_deserialized_sender::<T>(
            delegation_responder,
            context.map(|x| -> DelegationContext { Box::new(x) }),
          );
          // Spin up a non-async worker thread to perform the potentially-blocking tasks
          let res = tokio::task::spawn_blocking(move || {
            // Insert into the map prior to calling, so that a synchronous response won't find "nothing" waiting
            let id = map.insert(delegation).into_u64();
            // TODO: Safeguard against panics when dispatching to the remote
            // TODO: Allow the remote to fail here; report it as an FfiDelegationError "on Dispatch"
            dispatch_ffi(id)
          })
          .await;
          res.map_err(|_| DelegationError::DispatchFailed)
        },
      )
      .await;

      // At this point we have an FfiDelegationError, an FfiRemoteError, or an FfiDelegationResult
      // We need a strongly-typed context version of the result, so transform and attempt the downcast
      // Dodge the first with ? and map the innermost layer with a context-cast
      Ok(r?.map(|res @ DelegationResult(_, _)| {
        // Translate context via downcast to the original context type
        use std::convert::TryInto;
        res
          .try_into()
          .map_err(|_| ()) // Dodge expect's Debug requirement on the FfiDelegationResult type
          .expect("Result context must be the same type as was fed into the function")
      }))
    }
    .boxed()
  }

  pub async fn delegate_ffi_simple<
    T: Message + Default + Send + 'static,
    TDispatchFromId: (FnOnce(u64) -> ()) + Send + 'static,
  >(
    &self,
    dispatch_ffi: TDispatchFromId,
  ) -> Result<Result<T, RemoteError>, DelegationError> {
    let no_context: Option<!> = None;
    match self.delegate_ffi::<T, !, _>(dispatch_ffi, no_context).await {
      Err(delegation_error) => Err(delegation_error),
      Ok(Err(remote_error)) => Ok(Err(remote_error)),
      Ok(Ok(TypedDelegationResult(res, None))) => Ok(Ok(res)),
      Ok(Ok(TypedDelegationResult(_res, Some(_)))) => {
        unreachable!("Context was present in a context-free delegation!")
      }
    }
  }

  pub fn delegate_ffi_contextual<
    'a,
    'b: 'a,
    T: Message + Default + Send + 'static,
    TContext: Any + Send + 'static,
    TDispatchFromId: (FnOnce(u64) -> ()) + Send + 'static,
  >(
    &'b self,
    dispatch_ffi: TDispatchFromId,
    context: TContext,
  ) -> BoxFuture<'a, Result<Result<(T, TContext), RemoteError>, DelegationError>> {
    self
      .delegate_ffi(dispatch_ffi, Some(context))
      .boxed()
      .map(|v| {
        v.map(|v2| {
          v2.map(|TypedDelegationResult(l2, ctx)| {
            (l2, ctx.expect("Context must exist in contextual call"))
          })
        })
      })
      .boxed()
  }

  pub fn len(&self) -> usize {
    self.map.len()
  }

  pub async fn read_context<
    TContext: Any + Send + 'static,
    TResult: Send + 'static,
    FWithContext: (FnOnce(&TContext) -> TResult) + Send + 'static,
  >(
    &self,
    delegation_handle_id: u64,
    with_context: FWithContext,
  ) -> Result<TResult, anyhow::Error> {
    let map = Arc::clone(&self.map);
    Ok(
      tokio::task::spawn_blocking(move || {
        map.get_u64(delegation_handle_id, move |del_ref| {
          match &del_ref.context {
            None => Err(anyhow::Error::msg("No context available for given task")),
            Some(c) => {
              let ctx: Option<&TContext> = c.downcast_ref();
              ctx
                .map(with_context)
                .ok_or_else(|| anyhow::Error::msg("Context did not match the requested type"))
            }
          }
        })
      })
      .await??,
    )
  }

  fn clone_optarx_context_arc<TContextInOpt: Any + Send + 'static>(
    &self,
    delegation_handle_id: u64,
  ) -> BoxFuture<'_, Result<Arc<tokio::sync::Mutex<Option<TContextInOpt>>>, anyhow::Error>> {
    self.read_context::<
      Arc<tokio::sync::Mutex<Option<TContextInOpt>>>,
      Arc<tokio::sync::Mutex<Option<TContextInOpt>>>,
      _,
    >(
      delegation_handle_id,
      |c| Arc::clone(c),
    )
      .boxed()
      .map(|res| res.context("Arc<Mutex<Option<Context>> cloning read failure"))
      .boxed()
  }

  /// Allows working with Optarx content mutably, as long as it remains present.
  /// Errors when the item is no longer present in the context.
  pub fn with_context_optarx<
    'a,
    's: 'a,
    TContext: Any + Send + 'static,
    TResult: Send + 'static,
    FWithContext: (FnOnce(MappedOwnedMutexGuard<Option<TContext>, TContext>) -> FutResult) + Send + 'a,
    FutResult: Future<Output = TResult> + Send + 'a,
  >(
    &'s self,
    delegation_handle_id: u64,
    with_context: FWithContext,
  ) -> BoxFuture<'a, Result<TResult, anyhow::Error>> {
    async move {
      let context_optarx = self
        .clone_optarx_context_arc::<TContext>(delegation_handle_id)
        .await?;
      let lock = context_optarx.lock_owned().await;
      if lock.is_none() {
        Err(anyhow::Error::msg("Context was no longer owned by optarx"))
      } else {
        let mapped_lock = MappedOwnedMutexGuard::new(lock, |outer| outer.as_ref().unwrap());
        let contextual_result = with_context(mapped_lock).await;
        Ok(contextual_result)
      }
    }
    .boxed()
  }

  pub async fn extract_optarx_context<TContext: Any + Send + 'static>(
    &self,
    delegation_handle_id: u64,
  ) -> Result<Option<TContext>, anyhow::Error> {
    let context_optarx = self
      .clone_optarx_context_arc::<TContext>(delegation_handle_id)
      .await?;
    let mut lock = context_optarx.lock().await;
    Ok(std::mem::replace(&mut *lock, None))
  }

  pub fn detach_blocking(&self, task_id: u64) -> Result<Option<Delegation>, anyhow::Error> {
    Ok(self.map.remove_u64(task_id)?)
  }

  pub async fn detach(&self, task_id: u64) -> Result<Option<Delegation>, anyhow::Error> {
    let map = Arc::clone(&self.map);
    Ok(tokio::task::spawn_blocking(move || map.remove_u64(task_id)).await??)
  }

  fn map_completion_state(
    result: self::proto::DelegateResult,
  ) -> RemoteResultRaw<prost_types::Any> {
    let result = result
      .result
      .expect("DelegateResult instances must always fulfill a one-of constraint");
    use proto::delegate_result::{Cancellation, Completion, Exception, Result as Res};
    match result {
      Res::Completed(Completion { value, .. }) => Ok(value.expect("Field `value` is required")),
      Res::Cancelled(Cancellation { .. }) => Err(RemoteError::Cancelled),
      Res::Exception(Exception { exception, .. }) => {
        use proto::delegate_result::exception::Exception as EType;
        match exception.expect("Exception value must be included") {
          EType::Any(any) => match any.type_url.as_str() {
            "text" | "string" => Err(RemoteError::Exception(anyhow::Error::msg(
              String::decode(&*any.value).expect("Must unpack successfully"),
            ))),
            type_url => Err(RemoteError::Exception(anyhow::Error::msg(format!(
              "Any-type exception with type url {}",
              type_url
            )))),
          },
          EType::Text(text) => Err(RemoteError::Exception(anyhow::Error::msg(text))),
          EType::Json(json) => {
            let json: serde_json::Value =
              serde_json::from_str(&json).expect("Remote Exception contents must be valid json");
            let pretty_json_str = serde_json::to_string_pretty(&json)
              .expect("Reencoding a freshly decoded json value must succeed");
            Err(RemoteError::Exception(anyhow::Error::msg(pretty_json_str)))
          }
        }
      }
    }
  }

  pub fn fulfill_blocking(&self, task_id: u64, result: Vec<u8>) -> Result<(), anyhow::Error> {
    let result = self::proto::DelegateResult::decode_length_delimited(&*result)
      .map_err(|_| anyhow::Error::msg("Delegation result wrapper could not be deserialized"))?;
    let delegation = self
      .detach_blocking(task_id)?
      .ok_or_else(|| anyhow::Error::msg("Delegation handle missing?"))?;
    delegation
      .send(Self::map_completion_state(result))
      .map_err(|_| anyhow::Error::msg("Delegation handle was already consumed?"))
  }

  pub async fn fulfill(&self, task_id: u64, result: Vec<u8>) -> Result<(), anyhow::Error> {
    let result = self::proto::DelegateResult::decode_length_delimited(&*result)
      .map_err(|_| anyhow::Error::msg("Delegation result wrapper could not be deserialized"))?;
    let delegation = self
      .detach(task_id)
      .await?
      .ok_or_else(|| anyhow::Error::msg("Delegation handle missing?"))?;
    delegation
      .send(Self::map_completion_state(result))
      .map_err(|_| anyhow::Error::msg("Delegation handle was already consumed?"))
  }
}

#[cfg(test)]
mod tests {
  use std::{convert::TryInto, sync::Arc};

  use crate::util::messenger::*;
  use prost::Message;

  use super::proto;
  use super::{DelegationError, DelegationSet, RemoteError, RemoteResult};
  use crate::ffi::CompletionState;

  #[tokio::test]
  async fn test_ffi_delegation_context() {
    let delegations = Arc::new(DelegationSet::new());
    let delegations_clone = Arc::clone(&delegations);
    let runtime = tokio::runtime::Handle::current();
    let res: Result<Result<(String, _), RemoteError>, DelegationError> = {
      delegations
        .delegate_ffi_contextual::<String, Arc<String>, _>(
          move |id| {
            let ctxres = runtime
              .block_on(
                delegations_clone
                  .read_context::<Arc<String>, _, _>(id, |x| String::from(x.as_ref())),
              )
              .unwrap();

            assert_eq!(ctxres, String::from("Test Context"));

            let buffer = (proto::DelegateResult {
              result: Some(proto::delegate_result::Result::Completed(
                proto::delegate_result::Completion {
                  value: Some(
                    AnyProto::new("string", String::from("\"hello world\""))
                      .try_into()
                      .unwrap(),
                  ),
                },
              )),
            })
            .encode_length_delimited_vec()
            .unwrap();

            delegations_clone.fulfill_blocking(id, buffer).unwrap();
          },
          Arc::new(String::from("Test Context")),
        )
        .await
    };
    let res = res.unwrap().unwrap();

    println!("FFI returned result: {:#?}", res);
    assert_eq!(&res.0, "\"hello world\"");
  }

  #[tokio::test]
  async fn test_ffi_delegation_remote_failure() {
    let delegations = Arc::new(DelegationSet::new());
    let delegations_clone = Arc::clone(&delegations);
    let runtime = tokio::runtime::Handle::current();
    let res: Result<Result<(String, _), RemoteError>, DelegationError> = {
      delegations
        .delegate_ffi_contextual::<String, Arc<String>, _>(
          move |id| {
            let ctxres = runtime
              .block_on(
                delegations_clone
                  .read_context::<Arc<String>, _, _>(id, |x| String::from(x.as_ref())),
              )
              .unwrap();

            assert_eq!(ctxres, String::from("Test Context"));

            let mut buffer = Vec::<u8>::new();
            (proto::DelegateResult {
              result: Some(proto::delegate_result::Result::Cancelled(
                proto::delegate_result::Cancellation {},
              )),
            })
            .encode_length_delimited(&mut buffer)
            .unwrap();

            delegations_clone.fulfill_blocking(id, buffer).unwrap();
          },
          Arc::new(String::from("Test Context")),
        )
        .await
    };
    let res = res.unwrap();

    println!("FFI returned result: {:#?}", res);
    assert!(matches!(res, Err(RemoteError::Cancelled)));
  }
}
