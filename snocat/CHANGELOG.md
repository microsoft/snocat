# Change Log
The format is based on [Keep a Changelog](http://keepachangelog.com/).
This project will adhere to [Semantic Versioning](http://semver.org/),
following the release of version 1.0.0.

<!-- next-header -->

## [Unreleased] - ReleaseDate

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
[Unreleased]: https://github.com/Microsoft/snocat/compare/snocat-v0.1.2...HEAD
[0.1.2]: https://github.com/Microsoft/snocat/compare/v0.1.1...snocat-v0.1.2
[0.1.1]: https://github.com/microsoft/snocat/compare/855fc4beacf4f568a08e848193fba65e6e840fd1...v0.1.1
[0.1.0]: https://github.com/microsoft/snocat/compare/b8d28e83c0bf7010d86eaddcdd212fe72848f6bb...855fc4beacf4f568a08e848193fba65e6e840fd1

