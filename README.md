# SNOCAT

_Streaming Network Overlay Connection Arbitration Tunnel_

[![Crates.io](https://img.shields.io/crates/v/snocat)](https://crates.io/crates/snocat)
[![docs.rs](https://img.shields.io/docsrs/snocat)](https://docs.rs/snocat/0.1.0/snocat/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE-APACHE)

`snocat` is a library and toolkit for TCP reverse tunnelling over the
[QUIC protocol](https://quicwg.org/).

When used as a library, [`libsnocat`](https://crates.io/crates/snocat)
allows a dynamic set of `client`s to connect over UDP to a `server`,
and forwards TCP streams from the `server` to the `client`s.

As a command-line tool, [`snocat-cli`](https://crates.io/crates/snocat-cli) exists to allow
small-scale port redirection akin to SSH Remote Forwarding.

## Third-Party Dependencies

Primary crates used include the [Tokio stack](https://tokio.rs/) and
[futures-rs](https://rust-lang.github.io/futures-rs/) for async-await capabilities,
[Quinn](https://github.com/quinn-rs/quinn) for its [QUIC](https://quicwg.org/)
implementation.

Various other dependencies are included under their respective licenses, and may
be found in [snocat/Cargo.toml](snocat/Cargo.toml) and
[snocat-cli/Cargo.toml](snocat-cli/Cargo.toml).

Notable exceptions from *MIT* or *MIT OR Apache 2.0* licensing in dependencies are the following crates:
- `ring` for TLS, distributed under a BoringSSL-variant, *ISC-style* permissive license
- `untrusted` required by `ring` for parsing of TLS, distributed under an *ISC-style* permissive license
- `webpki` for TLS WebPKI certificate handling, distributed under an *ISC-style* permissive license
- `memchr`, `byteorder`, `regex-automata` are licensed under *Unlicense OR MIT*
- `prost`, `prost-types`, `prost-derive`, and `prost-build`, licensed solely under the *Apache-2.0* license
- `ryu` required by `serde_json` for floating point parsing from json, licensed under *Apache-2.0* OR *BSL-1.0*

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
