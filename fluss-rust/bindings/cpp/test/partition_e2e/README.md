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

SDK baseline: `codex/cpp-flush-diagnostics`, commit `f4463335d`. It contains #4941,
#4527, #4969, #4994 and the C++ pending-lookup API. It installs diagnostic logging:
write/metadata DEBUG, other modules WARN. This is a correctness/diagnostic branch,
not a throughput baseline. It does not contain the later scan fix #4606
(`0661d7e7b`); use the recorded revision when interpreting results.

## Build and run

In a dedicated ECS checkout of the fork:

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
python3 "$TEST_DIR/run.py" run --config /root/cpp-test/config.json \
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

Results are under `target/cpp-partition-e2e/results/<run-id>/`:

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
