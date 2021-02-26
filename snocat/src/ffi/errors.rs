// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use anyhow::Error;
use ffi_support::{ErrorCode, ExternError};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum FfiError {
  Generic(String),
  JsonParseError(serde_json::error::Error),
  HandleError(::ffi_support::HandleError),
  EventingError(String),
}

pub mod error_codes {
  // -1 and 0 are reserved by ffi_support, start at 1
  pub const GENERIC_ERROR: i32 = 1;
  pub const JSON_PARSE_ERROR: i32 = 2;
  pub const HANDLE_ERROR: i32 = 3;
  pub const EVENTING_ERROR: i32 = 4;
}

fn get_code(e: &FfiError) -> ErrorCode {
  match e {
    FfiError::Generic(_) => ErrorCode::new(error_codes::GENERIC_ERROR),
    FfiError::JsonParseError(_) => ErrorCode::new(error_codes::JSON_PARSE_ERROR),
    FfiError::HandleError(_) => ErrorCode::new(error_codes::HANDLE_ERROR),
    FfiError::EventingError(_) => ErrorCode::new(error_codes::EVENTING_ERROR),
  }
}

impl From<FfiError> for ExternError {
  fn from(e: FfiError) -> Self {
    ExternError::new_error(get_code(&e), format!("{:#}", e))
  }
}

impl PartialEq for FfiError {
  fn eq(&self, other: &Self) -> bool {
    get_code(self) == get_code(other)
  }
}

impl std::error::Error for FfiError {}

impl From<anyhow::Error> for FfiError {
  fn from(e: anyhow::Error) -> Self {
    FfiError::Generic(format!("{:#}", e))
  }
}

impl From<serde_json::Error> for FfiError {
  fn from(e: serde_json::Error) -> Self {
    FfiError::JsonParseError(e)
  }
}

impl From<::ffi_support::HandleError> for FfiError {
  fn from(e: ::ffi_support::HandleError) -> Self {
    FfiError::HandleError(e)
  }
}

impl From<super::eventing::EventingError> for FfiError {
  fn from(e: super::eventing::EventingError) -> Self {
    FfiError::EventingError(format!("{:#}", e))
  }
}

impl std::fmt::Display for FfiError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      FfiError::Generic(s) => f.write_str(&s),
      FfiError::HandleError(e) => std::fmt::Debug::fmt(e, f),
      FfiError::EventingError(s) => f.write_str(&s),
      other => std::fmt::Debug::fmt(other, f),
    }
  }
}
