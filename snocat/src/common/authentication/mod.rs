// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! Types supporting authentication of Snocat tunnel connections
#[deny(unused_imports)]
mod traits;
pub use traits::*;

mod no_op_authentication;
pub use no_op_authentication::NoOpAuthenticationHandler;

mod simple_ack_authentication;
pub use simple_ack_authentication::SimpleAckAuthenticationHandler;
