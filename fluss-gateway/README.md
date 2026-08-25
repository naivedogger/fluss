<!--
 Licensed to the Apache Software Foundation (ASF) under one
 or more contributor license agreements.  See the NOTICE file
 distributed with this work for additional information
 regarding copyright ownership.  The ASF licenses this file
 to you under the Apache License, Version 2.0 (the
 "License"); you may not use this file except in compliance
 with the License.  You may obtain a copy of the License at

     http://www.apache.org/licenses/LICENSE-2.0

 Unless required by applicable law or agreed to in writing,
 software distributed under the License is distributed on an
 "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 KIND, either express or implied.  See the License for the
 specific language governing permissions and limitations
 under the License.
-->

# Apache Fluss Gateway

A stateless REST gateway for Apache Fluss. It exposes REST APIs for writing to
Fluss tables and performing DDL operations, while keeping no session, cursor,
or replay state: any instance can serve any request behind a plain load
balancer.

The gateway is an executable, not a library on crates.io, and it is its own
Cargo workspace so its dependencies never touch the `fluss-rust` workspace's
lock file or its generated dependency inventories.

## Status

This module currently contains only the scaffolding: the manifest with
placeholder bin/lib targets, the pinned toolchain, lint and license
configuration, and the developer recipes. The runtime — configuration,
lifecycle management, the REST layer, and the test suites — lands in follow-up
pull requests.

## Distribution and container

The Linux convenience distribution uses the same layout as the Java Fluss
distribution:

```text
fluss-gateway-<version>-bin-linux-<arch>/
├── bin/
├── conf/
├── openapi.yaml
├── DEPENDENCIES.rust.tsv
├── LICENSE
└── NOTICE
```

Create it from the repository `tools/` directory:

```bash
RELEASE_VERSION=1.0.0 SKIP_GPG=true releasing/create_gateway_release.sh
```

The release script uses `docker/fluss-gateway/Dockerfile.build` to pin Rust
1.88 and Debian Bookworm as the Linux build environment. It builds the host
architecture by default; set `GATEWAY_ARCH=amd64` or
`GATEWAY_ARCH=arm64` only when the selected buildx node is native for that
platform.

After extracting the archive, edit `conf/gateway.yaml` and start the foreground
process:

```bash
bin/fluss-gateway.sh
```

The wrapper resolves `FLUSS_HOME` from its own location, uses
`conf/gateway.yaml` by default, and forwards additional CLI options to the
binary. The convenience distribution follows the Java distribution and binds
listeners to loopback by default. `gateway-healthcheck.sh` requires `curl`.
Set `RUST_LOG=debug` when temporary diagnostic logging is needed.

The container image also installs into `/opt/fluss`, uses the `fluss` user with
UID/GID 9999, and reads `/opt/fluss/conf/gateway.yaml`. The image is assembled
from the prepared binary distribution, matching the Java image's
`build-target` flow. Typed environment defaults bind its REST and Prometheus
listeners to `0.0.0.0` without modifying the packaged configuration.
See the [Gateway image README](../docker/fluss-gateway/README.md).

## Prerequisites

- Rust toolchain managed by [rustup](https://rustup.rs); the workspace pins the
  channel in `rust-toolchain.toml` (stable with `rustfmt` and `clippy`)
- The declared minimum supported Rust version is 1.88, enforced by the
  `gateway-msrv` CI job
- [`just`](https://github.com/casey/just) for the recipes below

## Build and test

Run everything from this directory, or use the `just` recipes:

```bash
just build        # cargo build --all-targets
just test         # cargo test --all-targets
just fmt-check    # cargo fmt --all -- --check
just clippy       # cargo clippy --all-targets -- -D warnings
just doc          # RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
just licenses     # cargo deny check licenses
just image        # build the Gateway container image
just image-smoke  # run container health and shutdown smoke tests
```

The MSRV can be verified locally with `cargo +1.88.0 check --all-targets`.

## CI

Gateway changes are gated by a dedicated workflow, `.github/workflows/gateway-ci.yml`
(build and tests on Linux/macOS, MSRV 1.88, license headers, dependency
licenses via `cargo-deny`, formatting, clippy, rustdoc). Gateway-only changes
are excluded from the Java CI, mirroring `fluss-rust`, and the gateway workflow
never builds the `fluss-rust` workspace. The container job builds the Gateway
against the in-tree client on native `amd64` and `arm64` runners, packages the
binary distribution, assembles the image from that distribution, and exercises
startup, health, readiness, non-root execution, configuration errors, and
SIGTERM draining.

## License enforcement

Like `fluss-rust`, the gateway carries its own license enforcement: source
headers are checked by `skywalking-eyes` (`.licenserc.yaml`) and dependency
licenses by `cargo-deny` (`deny.toml`). `LICENSE-bin` and `NOTICE-bin` are
generated from the normal Linux runtime dependency closure with
`scripts/generate_binary_license.py`; build, development, and procedural-macro
tooling is excluded because it is not linked into the distributed executable.
CI rejects drift in all three dependency records.
