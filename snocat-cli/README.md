# SNOCAT-CLI

_Streaming Network Overlay Connection Arbitration Tunnel_

[![Crates.io](https://img.shields.io/crates/v/snocat-cli)](https://crates.io/crates/snocat-cli)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE-APACHE)

[`snocat-cli`](https://crates.io/crates/snocat-cli) is a command-line tool for TCP reverse
tunnelling over the [QUIC protocol](https://quicwg.org/).
It allows small-scale port redirection akin to SSH Remote Forwarding.

## Usage

Launching a server:
```sh
snocat-cli server \
  --cert $SERVER_CERT_PUB_PEM \
  --key $SERVER_CERT_PRIV_PEM \
  --quic 127.0.0.1:9090 \
  --ports 8080:8090
```

Binding to a server:
```sh
snocat-cli client \
  --authority $AUTHORITY_CERT_PUB_PEM
  --driver localhost:9090 \
  --target $TARGET \
  --san localhost
```

### Certificate Generation

As `QUIC` requires a certificate to operate, `snocat-cli` includes a
tool for self-signed certificate generation, which- while intended for development,
can also be used in production when full `WebPKI` is unnecessary for your use-case.

See `snocat-cli cert --help` for self-signed certificate generation instructions.

`snocat-cli` does not use the system certificate registry to verify certificates,
and only uses the certificate you provide as the authority.

Note that the authority can be a chain, with sequence of signers showing that the
server's chosen cert is trusted by the given authority.

For anything beyond this tool's scope, `openssl` is the de facto solution for
certificate generation and management. `snocat-cli` operates on `PEM` certificates.

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
