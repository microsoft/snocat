// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! Types for building an Snocat server and accepting, authenticating, and routing connections
#![warn(unused_imports)]
use futures::future::FutureExt;
use std::collections::HashSet;
use std::{ops::RangeInclusive, sync::Arc};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct PortRangeAllocator {
  range: std::ops::RangeInclusive<u16>,
  allocated: Arc<Mutex<std::collections::HashSet<u16>>>,
  mark_queue: tokio::sync::mpsc::UnboundedSender<u16>,
  // UnboundedReceiver does not implement clone, so we need an ArcMut of it
  mark_receiver: Arc<Mutex<tokio::sync::mpsc::UnboundedReceiver<u16>>>,
}

#[derive(thiserror::Error, Debug)]
pub enum PortRangeAllocationError {
  #[error("No ports were available to be allocated in range {0:?}")]
  NoFreePorts(std::ops::RangeInclusive<u16>),
}

impl PortRangeAllocator {
  pub fn new<T: Into<u16>>(bind_port_range: std::ops::RangeInclusive<T>) -> PortRangeAllocator {
    let (start, end): (u16, u16) = {
      let (a, b) = bind_port_range.into_inner();
      (a.into(), b.into())
    };
    let (mark_sender, mark_receiver) = tokio::sync::mpsc::unbounded_channel();
    PortRangeAllocator {
      range: std::ops::RangeInclusive::new(start, end),
      allocated: Default::default(),
      mark_queue: mark_sender,
      mark_receiver: Arc::new(Mutex::new(mark_receiver)),
    }
  }

  pub async fn allocate(&self) -> Result<PortRangeAllocationHandle, PortRangeAllocationError> {
    // Used for cleaning up in the deallocator
    let cloned_self = self.clone();
    let range = self.range.clone();
    let mark_receiver = Arc::clone(&self.mark_receiver);
    let mut lock = self.allocated.lock().await;

    // Consume existing marks for freed ports
    {
      let mut mark_receiver = mark_receiver.lock().await;
      Self::cleanup_freed_ports(&mut *lock, &mut *mark_receiver);
    }

    let port = range
      .clone()
      .into_iter()
      .filter(|test_port| !lock.contains(test_port))
      .min()
      .ok_or_else(|| PortRangeAllocationError::NoFreePorts(range.clone()))?;

    let allocation = PortRangeAllocationHandle::new(port, cloned_self);
    lock.insert(allocation.port);
    Ok(allocation)
  }

  pub async fn free(&self, port: u16) -> Result<bool, anyhow::Error> {
    let mark_receiver = Arc::clone(&self.mark_receiver);
    let mut lock = self.allocated.lock().await;
    let removed = lock.remove(&port);
    if removed {
      tracing::trace!(port = port, "unbound port");
    }
    let mut mark_receiver = mark_receiver.lock().await;
    Self::cleanup_freed_ports(&mut *lock, &mut *mark_receiver);
    Ok(removed)
  }

  fn cleanup_freed_ports(
    allocations: &mut HashSet<u16>,
    mark_receiver: &mut tokio::sync::mpsc::UnboundedReceiver<u16>,
  ) {
    // recv waits forever if a sender can still produce values
    // skip that by only receiving those immediately available
    // HACK: Relies on unbounded receivers being immediately available without intermediate polling
    while let Some(Some(marked)) = mark_receiver.recv().now_or_never() {
      let removed = allocations.remove(&marked);
      if removed {
        tracing::trace!(port = marked, "unbound marked port");
      }
    }
  }

  pub fn mark_freed(&self, port: u16) {
    match self.allocated.try_lock() {
      // fast path if synchronous is possible, skipping the mark queue
      Ok(mut allocations) => {
        // remove specified port; we don't care if it succeeded or not
        let _removed = allocations.remove(&port);
        return;
      }
      Err(_would_block) => {
        match self.mark_queue.send(port) {
          // Message queued, do nothing
          Ok(()) => (),
          // Other side was closed
          // Without a receiver, we don't actually need to free anything, so do nothing
          Err(_send_error) => (),
        }
      }
    }
  }

  pub fn range(&self) -> &RangeInclusive<u16> {
    &self.range
  }
}

pub struct PortRangeAllocationHandle {
  port: u16,
  allocated_in: Option<PortRangeAllocator>,
}

impl PortRangeAllocationHandle {
  pub fn new(port: u16, allocated_in: PortRangeAllocator) -> Self {
    Self {
      port,
      allocated_in: Some(allocated_in),
    }
  }
  pub fn port(&self) -> u16 {
    self.port
  }
}

impl Drop for PortRangeAllocationHandle {
  fn drop(&mut self) {
    match std::mem::replace(&mut self.allocated_in, None) {
      None => (),
      Some(allocator) => {
        allocator.mark_freed(self.port);
      }
    }
  }
}
