// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![cfg_attr(test, feature(assert_matches))]
#![feature(async_closure)]
#![feature(backtrace)]
#![feature(generic_associated_types)]
#![feature(never_type)]
#![feature(try_blocks)]
#![feature(type_ascription)]
// Some of these are incorrect with regards to exposed behaviour, especially around
// exported traits which may require extra lifetimes at implementation time, or where
// a boxed future to a dyn result is valid for a different lifetime than its dyn component.
// GATs also present complications wherein this lint has not yet caught up to all nuance.
#![allow(clippy::needless_lifetimes)]
// Codebase policy prefers an explicit unit return when it clarifies intent,
// for consistency with Ok(()) returns at the end of Result-bearing functions.
#![allow(clippy::unused_unit)]

pub mod common;
pub mod util;

pub mod client;
pub mod server;
