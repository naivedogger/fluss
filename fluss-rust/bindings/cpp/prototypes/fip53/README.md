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

# FIP-53 Rust/C++ client prototype

**Experimental and not usable against a real Fluss cluster.**
Baseline: FIP-53 wiki version 16 (September 1, 2026), inspected October 8, 2026.
Reference: https://cwiki.apache.org/confluence/spaces/FLUSS/pages/449282458

## Scope

This implements the **manual assignment + explicit offset restore/commit** slice,
not a complete consumer group. Bucket ownership must be supplied externally.
There is no external offset database, local-file persistence or client-to-client
communication. Persistence belongs to the future server adapter.

```text
C++ experimental GroupOffsetClient
              |
          existing cxx bridge
              |
Rust GroupOffsetClient (validation / lifecycle / partial outcomes)
              |
        GroupService trait
              +-- explicit in-process mock (demo/tests only)
              +-- UnavailableGroupService (live path fails closed)
              +-- future native Fluss RPC adapter (not implemented)
```

The core is in `crates/fluss/src/client/group_offsets_prototype.rs`; the bridge is
in `bindings/cpp/src/group_offsets_prototype.rs`. `group_offsets.hpp` is an
uninstalled experimental wrapper, separate from the stable `fluss.hpp`.
No dependency, existing build configuration, protobuf or RPC API key is changed.
The ordinary SDK never selects a mock.

## Implemented contract

- Stable group name plus full table/optional-partition/bucket identity.
- One fixed manual assignment per client lifetime.
- Restore returns start positions for an existing scanner. Missing positions
  either fail or explicitly return the scanner's `-2` earliest sentinel.
- A service error, malformed response or missing response entry is NOT absence
  of a saved position and must never trigger an earliest fallback.
- Explicit commits contain **next offsets for contiguous completed work**.
  Nothing automatically commits fetched data, background timer progress, on
  close, or on destruction. The SDK cannot prove business processing completed.
- Each committed bucket reports its own result. Only acknowledged successes
  update locally confirmed progress. A partial error is not all-or-nothing.
- Empty/duplicate/unknown bucket commits, negative positions and regressions
  within one client lifetime are rejected before IO. Administrative reset is a
  separate future operation, not `max(old, new)` applied by the server.
- Rust requires `&mut self`; C++ callers must serialize access to the same
  wrapper. Call ordering on this client does not fence other processes.
- Unknown commit outcomes, cancellation and fencing fail closed and prohibit
  more commits on that client. No blind retry is attempted. Recovery requires
  a server-side ordering/fencing contract or externally established quiescence;
  merely creating a new client and fetching offsets is NOT proof that an old
  request cannot arrive later.
- The exception-based C++ request-error API is provisional. Per-bucket outcomes
  have prototype-local statuses, not guessed server error-code values.

## Reproduce

Run from the repository's `fluss-rust` directory. The existing workspace
dependencies and toolchain are sufficient; no Arrow C++ installation is needed
for this control-path demo.

```sh
# macOS: use the same deployment target for Rust's native dependencies and C++.
export MACOSX_DEPLOYMENT_TARGET="$(sw_vers -productVersion)"
cargo test --locked -p fluss-rs --lib group_offsets_prototype
cargo build --locked -p fluss-cpp

TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
c++ -std=c++17 -Wall -Wextra -Werror \
  -I "$TARGET_DIR/cxxbridge" \
  -I bindings/cpp/include \
  bindings/cpp/prototypes/fip53/demo.cpp \
  "$TARGET_DIR/debug/libfluss_cpp.a" \
  -o "$TARGET_DIR/fip53-demo" \
  -framework Security -framework SystemConfiguration -framework CoreFoundation \
  -liconv -lresolv
"$TARGET_DIR/fip53-demo"
```

The link command above is for macOS. On Linux use the toolchain's Rust native
static-library link requirements (normally `-ldl -lpthread -lm` instead of the
framework flags). A custom relative CARGO_TARGET_DIR must be resolved relative to
the directory from which Cargo was invoked.

The executable checks the actual C++ -> cxx -> Rust -> mock path. It destroys and
recreates a **client object** while retaining mock server state. It does NOT
restart the process: the mock is volatile and loses all data with the executable.
There is no live Fluss data polling in this demo. No results demonstrate real
server replication, durability, compatibility or failover.
The `RestoreInto` handoff is exercised with a scanner double; it installs the
resolved positions through the SDK's existing subscription method signatures.
The two real SDK scanner template instantiations can also be syntax-checked.

## Connecting returned offsets to an existing scanner

`RestoreInto` installs the resolved positions into a fresh scanner and closes
the group client if any subscription fails. The caller must then discard that
partially initialized scanner. The handoff is intentionally explicit:

```cpp
// Sketch, not a working live-service example: the live RPC adapter is absent.
// Use a fresh scanner for this fixed table and assignment, and its actual table ID.
auto starts = group.RestoreInto(scanner, table_id, assigned_buckets,
                                MissingOffsetPolicy::Fail);
// Poll using existing SDK APIs. After processing a contiguous range successfully:
auto outcomes = group.CommitSync(processed_next_offsets);
// Check EVERY outcome, not merely that CommitSync returned without throwing.
```

A single table scanner cannot consume an assignment spanning multiple tables;
route positions to the matching scanners. Both record and Arrow-batch consumers
can use the offset API. This prototype does not implement batch completion
tracking or infer progress from row counts/filter results. For partially completed
batches or out-of-order tasks the application must not commit past unfinished work.

## Deliberately deferred

- Real FindGroupCoordinator / OffsetFetch / OffsetCommit RPC adapter.
- JoinGroup, SyncGroup, heartbeat, leave, assignor and rebalance callbacks.
- Multi-process exclusivity, static membership and ownership fencing.
- Auto-commit and public async commit.
- Full admin APIs, offset reset/expiry policy, latest/timestamp fallback resolution.
- Transparent scanner ownership and delivery/completion tracking.
- Exactly-once output/offset transactions.
- Stable/public C++ API and packaging, server-backed integration/failure tests.

Before the real adapter lands, agree with the FIP author on shared protobufs,
API keys, error codes, no-member/manual commit identity, missing-offset encoding,
commit timeout/ordering/partial-failure semantics and capability negotiation.
Before group coordination, also agree on subscription/assignment byte encoding,
client-vs-server assignment and revocation behavior. The current prototype neither
claims the FIP is accepted nor freezes any of these details.
