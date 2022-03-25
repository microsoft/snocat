# Change Log
The format is based on [Keep a Changelog](http://keepachangelog.com/).
This project will adhere to [Semantic Versioning](http://semver.org/),
following the release of version 1.0.0.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Hybrid Eventually-Consistent Tunnel Repositories (0.6.0-alpha.1)
Tunnels are now considered "snocat tunnels" immediately after connection,
and their metadata is now an immutable `HashMap<String, Vec<u8>>`.
The "Baggage" trait still exists, but is deprecated for future removal.

`ModularDaemon` now provides tokio::sync::Broadcast-based event subscriptions
which can be used for monitoring of tunnel lifecycles.
These are not guaranteed-reliable- any listeners that lag too far behind may
miss a message if the buffer fills before they are processed; Such misses will
notify with a "Lagged" error on the receiver-side. To avoid this, ensure that a
task processes messages as soon as possible and pushes them into a work queue
as-needed with minimal processing, and that work is handled on a separate task.

Tunnels are now registered to the provided `TunnelRegistry` only after they have
authenticated successfully.
Tunnels which fail authentication are never considered "registered", and thus
never enter the registry or process requests. This is a change from the prior
model which allowed a tunnel's registry record to be mutated post-write.

Mutation is allowed via the `SharedAttributeRegistry` trait but no provided
reference registry implements this functionality. A CRDT-based table could
feasibly allow distributed, eventually consistent mutability of attributes
in the future, but the design is in flux.

#### Reduction of Registry content
Tunnel Registries now purely track externally-viewable tunnel state, instead of
tracking a local-and-remote set of items; Stored entries are allowed to be out-of-date,
which reduces consistency requirements to allow for use of non-transactional databases.

A Redis implementation is provided which allows for "last-named-wins" semantics of tunnel
registration via feature flag `redis-store`.
This may be placed behind caching layers to allow for reduced registrations, but the
provided cache - recursively defined as a registry composing two others - lacks some
ID semantics needed for proper, timely cleanup of cached resource instances.
Integration tests can be run with `./.scripts/run-integration-tests.sh` or providing
`integration-redis` as a feature flag during test execution.

An in-memory implementation based upon DashMap is provided which has the same
last-entry-wins semantics as the Redis implementation in order to facilitate
database mocking in integration testing.

#### Daemon Peer Tracking
A new, guaranteed-locally-consistent in-memory store of tunnels now handles
tunnels active within the current daemon context.
This is exposed as a read-only view via the `ModularDaemon::peers(&self)` method,
with the implementation mechanism hidden behind a set of accessors on `PeersView`.
Tunnels are tracked only after name registration, and are inserted or removed
prior to the asynchronous calls for Registry updates.
Tunnels which fail authentication never reach a "registered" state and are
thus never included in the peer tracker nor the `TunnelRegistry`.


#### Inner Loop Simplification
Tunnel lifecycle handling code within `ModularDaemon` has been dramatically simplified:
Generic `TTunnel` on ModularDaemon has been moved to a generic parameter upon the
`ModularDaemon::run` method, and internal functions are parameterized upon it.
An RAII wrapper utilizing `DropKick` now provides deregistration and closure of tunnels
instead of intercepting potential exit points, as
[AsyncDrop](https://rust-lang.github.io/async-fundamentals-initiative/roadmap/async_drop.html)
is still in the preliminary planning stages by the associated Rust initiative.
This allows us to encode deregistration logic within the wrapper type, ensuring
that the daemon does not miss a path so long as the compiler itself is correct.

#### Service and ServiceRegistry modifications

`Service` and `ServiceRegistry` now work with instances of (or references to) `ArcTunnel` in lieu of `TunnelId`.

`MappedService` is now a `struct` instead of a `trait`, and acts as a wrapper type which implements
`Service` by mapping the result to a phantom type stored on the instance. Remapping from one mapped
type to another can be performed with `MappedService::new(MappedService::into_inner(s))`.

`Service` is now implemented for the following reference types:
- `&S`
- `&mut S`
- `Box<S>`
- `Rc<S>`
- `Arc<S>`

## [0.5.0-alpha.6] - 2022-03-04

### Generic Tunnel Registries and Services
The majority of `ModularDaemon` module types have switched from dynamic to generic dispatch.

If you still require dynamic dispatch, you may continue to use it, but associated types
will need to be specified depending on which module is being implemented, though effort
has been made to allow boxing of the `TunnelRegistry`'s associated types via `Any`.

#### Associated Errors

This genericization change also comes with benefits to library users' error handling:

Several types now include `Error` associated types, which allow specification of
implementation-specific error types for the various modules.

`Service` is now accessed via `MappedService`, which handles translation from
`Service`-specific `Error` types to those known by an owning `ServiceRegistry`.

This allows `Service` instances to- for example- use `thiserror` to describe internal
failure classes, while an application's `ServiceRegistry` could use `anyhow::Error`,
automatically translating to the registry's error types in the process.

The implementor of the `ServiceRegistry` could, alternatively, make an error type
specific to their implementation, and implement `From` for the `Error` types of
each service they expect to support.

General "mapping" behaviour is intended to be supported later, but trait changes will
likely follow, so avoid tight coupling with the current structure of `MappedService`.

#### `TunnelRegistry` behavioural changes

Tunnel Registries can now track tunnels to which they do not have reference access.
This is meant to allow for registries which span multiple processes or machines,
wherein a Tunnel can be known but not owned by the current registry instance.

A registry should, in general, only have access to - or alter - tunnels it owns.

### Tunnel unification refactoring
Tunnel is now a hybrid trait of uplink and downlink.
`TunnelUplink` and `TunnelDownlink` are traits, and sidedness is split
off into its own trait. The hybrid tunnel allows any number of accesses
to the incoming stream, but they are exclusive, and will await an async
Mutex- possibly until the tunnel in question has been closed.

Tunnel traits now exist which specify behaviour for monitoring link
closure events, and a means of forcing closure of a tunnel on command.
This behaviour is currently only implemented for `QuinnTunnel`.

Tunnels implement a new "Baggage" trait which allows attachment of arbitrary
data to tunnel types, shared between all handles to that tunnel instance.
This data is not networked, and is dropped with the last tunnel handle.

_It is strictly illegal to store a tunnel handle in any tunnel's baggage_.

### Daemon System
Revamped networking system to use a `ModularDaemon` for both client and server management.

The Protocol is now P2P-capable and uses connection sidedness rather than server or client identity.

### Miscellaneous

- `Dropkick` utility added for drop notifications.
- Dependency `triggered` dropped in favor of `tokio_util::sync::CancellationToken`.
- `ModularDaemon` has been moved from `snocat::server::modular` to `snocat::common::daemon`.


## [0.1.2] - 2021-03-01
Add release and changelog management mechanisms

## [0.1.1] - 2021-03-01
Add crate repository metadata

## [0.1.0] - 2021-02-26
Initial release

<!-- next-url -->
[Unreleased]: https://github.com/Microsoft/snocat/compare/snocat-v0.5.0-alpha.6...HEAD
[0.5.0-alpha.6]: https://github.com/Microsoft/snocat/compare/snocat-v0.1.2...snocat-v0.5.0-alpha.6
[0.1.2]: https://github.com/Microsoft/snocat/compare/v0.1.1...snocat-v0.1.2
[0.1.1]: https://github.com/microsoft/snocat/compare/855fc4beacf4f568a08e848193fba65e6e840fd1...v0.1.1
[0.1.0]: https://github.com/microsoft/snocat/compare/b8d28e83c0bf7010d86eaddcdd212fe72848f6bb...855fc4beacf4f568a08e848193fba65e6e840fd1

