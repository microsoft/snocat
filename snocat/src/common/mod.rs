// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use serde::{Deserialize, Serialize};

pub mod authentication;
pub mod daemon;
pub mod protocol;
pub mod tunnel_source;

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct MetaStreamHeader {}

impl MetaStreamHeader {
  pub fn new() -> MetaStreamHeader {
    MetaStreamHeader {}
  }
}
