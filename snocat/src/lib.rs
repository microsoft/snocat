// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![cfg_attr(test, feature(assert_matches))]
#![feature(async_closure)]
#![feature(backtrace)]
#![feature(generic_associated_types)]
#![feature(label_break_value)]
#![feature(never_type)]
#![feature(try_blocks)]
#![feature(type_ascription)]
#![allow(dead_code)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::unused_unit)]

pub mod common;
pub mod util;

pub mod client;
pub mod server;
