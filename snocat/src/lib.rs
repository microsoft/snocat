// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#![feature(async_closure)]
#![feature(backtrace)]
#![feature(drain_filter)]
#![feature(label_break_value)]
#![feature(never_type)]
#![feature(try_blocks)]
#![feature(type_ascription)]
// Move GATs to the above upon completion of https://github.com/rust-lang/rust/pull/84623
#![allow(incomplete_features)]
#![feature(generic_associated_types)]
#![allow(dead_code)]
#![allow(unused_imports)]

pub mod common;
pub mod util;

pub mod client;
pub mod server;
