# SNOCAT

_Streaming Network Overlay Connection Arbitration Tunnel_

[![Crates.io](https://img.shields.io/crates/v/snocat)](https://crates.io/crates/snocat)
[![docs.rs](https://img.shields.io/docsrs/snocat)](https://docs.rs/snocat)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE-APACHE)

[`snocat`](https://crates.io/crates/snocat) is a framework for forwarding
streams across authenticated, encrypted [QUIC](https://quicwg.org/) tunnels,
from a tunnel aggregator to a dynamic set of clients.

Unlike VPNs, which scale vertically with the number of users
connected to one network, `snocat` is intended to scale horizontally
on the number of networks, allowing dynamic provisioning and selection
of target networks, with a small number of concurrent users per target.

## Usage

> :warning: _This library is under active development, and its API and protocol are extremely unstable_

`libsnocat` allows creation of custom [server](#Custom-Server) and
[client](#Custom-Client) applications atop the `snocat` protocol.

This allows for greater configurability and extensibility
than is possible through `snocat-cli`.

### Custom Server

To create a server:

- Create a `quinn` listen endpoint, referred to as its `driver`
- Implement a `TunnelManager`
  - It doesn't have to be TCP, you can forward whatever you'd like!
- Implement an [AuthenticationHandler](#Authentication-Layer)
- Implement a [Router](#Routing-Layer)
- Instantiate a `TunnelServer` atop your manager
  - Due to the complexity in writing a `TunnelServer` without lifetime or parallelism issues,
    we include a fully asynchronous, stream-oriented implementation,
    [`ConcurrentDeferredTunnelServer`](src/server/deferred.rs),
    but you can build your own if needed.
- Forward connections to your server, and await graceful shutdown

Server lifecycle is stream oriented, using a monadic flow from
incoming connections to connection event outputs.

`Sync` trait-objects act as dispatchers to various functionality,
relying on interior mutability for any state updates within the dispatch handlers.

### Custom Client

To create a client:

- Create a `quinn` connection to your server's `driver`
- Implement an [`AuthenticationClient`](#Authentication-Layer)
- Implement a [`RoutingClient`](#Routing-Layer)
- Forward connections from the connection to your `RoutingClient`,
  and await graceful shutdown or `quinn::endpoint::Endpoint::wait_idle`.

### Authentication Layer

Authentication is handled by implementing the client and server portions of an Authenticator.

The `AuthenticationHandler` trait performs server-side authentication,
while a matching `AuthenticationClient` trait is invoked by the client.

There are plans to allow "by name" dispatch to authenticators in the future,
allowing multiple to be registered with a server or client, so clients and
servers can negotiate a compatible authentication method.

Authentication provides a single, reliable-ordered bidirectional stream,
and either side may close the channel at any time to abort authentication.

### Routing Layer

`Routing` is a term used to describe client handling of a server-provided stream,
or server handling of a client-provided stream.

A Router receives a stream header and a routing identifier.
If a router does not recognize the type identity of the stream header,
it can refuse the stream, which leaves the providing side responsible
for stream closure.

Streams are bidirectional, and a canonical example is `snocat-cli`'s TCP streaming,
which takes a target port and forwards the stream to `localhost` at that port via TCP.

### Protocol

Details of the protocol will be published in [docs/Protocol.md] when it is stabilized.

---

## Development

For debug usage, `SSLKEYLOGFILE` and `RUST_LOG` parameters are supported.

`SSLKEYLOGFILE` allows interception with [Wireshark](https://www.wireshark.org/)
TLS Decryption and QUIC dissection.

For example usage, `snocat-cli` debugging is often performed with a command-line such as the following:

```sh
SSLKEYLOGFILE=~/keylog.ssl.txt RUST_LOG="trace,quinn=warn,quinn_proto=warn" \
  cargo run -- client --authority $SERVER_CERT \
    --driver localhost:9090 \
    --target $TARGET \
    --san localhost
```

See [CONTRIBUTING.md](../CONTRIBUTING.md) in the official project
repository for further development and contribution guidance.

---

## Third-Party Dependencies

Primary crates used include the [Tokio stack](https://tokio.rs/) and
[futures-rs](https://rust-lang.github.io/futures-rs/) for async-await capabilities,
[Quinn](https://github.com/quinn-rs/quinn) for its [QUIC](https://quicwg.org/)
implementation.

Various other dependencies are included under their respective licenses,
and may be found in [Cargo.toml](Cargo.toml).

Notable exceptions from _MIT_ or _MIT OR Apache 2.0_ licensing in dependencies are the following crates:

- `ring` for TLS, distributed under a BoringSSL-variant, _ISC-style_ permissive license
- `untrusted` required by `ring` for parsing of TLS, distributed under an _ISC-style_ permissive license
- `webpki` for TLS WebPKI certificate handling, distributed under an _ISC-style_ permissive license
- `memchr`, `byteorder`, `regex-automata` are licensed under _Unlicense OR MIT_
- `prost`, `prost-types`, `prost-derive`, and `prost-build`, licensed solely under the _Apache-2.0_ license
- `ryu` required by `serde_json` for floating point parsing from json, licensed under _Apache-2.0_ OR _BSL-1.0_

See [NOTICE.md](NOTICE.md) for license details of individual crates,
and links to their project webpages.

## Trademarks

This project may contain trademarks or logos for projects, products, or services.
Authorized use of Microsoft trademarks or logos is subject to and must follow
[Microsoft's Trademark & Brand Guidelines](https://www.microsoft.com/en-us/legal/intellectualproperty/trademarks/usage/general).
Use of Microsoft trademarks or logos in modified versions of this project must not
cause confusion or imply Microsoft sponsorship. Any use of third-party trademarks
or logos are subject to those third-party's policies.

## Code of Conduct

This project has adopted the [Microsoft Open Source Code of Conduct](https://opensource.microsoft.com/codeofconduct/).
For more information see the [Code of Conduct FAQ](https://opensource.microsoft.com/codeofconduct/faq/)
or contact [opencode@microsoft.com](mailto:opencode@microsoft.com) with any additional questions or comments.

## License

Copyright (c) Microsoft Corporation. All rights reserved.

Licensed under either of

- Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license
  ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contribution

Under the Contributor License Agreement, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
