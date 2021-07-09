// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use snocat::{
  common::protocol::traits::ServiceRegistry,
  common::protocol::{tunnel::TunnelId, RouteAddress, Service},
};
use std::{
  fmt::{Debug, Display},
  sync::Arc,
};

pub mod demand_proxy;

pub struct PresetServiceRegistry<TServiceError> {
  pub services: std::sync::RwLock<Vec<Arc<dyn Service<Error = TServiceError> + Send + Sync>>>,
}

impl<TServiceError> PresetServiceRegistry<TServiceError> {
  pub fn new() -> Self {
    Self {
      services: std::sync::RwLock::new(Vec::new()),
    }
  }

  pub fn add_service_blocking(
    &self,
    service: Arc<dyn Service<Error = TServiceError> + Send + Sync + 'static>,
  ) {
    self
      .services
      .write()
      .expect("Service registry lock poisoned")
      .push(service);
  }
}

impl<TServiceError> ServiceRegistry for PresetServiceRegistry<TServiceError>
where
  TServiceError: Debug + Display,
{
  type Error = TServiceError;

  fn find_service(
    self: std::sync::Arc<Self>,
    addr: &RouteAddress,
    tunnel_id: &TunnelId,
  ) -> Option<std::sync::Arc<dyn Service<Error = TServiceError> + Send + Sync>> {
    self
      .services
      .read()
      .expect("Service registry lock poisoned")
      .iter()
      .find(|s| s.accepts(addr, tunnel_id))
      .map(Arc::clone)
  }
}
