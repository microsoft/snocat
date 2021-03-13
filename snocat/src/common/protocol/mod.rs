// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
//! Types for handling routing of Snocat bidirectional streams to stream consumers

// Lifetime stages:
//
// - ("Caller" side)
// -
// - receive connections from user
// - transform connections into (type, headers, stream)
// - - other protocols provide (type, parameters); "parameters" are owned (static) and may include streams
// - determine which tunnel to use (should this be before typification, or after?)
// - (Tunnel)
// - receive (type, headers, stream) tuples
// - choose which routing protocol to apply based on type; pass it (headers, stream)
// - - protocol perform connection to the routing-protocol-chosen target; protocol chooses how to use stream
// -
// - (Server / "broker" side)
//
// "Type" is a multiaddr
//
// Note that authentication doesn't occur on this level- this is *after* tunnel establishment

pub mod traits;
pub use traits::{
  Client, ClientError, DynamicResponseClient, Request, Response, RouteAddress, Router,
  RoutingError, Service, ServiceError,
};

pub mod negotiation;
pub mod proxy_tcp;
pub mod tunnel;
