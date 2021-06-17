# Change Log
The format is based on [Keep a Changelog](http://keepachangelog.com/).
This project will adhere to [Semantic Versioning](http://semver.org/),
following the release of version 1.0.0.

<!-- next-header -->

## [Unreleased] - ReleaseDate
### Tunnel unification refactoring
Tunnel is now a hybrid trait of uplink and downlink.
`TunnelUplink` and `TunnelDownlink` are traits, and sidedness is split
off into its own trait. The hybrid tunnel allows any number of accesses
to the incoming stream, but they are exclusive, and will await an async
Mutex- possibly until the tunnel in question has been closed.

Future revisions may include an API to split tunnels, and will
include control operations and event monitoring to allow connections
to be closed on command, or observed for current and future state.

### Daemon System
Revamped networking system to use a `ModularDaemon` for both client and server management.

The Protocol is now P2P-capable and uses connection sidedness rather than server or client identity.

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

