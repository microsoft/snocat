// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use snocat::common::protocol::{tunnel::TunnelId, MappedService, RouteAddress, ServiceRegistry};
use std::{
  fmt::{Debug, Display},
  sync::Arc,
};

pub mod demand_proxy;

pub struct PresetServiceRegistry<TServiceError> {
  pub services: std::sync::RwLock<Vec<Arc<dyn MappedService<TServiceError> + Send + Sync>>>,
}

impl<TServiceError> PresetServiceRegistry<TServiceError> {
  pub fn new() -> Self {
    Self {
      services: std::sync::RwLock::new(Vec::new()),
    }
  }

  pub fn add_service_blocking<TService>(&self, service: Arc<TService>)
  where
    TServiceError: Debug + Display,
    TService: MappedService<TServiceError> + Send + Sync + 'static,
  {
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
  ) -> Option<std::sync::Arc<dyn MappedService<TServiceError> + Send + Sync>> {
    self
      .services
      .read()
      .expect("Service registry lock poisoned")
      .iter()
      .find(|s| s.accepts_mapped(addr, tunnel_id))
      .map(Arc::clone)
  }
}
