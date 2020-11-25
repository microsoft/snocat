//! Utilities for handling transfer of ownership of a future to an external resolver, and
//! tracking the ouutstanding tasks without blocking.

use futures::future::BoxFuture;
use futures::future::{Future, FutureExt};
use pin_project::pin_project;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::oneshot::error::RecvError;
use tokio::sync::{oneshot, Mutex};
use futures::TryFutureExt;

#[derive(Default, Debug)]
pub struct DelegatedTask {}

#[derive(Default, Debug)]
struct DelegationSet {
  next_id: std::sync::atomic::AtomicU64,
  items: std::collections::BTreeMap<u64, DelegatedTask>,
}

impl DelegationSet {
  fn gen_next_delegation_id(&self) -> u64 {
    // `Relaxed` because we don't need any specific ordering, only uniqueness via atomicity
    self
      .next_id
      .fetch_add(1u64, std::sync::atomic::Ordering::Relaxed)
  }

  pub fn attach_new(&mut self, delegated_task: DelegatedTask) -> u64 {
    let next_id = self.gen_next_delegation_id();
    match self.items.insert(next_id, delegated_task) {
      None => next_id,
      Some(_replaced) => unreachable!("Duplicate key registered into delegation set"),
    }
  }

  pub fn detach(&mut self, task_id: &u64) -> Result<DelegatedTask, ()> {
    self.items.remove(task_id).ok_or(())
  }

  pub fn get_outstanding_ids(&self) -> Vec<u64> {
    self.items.keys().cloned().collect()
  }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DelegationError {
  DispatcherDropped,
}

#[derive(Default, Debug)]
pub struct DelegationPool {
  delegations: Mutex<DelegationSet>,
}

pub struct DelegatedReceiver<'a, T: Send> {
  task_id: u64,
  receiver: BoxFuture<'a, Result<T, DelegationError>>,
}

impl<'a, 'b: 'a, T: Send + 'b> std::fmt::Debug for DelegatedReceiver<'a, T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "[DTaskR #{}]", &self.task_id)
  }
}

impl<'a, 'b: 'a, T: Send + 'b> Future for DelegatedReceiver<'a, T> {
  type Output = Result<T, DelegationError>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    self.receiver.poll_unpin(cx)
  }
}

impl DelegationPool {
  pub fn new() -> DelegationPool {
    DelegationPool {
      delegations: Default::default(),
    }
  }

  pub fn delegate<
    'a,
    'b: 'a,
    'c: 'b,
    T: Sized + Send + 'c,
    FutDispatch: futures::future::Future<Output = ()> + Send + 'a,
  >(
    &'b self,
    dispatch: impl (FnOnce(oneshot::Sender<T>) -> FutDispatch) + Send + 'a,
  ) -> impl Future<Output = DelegatedReceiver<'b, T>> + 'a {
    let (dispatcher, promise) = oneshot::channel::<T>();

    async move {
      let task_id = self.delegations.lock().await.attach_new(DelegatedTask {});

      // Fire the `dispatch` closure that must eventually result a value being sent via `dispatcher`
      dispatch(dispatcher).await;

      DelegatedReceiver {
        task_id,
        receiver: async move {
          self.detach(task_id).await;
          promise.map_err(|_| DelegationError::DispatcherDropped).await
        }.boxed()
      }
    }
  }

  pub(super) async fn detach(&self, task_id: u64) -> () {
    let mut lock = self.delegations.lock().await;
    lock
      .detach(&task_id)
      .expect("Value must not be detached by another source");
  }
}

#[cfg(test)]
mod tests {

  #[tokio::test]
  async fn delegation_sync() {
    let pool = super::DelegationPool::new();
    let res = pool
      .delegate(async move |dispatch| {
        dispatch.send(42).unwrap();
      })
      .await
      .await
      .unwrap();
    assert_eq!(res, 42);
  }

  #[tokio::test]
  async fn delegation_async() {
    let pool = super::DelegationPool::new();
    let res = pool
      .delegate(async move |dispatch| {
        tokio::task::yield_now().await;
        dispatch.send(42).unwrap();
      })
      .await
      .await
      .unwrap();
    assert_eq!(res, 42);
  }

  #[tokio::test]
  async fn delegation_externality_prior() {
    let pool = super::DelegationPool::new();
    let mut dispatcher = None;
    let dispatched = {
      let dispatched = pool.delegate(|dispatch| async {
        tokio::task::yield_now().await;
        dispatcher = Some(dispatch);
      });
      assert_eq!(
        pool.delegations.lock().await.get_outstanding_ids().len(),
        0,
        "Must not register until dispatched"
      );
      dispatched
    }
    .await;

    {
      let dispatcher = dispatcher.unwrap();
      dispatcher.send(42).unwrap();
    }

    assert_eq!(
      pool.delegations.lock().await.get_outstanding_ids().len(),
      1,
      "Must register active tasks"
    );

    let res = dispatched.await.unwrap();
    assert_eq!(res, 42);

    assert_eq!(
      pool.delegations.lock().await.get_outstanding_ids().len(),
      0,
      "Must not leak tasks"
    );
  }
}
