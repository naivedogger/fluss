---
sidebar_position: 4
---

<!--
 Licensed to the Apache Software Foundation (ASF) under one
 or more contributor license agreements. See the NOTICE file
 distributed with this work for additional information
 regarding copyright ownership. The ASF licenses this file
 to you under the Apache License, Version 2.0 (the
 "License"); you may not use this file except in compliance
 with the License. You may obtain a copy of the License at

   http://www.apache.org/licenses/LICENSE-2.0

 Unless required by applicable law or agreed to in writing,
 software distributed under the License is distributed on an
 "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 KIND, either express or implied. See the License for the
 specific language governing permissions and limitations
 under the License.
-->

# Preparing Clients for a Fluss Release

The Rust client and its Python, C++ and Elixir bindings share the main Fluss
release version and Git tag. There is **no separate client source release**.
The authoritative process for release branches, signing, staging, voting and
publication is [Creating a Fluss Release](https://fluss.apache.org/community/how-to-release/creating-a-fluss-release/).
This page covers client preparation within that process, not a second release
procedure. Do not use the historical standalone-repository release instructions.

## Tools and boundaries

| Tool | Responsibility |
| --- | --- |
| `scripts/bump-version.sh CURRENT NEXT` | Update the Rust workspace version, its `fluss` self-dependency and the Elixir application's `@version` |
| `scripts/release_version.py check --tag TAG` | Read-only version preflight for the proposed shared tag |
| `scripts/dependencies.py generate` / `verify` | Generate / verify the client dependency inventories |
| `scripts/release.sh` / `just release` | Retired; fail with a pointer to the main Fluss release process |

The Python tools require **Python 3.11+**. Set `PYTHON` when using the shell
wrapper or just recipes if `python3` selects an older interpreter.
No tool on this page creates or pushes a tag, uploads a package, or approves a
release. The preflight is local tooling; it does not replace the checks in the
publishing workflows or automatically add a new CI gate.

## Prepare all versions before tagging

Use a clean release checkout. Run the commands below from the **main repository
root**, unless indicated otherwise. Finish this section **before** the main
release guide's step that creates the RC tag or builds source artifacts.

1. Follow the main guide to prepare Java and Gateway versions with
   `tools/releasing/update_branch_version.sh`. That script does not update the
   Rust workspace. It also creates a commit, so review the checkout first.
2. Update the client versions. The bindings inherit the Cargo workspace version;
   the Elixir application's separate version is updated by the same command:

   ```bash
   RELEASE_VERSION="1.0.0"
   CURRENT_CLIENT_VERSION="1.0.0"  # Set to the actual current client version.
   fluss-rust/scripts/bump-version.sh "$CURRENT_CLIENT_VERSION" "$RELEASE_VERSION"
   ```

   The script validates all three current version fields before writing. It
   edits only the owned manifest fields, not unrelated dependency versions.
   It does **not** hand-edit lockfiles or generated license inventories.
3. Refresh lockfiles using Cargo in **both** workspaces. Gateway has a separate
   lockfile and a path dependency on the Rust client:

   ```bash
   cargo update --workspace --offline --manifest-path fluss-rust/Cargo.toml
   cargo update --workspace --offline --manifest-path fluss-gateway/Cargo.toml
   ```

   Offline resolution requires dependencies to be cached. If they are missing,
   populate the cache deliberately before retrying. Review both lockfile diffs;
   do not silently include unrelated dependency upgrades in a version-only change.
4. Regenerate and review the dependency inventories with the pinned cargo-deny
   version required by the client scripts:

   ```bash
   (cd fluss-rust && python3 scripts/dependencies.py generate)
   (cd fluss-rust && python3 scripts/dependencies.py verify)
   ```

   Also refresh and verify Gateway's inventory and binary license files according
   to its release instructions. Its dependency closure includes the Rust client.
5. Run the read-only version preflight:

   ```bash
   python3 fluss-rust/scripts/release_version.py check --tag "v${RELEASE_VERSION}-rc1"
   ```

   This checks the direct Maven project version, Gateway version, Rust workspace
   and self-dependency versions, inherited member versions, Elixir version,
   Python's dynamic version setting, and local package versions in both Cargo
   lockfiles. A development `SNAPSHOT` version is not a release version.
6. Review and commit **all** manifest, lockfile and inventory changes, run the
   component verification required by the main guide, then create the shared RC
   tag at that exact commit. Do not bump clients after tagging the source release.

The accepted tag names are `vX.Y.Z`, `vX.Y.Z-rcN` and `vX.Y.Z-rc.N`, with N starting
at 1. Source manifests keep `X.Y.Z` for both RC and final tags. This command checks
the working tree against a **proposed tag name**; it does not resolve a Git tag or
prove that an existing tag points to these files. It does not replace Cargo
`--locked` resolution, builds, license audits, signatures or release voting.

For the next development cycle, the bump command accepts Cargo SemVer such as
`1.1.0-SNAPSHOT`, not Maven shorthand `1.1-SNAPSHOT`. Refresh generated files and
review them there as well. Java/Gateway version changes remain under the main
release tooling.

## RC and final publication

The main Fluss release process controls source artifacts and approval:

- The source distribution is `fluss-${RELEASE_VERSION}-src.tgz`. Client sources
  are under `fluss-${RELEASE_VERSION}/fluss-rust/` after extraction.
- An RC tag runs Rust's crates.io dry-run and publishes Python wheels/sdist to
  TestPyPI through the repository workflows.
- After release approval, the final tag triggers crates.io and PyPI publication.
- C++ currently ships as source, not as a separately published precompiled SDK.
  This tooling does not introduce a C++ binary publisher or an Elixir/Hex publisher.

Do not create a separate `fluss-rust-${RELEASE_VERSION}.tgz`, a client-only release
branch, or a competing release tag. A future independent client release policy
would require community agreement and corresponding tooling changes.

## Verify these tools without publishing

```bash
python3 -m unittest discover -s fluss-rust/scripts -p 'test_release_version.py' -v
```

The tests use temporary fixtures and do not modify the checkout's versions, call
package registries, create Git tags, or invoke GPG. From `fluss-rust/`, the same
suite is available as `just test-release-tools`.

For artifact checks and client testing, see
[Verifying a Release Candidate](verifying-a-release-candidate.md).
