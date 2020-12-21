use serde::{Serialize, Deserialize};
use ffi_support::{ErrorCode, ExternError};
use anyhow::Error;

#[derive(Debug)]
pub enum FfiError {
  Generic(String),
  JsonParseError(serde_json::error::Error),
}

pub mod error_codes {
  // -1 and 0 are reserved by ffi_support, start at 1
  pub const GENERIC_ERROR: i32 = 1;
  pub const JSON_PARSE_ERROR: i32 = 2;
}

fn get_code(e: &FfiError) -> ErrorCode {
  match e {
    FfiError::Generic(_) => ErrorCode::new(error_codes::GENERIC_ERROR),
    FfiError::JsonParseError(_) => ErrorCode::new(error_codes::JSON_PARSE_ERROR),
  }
}

impl From<FfiError> for ExternError {
  fn from(e: FfiError) -> Self {
    ExternError::new_error(get_code(&e), format!("{:?}", e))
  }
}

impl PartialEq for FfiError {
  fn eq(&self, other: &Self) -> bool {
    get_code(self) == get_code(other)
  }
}

impl From<anyhow::Error> for FfiError {
  fn from(e: Error) -> Self {
    FfiError::Generic(e.to_string())
  }
}

impl From<serde_json::Error> for FfiError {
  fn from(e: serde_json::Error) -> Self {
    FfiError::JsonParseError(e)
  }
}
