use crate::ffi::CompletionState;
use ffi_support::{ConcurrentHandleMap, Handle, HandleError};
use futures::future::{BoxFuture, Either, Future, FutureExt};
use futures::AsyncWriteExt;
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

#[derive(Debug)]
pub enum FfiDelegationError {
  DispatcherDropped,
  DeserializationFailed(anyhow::Error),
  DispatchFailed,
}

impl std::fmt::Display for FfiDelegationError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    std::fmt::Debug::fmt(self, f)
  }
}

impl std::error::Error for FfiDelegationError {}

pub type FfiDelegationContextInner = Box<dyn Any + Send + 'static>;
pub type FfiDelegationContext = Option<FfiDelegationContextInner>;
pub type FfiDelegationResult<T, E> = (Result<T, E>, FfiDelegationContext);

enum FfiDelegationHandler {
  // Disposal of the Box for the method should also result in disposal of any embedded Sender
  BoxedMethod(
    Box<dyn (FnOnce(Result<String, String>, FfiDelegationContext) -> Result<(), ()>) + Send>,
  ),
  Sender(oneshot::Sender<FfiDelegationResult<String, String>>),
}

pub struct FfiDelegation {
  sender: FfiDelegationHandler,
  context: FfiDelegationContext,
}

impl FfiDelegation {
  pub fn new_from_sender(
    fulfill: oneshot::Sender<(Result<String, String>, FfiDelegationContext)>,
  ) -> Self {
    Self {
      sender: FfiDelegationHandler::Sender(fulfill),
      context: None,
    }
  }

  pub fn new_from_sender_contextual(
    fulfill: oneshot::Sender<(Result<String, String>, FfiDelegationContext)>,
    context: impl Any + Send + 'static,
  ) -> Self {
    Self {
      sender: FfiDelegationHandler::Sender(fulfill),
      context: Some(Box::new(context)),
    }
  }

  pub fn new_from_deserialized_sender<
    T: serde::de::DeserializeOwned + Send + 'static,
    E: serde::de::DeserializeOwned + Send + 'static,
  >(
    fulfill: oneshot::Sender<FfiDelegationResult<T, E>>,
    context: FfiDelegationContext,
  ) -> Self {
    let method = Box::new(
      |res: Result<String, String>, ctx: FfiDelegationContext| match &res {
        Ok(t) => match serde_json::from_str::<T>(&t) {
          Ok(t) => fulfill.send((Ok(t), ctx)).map(|_| ()).map_err(|_| ()),
          Err(_) => Err(()),
        },
        Err(e) => match serde_json::from_str::<E>(&e) {
          Ok(e) => fulfill.send((Err(e), ctx)).map(|_| ()).map_err(|_| ()),
          Err(_) => Err(()),
        },
      },
    );
    Self {
      sender: FfiDelegationHandler::BoxedMethod(method),
      context,
    }
  }

  pub fn send(self, result: Result<String, String>) -> Result<(), ()> {
    match self.sender {
      FfiDelegationHandler::Sender(handler) => handler.send((result, self.context)).map_err(|_| ()),
      FfiDelegationHandler::BoxedMethod(handler) => handler(result, self.context),
    }
  }
}

pub struct FfiDelegationSet {
  map: Arc<ConcurrentHandleMap<FfiDelegation>>,
}

impl FfiDelegationSet {
  pub fn new() -> Self {
    Self {
      map: Arc::new(ConcurrentHandleMap::new()),
    }
  }

  /// Handles delegation across a oneshot barrier, but does not register with an ID table
  pub fn delegate_raw<
    'a,
    'b: 'a,
    T: Send + 'static,
    Dispatcher: (FnOnce(oneshot::Sender<T>) -> FutDispatch) + Send + 'a,
    FutDispatch: futures::future::Future<Output = Result<(), FfiDelegationError>> + Send + 'a,
  >(
    &'b self,
    dispatch: Dispatcher,
  ) -> impl Future<Output = Result<T, FfiDelegationError>> + 'a {
    let (sender, receiver) = oneshot::channel::<T>();

