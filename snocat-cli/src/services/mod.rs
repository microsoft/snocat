// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use snocat::common::protocol::{
  tunnel::ArcTunnel, MappedService, RouteAddress, Service, ServiceRegistry,
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

  pub fn add_service_blocking<TService>(&self, service: Arc<TService>)
  where
    TService: Service + Send + Sync + 'static,
    TServiceError: From<<TService as Service>::Error> + Debug + Display + Send + Sync + 'static,
  {
    self
      .services
      .write()
      .expect("Service registry lock poisoned")
      .push(Arc::new(MappedService::new(service)) as Arc<_>);
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
    tunnel: &ArcTunnel,
  ) -> Option<std::sync::Arc<dyn Service<Error = TServiceError> + Send + Sync>> {
    self
      .services
      .read()
      .expect("Service registry lock poisoned")
      .iter()
      .find(|s| s.accepts(addr, tunnel))
      .map(Arc::clone)
  }
}
