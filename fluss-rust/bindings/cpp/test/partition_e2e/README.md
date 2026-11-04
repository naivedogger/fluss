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

# C++ partition E2E on ECS

Run against an existing test cluster using the same connection JSON as the OOO/upgrade
scripts. Each case is a separate process with a deadline. No cluster restart, leader
migration, database creation, or existing-table deletion is automated.

SDK baseline: `codex/cpp-flush-diagnostics` plus upstream `0661d7e7b` (#4606)
and the #5042 stale-leader-index fix/review tests (`2896e1c6a`). It contains #4941,
#4527, #4969, #4994 and the C++ pending-lookup API. It retains diagnostic logging:
write/metadata DEBUG, other modules WARN. This is a correctness/diagnostic branch,
not a throughput baseline. #5042 does not fix missing-partition isolation in scanner
or lookup metadata refresh; the fault cases still need evidence of that path.

## Build and run

The normal ECS path uses a Linux runtime built and published by CI. Select
`codex/cpp-partition-e2e` for the SDK, the `fluss-e2e` harness and the `fluss-ali`
publish configuration. The harness enables `NATIVE_PARTITION_E2E=ON` and includes
this binary and runner in the existing OSS runtime package.

On ECS, fetch this branch from the SDK fork into the existing checkout and create
an adjacent worktree, preserving the previous OOO/upgrade checkout and its results:

```bash
cd /root/cpp-test/fluss-ooo-fixed
git fetch https://github.com/naivedogger/fluss.git codex/cpp-partition-e2e
git worktree add --detach /root/cpp-test/fluss-partition FETCH_HEAD

python3 /root/cpp-test/fluss-partition/fluss-rust/bindings/cpp/test/partition_e2e/download.py \
  --sdk-ref 6f683b517538f4a0fc9858ff6a712b636b28537d \
  --output /root/cpp-test/partition-results -- \
  --config /root/cpp-test/config.json \
  --server-version YOUR_DEPLOYED_VERSION --rows 100 \
  --cases log arrow pk lifecycle missing-log missing-pk
```

Reuse the existing connection JSON, updating it for the temporary test cluster.
The downloader requires Python 3.9+, `ossutil` on PATH and its configured OSS access.
For a non-default OSS configuration, add `--oss-config /path/to/.ossutilconfig`
before `--`. This is separate from the Fluss connection JSON after `--`.
The runtime must match the ECS architecture and glibc version.

The SDK commit above selects the already published binary, independently of the
Git revision containing the downloader. The script verifies the archive, SDK
revision, platform and bundled partition binary/runner hashes, then executes that
runner without compiling or requiring the internal harness repository. It prints
the output directory and resolved SDK commit. Each invocation uses a fresh directory.

For local development, build from an SDK source checkout:

```bash
git fetch origin codex/cpp-partition-e2e
git switch codex/cpp-partition-e2e

TEST_DIR="$PWD/fluss-rust/bindings/cpp/test/partition_e2e"
python3 "$TEST_DIR/run.py" build
python3 "$TEST_DIR/run.py" run \
  --config /root/cpp-test/config.json \
  --server-version YOUR_DEPLOYED_VERSION \
  --cases log arrow pk lifecycle missing-log missing-pk
```

The first build compiles the SDK and is separate from test execution time. The default
Arrow archive is `/root/cpp-test/deps/arrow/19.0.1/apache-arrow-19.0.1.tar.gz`;
override with `build --arrow /path/to/archive`, or use an installed Arrow configuration
with `build --arrow-dir /path/to/cmake/Arrow`. Rebuild after updating the branch.

Configuration keys: `endpoint`, `security_protocol`, `sasl_mechanism`, `username`,
`password`, `database`, `replication_factor`. The database must already exist.
Replication defaults to 3; set it to the test cluster's supported value. Credentials are
passed through the child environment, not command arguments or the summary.

Defaults: 1,000 rows per partition, 4 buckets, three partitions for Log/Arrow/PK,
120 seconds maximum per automatic case. Most cases should finish much earlier;
timeouts are upper limits, not target durations. Use `--rows 100` for the initial smoke.
The 5 ms writer/lookup batching settings keep correctness tests short.
Initial table acquisition retries transient metadata errors for up to 30 seconds
after creation; fault-path reads and individual write acknowledgments retain their errors.

## Cases and assertions

| Case | Checks |
| --- | --- |
| `log` | Interleaved row appends to A/B/C; full payload, physical partition ID, duplicate/missing IDs; rebuild scanner using per-bucket next offsets |
| `arrow` | Homogeneous partition batches of up to 127 rows; bounded Arrow read to captured latest offsets; exact set and payload comparison |
| `pk` | Full insert/readback; bucket-key prefix lookup; update B, delete a subset of A; exact final state including unchanged C; absent partition lookup |
| `lifecycle` | Two partition keys; delete/recreate A and add B; IDs change; partial-spec listing; reuse writer and check individual ACKs; explicitly retire old subscriptions and discover/subscribe new IDs |
| `missing-log`, `missing-pk` | Disabled dynamic creation returns code 36 while B on the same connection completes; enabled creation makes A writable; read back A/B |
| `float-name` | Precreate the Java-formatted DOUBLE 1.0 partition `1_0`; a C++ write must not create `1` instead; verify offset count in `1_0` |
| `scanner-drop`, `lookup-drop` | Warm original reader caches, delete A, allow operator to change B's routing, verify B recovery and finite A lookup completion |
| `scanner-control`, `lookup-control` | Same fault procedure while retaining A |

Without `--cases`, all seven automatic cases run, including `float-name`.
The floating-point compatibility probe is expected to expose the known naming defect
on this baseline. A failure is reported normally, never converted to a pass.
It uses Java's known canonical name; it does **not** execute a Java producer.

Every table has a unique `pe_<run-id>_<case>` name. Successful automatic cases delete
only their own table unless `--keep` is set. Failed/timed-out cases and manual fault
cases retain their tables. Read `stdout.log` for the table path before manual cleanup.

This entry does not yet automate auto-partition expiry, different bucket counts between
partitions, historical partition writes, or bidirectional Java/C++ interoperability.
Those require a separate service-side setup/Java driver. It does not exercise writes
already in flight when a partition is recreated; `lifecycle` flushes before deletion.
An old writer can still assign a subsequent write to its cached, deleted partition.
The lifecycle case accepts either a successful ACK or explicit code 36; on code 36 it
logs the rejection and resubmits through a fresh connection, then checks exact readback.
It does not accept silent loss or an arbitrary error as a successful recovery.

## Manual fault procedure

Run the control and deletion variants separately against an isolated test cluster.
Each uses fresh tables. Do not run overlapping fault injections.

```bash
python3 fluss-e2e-native/ci/partition.py --sdk-ref codex/cpp-partition-e2e -- \
  --config /root/cpp-test/config.json \
  --server-version YOUR_DEPLOYED_VERSION --cases scanner-drop
```

1. The runner prints the case log directory. Wait for its `fault.ready` file.
   `stdout.log` contains table and partition IDs; A has already been deleted in the
   `*-drop` cases. Keep the process alive: it retains the original reader and connection.
2. Identify the leader(s) of this table's B buckets from cluster tooling. Migrate the
   leader away from the cached node, or perform an approved node restart and confirm
   actual route invalidation. A restart that retains the same leader may not reproduce
   the failure condition.
3. When the cluster is healthy and B's new leader is serving, record the before/after
   leader evidence and create the marker in the **case's** directory:

   ```bash
   touch /absolute/path/printed/by/runner/scanner-drop/recovery.ready
   ```

4. The scanner case writes a new sentinel through a separate connection, preventing
   the writer from refreshing the old scanner's shared metadata, and requires the
   original scanner to read it. The lookup case queues A and B on the original lookuper,
   waits up to 60 seconds for B, and requires A to terminate rather than stay pending.
5. Inspect SDK logs for unknown-leader/metadata refresh involving the dropped A and
   surviving B. Attach the server routing evidence. Without that evidence a successful
   readback does not establish coverage of the combined-refresh defect.

Successful manual cases are always `needs-evidence` (exit 2), not automatically passed.
A failed call or stalled readback is `failed`/`timeout` (exit 1); distinguish SDK failure
from a cluster that has not recovered. The operator window defaults to 600 seconds;
it is not a fixed load period. The in-process marker wait is bounded at eight minutes.

For the retry-limit check, rerun `lookup-drop --lookup-retries 0`. A stale leader RPC
can then fail immediately before reaching the unknown-leader path; verify logs before
claiming reproduction of the retry-counter defect.

## Results

Local results are under `target/cpp-partition-e2e/results/<run-id>/`.
The ECS downloader puts them under its printed output directory's `results/<run-id>/`:

- `summary.json`: built SDK commit/dirty flag and binary SHA-256, supplied server version, parameters,
  per-case outcome and elapsed time.
- Per case: `stdout.log` with stages/partition IDs, `stderr.log` with SDK diagnostics,
  and `pid`. Linux timeouts attempt `gdb thread apply all bt` before killing the
  process group; missing gdb or attach permission does not prevent timeout enforcement.
- Build products and temporary files stay under repository/worktree build directories.

Exit 0 means all selected automatic cases passed; exit 1 means at least one failed or
timed out; exit 2 means the manual case readbacks succeeded but evidence needs review.
`--timeout` bounds the entire case including cleanup, not each SDK operation.
A timeout does not prove the server accepted no writes.

Run the watchdog regression locally with:

```bash
python3 -m unittest discover -s "$TEST_DIR" -p 'test_runner.py'
```