    async move {
      // Fire the `dispatch` closure that must eventually result a value being sent via `dispatcher`
      dispatch(sender).await?;

      let res: Result<T, FfiDelegationError> = receiver
        .await
        .map_err(|_| FfiDelegationError::DispatcherDropped);
      res
    }
    .boxed()
  }

  fn deserialize_json_result<
    T: serde::de::DeserializeOwned + Send + 'static,
    E: serde::de::DeserializeOwned + Send + 'static,
  >(
    res: Result<String, String>,
  ) -> Result<Result<T, E>, FfiDelegationError> {
    match res {
      Ok(success) => serde_json::from_str::<T>(&success)
        .map_err(|e| FfiDelegationError::DeserializationFailed(anyhow::Error::from(e)))
        .map(|x| Ok(x)),
      Err(failure) => serde_json::from_str::<E>(&failure)
        .map_err(|e| FfiDelegationError::DeserializationFailed(anyhow::Error::from(e)))
        .map(|x| Err(x)),
    }
  }

  /// Registers delegation with a dispatch table, then hands that registration ID to a blocking task
  /// Expects the task to be fulfilled via `fulfill`; only accepts Result<String, String>, and
  /// handles translation from json to the given serde types.
  pub fn delegate_ffi<
    'a,
    'b: 'a,
    T: serde::de::DeserializeOwned + Send + 'static,
    E: serde::de::DeserializeOwned + Send + 'static,
    C: Any + Send + 'static,
  >(
    &'b self,
    dispatch_ffi: impl (FnOnce(u64) -> ()) + Send + 'static,
    context: Option<C>,
  ) -> impl Future<Output = Result<(Result<T, E>, Option<C>), FfiDelegationError>> + 'a {
    let map = Arc::clone(&self.map);
    async move {
      // Fire the `dispatch` closure that must eventually result a value being sent via `dispatcher`
      let r: Result<
        Result<(Result<T, E>, FfiDelegationContext), FfiDelegationError>,
        FfiDelegationError,
      > = self
        .delegate_raw::<Result<(Result<T, E>, FfiDelegationContext), FfiDelegationError>, _, _>(
          async move |delegation_responder| -> Result<(), FfiDelegationError> {
            // Spin up a non-async worker thread to perform the potentially-blocking tasks
            let res = tokio::task::spawn_blocking(move || {
              // Build the sender, which should translate into the appropriate contextual types and send them to the oneshot
              let boxed_sender = FfiDelegationHandler::BoxedMethod(Box::new(move |res, ctx| {
                // Map the result to a successful/failed output or a delegation failure
                let mapped = Self::deserialize_json_result::<T, E>(res).map(move |m| (m, ctx));
                delegation_responder.send(mapped).map_err(|_| ())
              }));
              // Insert into the map prior to calling, so that a synchronous response won't find "nothing" waiting
              let id = map
                .insert(FfiDelegation {
                  sender: boxed_sender,
                  context: context.map(|x| -> FfiDelegationContextInner { Box::new(x) }),
                })
                .into_u64();
              // TODO: Safeguard against panics when dispatching to the remote
              // TODO: Allow the remote to fail here; report it as an FfiDelegationError "on Dispatch"
              dispatch_ffi(id)
            })
            .await;
            res.map_err(|_| FfiDelegationError::DispatchFailed)
          },
        )
        .await;
      match r {
        Ok(Ok((res, context))) => {
          // Translate context via downcast to the original context type
          Ok((
            res,
            context.map(|c| {
              *c.downcast()
                .expect("Must be the same type as was fed into the function")
            }),
          ))
        }
        // Flatten layers of potential FFI errors
        Ok(Err(e)) => Err(e),
        Err(e) => Err(e),
      }
    }
    .boxed()
  }

  pub async fn delegate_ffi_simple<
    T: serde::de::DeserializeOwned + Send + 'static,
    E: serde::de::DeserializeOwned + Send + 'static,
    Dispatch: (FnOnce(u64) -> ()) + Send + 'static,
  >(
    &self,
    dispatch_ffi: Dispatch,
  ) -> Result<Result<T, E>, FfiDelegationError> {
    match self.delegate_ffi(dispatch_ffi, None: Option<!>).await {
      Ok((res, None)) => Ok(res),
      Err(e) => Err(e),
      _ => unreachable!("Context was present in a context-free delegation!"),
    }
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
      .delegate_ffi(dispatch_ffi, Some(context))
      .boxed()
      .map(|v| v.map(|(l2, ctx)| (l2, ctx.expect("Context must exist in contextual call"))))
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

  pub fn detach_blocking(&self, task_id: u64) -> Result<Option<FfiDelegation>, anyhow::Error> {
    Ok(self.map.remove_u64(task_id)?)
  }

  pub async fn detach(&self, task_id: u64) -> Result<Option<FfiDelegation>, anyhow::Error> {
    let map = Arc::clone(&self.map);
    Ok(tokio::task::spawn_blocking(move || map.remove_u64(task_id)).await??)
  }

  pub fn fulfill_blocking(
    &self,
    task_id: u64,
    completion_state: CompletionState,
    json: String,
  ) -> Result<(), anyhow::Error> {
    let delegation = self
      .detach_blocking(task_id)?
      .ok_or_else(|| anyhow::Error::msg("Delegation handle missing?"))?;
    delegation
      .send(match completion_state {
        CompletionState::Complete => Ok(json),
        CompletionState::Failed => Err(json),
      })
      .map_err(|_| anyhow::Error::msg("Delegation handle was already consumed?"))
  }

  pub async fn fulfill(
    &self,
    task_id: u64,
    completion_state: CompletionState,
    json: String,
  ) -> Result<(), anyhow::Error> {
    let delegation = self
      .detach(task_id)
      .await?
      .ok_or_else(|| anyhow::Error::msg("Delegation handle missing?"))?;
    delegation
      .send(match completion_state {
        CompletionState::Complete => Ok(json),
        CompletionState::Failed => Err(json),
      })
      .map_err(|_| anyhow::Error::msg("Delegation handle was already consumed?"))
  }
}

#[cfg(test)]
mod tests {
  use crate::ffi::ffi_delegation::FfiDelegationSet;
  use crate::ffi::CompletionState;
  use std::sync::Arc;

  #[tokio::test]
  async fn test_ffi_delegation_context() {
    let delegations = Arc::new(FfiDelegationSet::new());
    let delegations_clone = Arc::clone(&delegations);
    let runtime = tokio::runtime::Handle::current();
    let res: Result<(Result<String, ()>, _), _> = {
      delegations
        .delegate_ffi_contextual::<String, (), Arc<String>, _>(
          move |id| {
            let ctxres = runtime
              .block_on(
                delegations_clone
                  .read_context::<Arc<String>, _, _>(id, |x| String::from(x.as_ref())),
              )
              .unwrap();

            assert_eq!(ctxres, String::from("Test Context"));

            delegations_clone
              .fulfill_blocking(id, CompletionState::Complete, String::from("Test Context"))
              .unwrap();
          },
          Arc::new(String::from("Test Context")),
        )
        .await
    };

    println!("FFI returned result: {:#?}", res);
  }
}
