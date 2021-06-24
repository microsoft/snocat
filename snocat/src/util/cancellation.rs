// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use std::ops::Deref;

use futures::future::Future;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

/// A [CancellationToken] that cannot be triggered by its recipient
///
/// Child tokens can be produced from it, allowing sub-cancellation,
/// but the interface does not expose a way to cancel the inner token.
#[derive(Debug, Clone, Default)]
#[repr(transparent)]
pub struct CancellationListener {
  token: CancellationToken,
}

impl CancellationListener {
  pub fn child_token(&self) -> CancellationToken {
    self.token.child_token()
  }

  pub fn is_cancelled(&self) -> bool {
    self.token.is_cancelled()
  }

  pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
    self.token.cancelled()
  }
}

impl From<CancellationToken> for CancellationListener {
  fn from(token: CancellationToken) -> Self {
    Self { token }
  }
}
