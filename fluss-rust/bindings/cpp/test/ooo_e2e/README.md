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

# C++ OOO and performance comparison

Run the same harness from each experimental branch. Pull and rebuild after code changes;
no source archive is needed. This entry point is not part of the normal SDK build.

## On ECS

For the fixed checkout (use `/root/cpp-test/fluss-ooo-baseline` for the other arm):

```bash
cd /root/cpp-test/fluss-ooo-fixed
git pull --ff-only
cd fluss-rust/bindings/cpp/test/ooo_e2e
ln -s /absolute/path/to/new-cluster-config.json config.json
python3 run.py build
python3 run.py smoke
```

The config link is only needed once. `config*.json`, build outputs and results are
Git-ignored. The existing config is read-only: `endpoint`, `security_protocol`,
`sasl_mechanism`, `username`, `password`, `database`, `replication_factor`,
`payload_bytes` and `target_rows_per_second` are used;
unrelated benchmark options are ignored. No credentials enter commands or manifests.
Logs may contain connection context; redact before sharing.

Use an existing database: `--database NAME` overrides the config (default: `fluss`).
Replication factor defaults to 2. No database is created or deleted.
`python3 run.py check` is an optional connection/authentication/database check.

Build defaults: four jobs, Release, `file-logging`, locked Cargo dependencies, and
`/root/cpp-test/deps/arrow/19.0.1/apache-arrow-19.0.1.tar.gz`. Override with
`--arrow /path/to/archive -j 4`. Each checkout has its own build and Cargo target.
Other build dependencies still need network access or an existing cache.

## Compare once per arm

After smoke passes, run `python3 run.py run` in baseline, then in fixed; do not run
both simultaneously. Each arm sends for five minutes, followed by drain and readback.
Review the results and reclaim baseline's test-table storage
before starting fixed if space is limited. Tables are retained; delete them manually.

| Parameter | Smoke | Full run |
| --- | --- | --- |
| Processes × writers | 4 × 5 | 4 × 5 |
| Total target rows/s | 1,000 | config, default 400,000 |
| Warmup + measurement | 2 + 5 seconds | 60 + 240 seconds |
| Payload / buckets | config, default 1 KiB / 32 | config, default 1 KiB / 32 |

Both use one log table, one connection per process, dynamic batches from 256 KiB to
2 MiB, linger 100 ms, buffer 256 MiB/connection, idempotence, acks=all and 5 inflight
requests/bucket. Actual batch sizes depend on load. With the defaults, RF=2 full-run
replicated payload alone is about 229 GiB per arm; check **server** capacity, not the
ECS client disk.

For 100-byte payloads at a total target of 4 million rows/s, add these fields to the
existing config (preserve the connection/authentication fields):

```json
{
  "payload_bytes": 100,
  "target_rows_per_second": 4000000
}
```

Rebuild both checkouts once to pick up payload configuration; subsequent config
changes do not require compilation. Smoke uses the configured payload at 1,000 rows/s.
The size is the string payload, excluding the ID and encoding overhead. At this target,
payload traffic is about 381 MiB/s, similar to the default 391 MiB/s, but per-record
CPU work and readback counts can grow tenfold. Full-run replicated payload alone is
about 224 GiB at RF=2. Compare actual throughput; this target is not a capacity claim.
Each producer prints a `WORKLOAD` line with the effective payload size and total rate.
Keep each writer below 100 million records per run: the harness uses disjoint 32-bit
ID ranges and stops if a writer exhausts its range (4 million rows/s for five minutes
with 20 writers targets 60 million records per writer).

## Read an existing run from remote storage

Pull and rebuild the fixed checkout, then reuse a completed run on the **same cluster**:

```bash
python3 run.py readback --config /root/cpp-test/config.json \
  --run-dir /root/cpp-test/fluss-ooo-fixed/fluss-rust/bindings/cpp/test/ooo_e2e/runs/ooo_a3d7b880631d
```

This only scans the original table from offset 0; no setup, writes or deletion.
Keep the original `manifest.json`, `result.json`, `setup.log` and `counts-0` through
`counts-3`. The table ID must still match. Payload size comes from that run's manifest,
not the current config. Results go into a fresh `readback_<id>` subdirectory printed
as `READBACK_DIR`; original results remain unchanged. The read timeout is one hour.

The command enables `fluss::client::table::remote_log=debug`. After full ID verification,
`result.json` reports download chunk log observations by URI scheme:

- `ossj_observed` / `oss_observed`: ID verification passed and remote download activity
  was observed. `ossj://` is handled by Rust's OSS/OpenDAL path, not the native Jindo SDK.
- `other_remote_observed`: remote activity used another scheme; not OSS coverage.
- `remote_not_observed` (exit 2): ID verification passed, but remote reads remain unproven.
- `failed`: scan/verification failed or log evidence was incomplete; inspect `verify.log`
  and `sdk-logs/` in the new subdirectory.

Remote reads require uploaded segments whose local copies are no longer available;
exceeding segment size alone is insufficient. No retention settings are changed here.
The chunk logs are sampled and may include prefetch: they do not measure exact remote
bytes or consumed offsets. Scan rows/s and payload MiB/s include local/remote reads and
ID verification, not pure OSS bandwidth. ID checks do not compare payload contents.

## Results

Each invocation prints `RUN_DIR`, containing:

- `result.json`: warmup and measurement ACK rows/s (including drain), merged sampled
  P50/P99 latency, write errors, readback and client OOO/reset log counts.
- `producer-*.log`: per-second counters and per-process performance.
- `resources.jsonl`: producer RSS sampled every five seconds on Linux.
- `sdk-logs/`: per-process rotating logs at
  `warn,fluss::client::write::idempotence=debug`.
- `manifest.json`: checkout SHA, table name and workload parameters, no credentials.

Latency is Append-to-callback, sampled every 256 rows, with millisecond upper-bound
bins. The final 60001-ms bin is overflow, not a precise upper bound. Readback checks
all generated IDs for missing/duplicate/unexpected records, not payload equality.

Nonzero OOO counts are retained for comparison, not treated as a runner failure.
Log drops, truncation, write errors or missing drain stats make evidence incomplete.
A full rotation window is conservatively flagged. Zero client OOO logs alone do not
prove zero server errors. Failed/timed-out runs preserve logs and tables and are not
valid zero-error results. No automatic retry or repeat rounds.

Local runner tests: `python3 -m unittest -v test_runner.py`.

## Nonstop read/write during a cloud upgrade

`upgrade.py` creates a separate workload; it does not change the OOO comparison.
Run it on a three-TabletServer test cluster with **server min ISR already set to 2**
(`log.replica.min-in-sync-replicas-number`). The script sets RF=3 and acks=all, waits
for bucket leaders, and retains all tables. Confirm full ISR=3 and healthy OSS
offloading in the cloud console before spending the cluster's upgrade opportunity.

| Parameter | Default |
| --- | --- |
| Main log table | 64 buckets, RF=3 |
| Auxiliary empty log tables | 30 tables × 20 buckets × RF=3 |
| Total replicas | 1,992 |
| Writers / readers | 4 processes × 5 writers / 4 processes |
| Payload / ACKed preload | 1 KiB / 1 TiB |
| Target write / read rate | 500 / 300 MiB/s of uncompressed payload |
| Before / after upgrade | 10 / 15 minutes |
| Load limits | 4 hours or 4 TiB accepted payload, including preload |

The same writer processes and connections continue from preload through recovery,
without phase-boundary flushes. At the target rate preload takes about 35 minutes.
Readers start at offset 0 after preload and keep their scanners across the upgrade;
their lag intentionally grows because reads are slower than writes. Actual rates
depend on client and cluster capacity. Logical payload rates do not imply a fixed
compression ratio or server disk/OSS bandwidth; monitor those separately. RF=3 data
can exceed the three 900-GB local disks, so OSS upload and local eviction must work.
Keep the table's total retention long enough for the entire run and final scan.

From this directory, reuse only the connection/authentication/database fields in
the existing config; workload defaults above override its old payload/rate/RF:

```bash
python3 upgrade.py build
nohup python3 -u upgrade.py run --config /root/cpp-test/config.json \
  > /root/cpp-test/upgrade-test.log 2>&1 < /dev/null &
tail -f /root/cpp-test/upgrade-test.log
```

Wait for `STAGE ready_for_upgrade`. Writing and reading continue while waiting.
Start the upgrade in the cloud console, then record its start in another shell:

```bash
python3 upgrade.py mark-start
```

When the cloud upgrade completes:

```bash
python3 upgrade.py mark-done
```

The runner continues for 15 minutes, drains writers, then scans the entire main
table to check all generated 64-bit IDs, duplicates, missing rows, payload length
and final offsets. Full verification has a two-hour timeout and can take time on
TiB-scale data. It does not compare payload contents. No table is deleted.

`status`, `mark-start` and `mark-done` default to this checkout's latest upgrade run;
use `--run-dir /absolute/path` to select one explicitly. `--source-version` and
`--target-version` on `run` record operator-supplied version labels. Other workload
overrides are listed in `python3 upgrade.py --help`.

Results are under `runs/upgrade_<id>/`:

- `metrics.jsonl`: one-second ACK/read rates, pending rows, errors and ACK stalls.
  Console rates average about ten seconds; `recent_*_p99_ms` uses the latest
  histogram from each client process, rather than the whole phase.
- `writer-*.log` / `reader-*.log`: sampled callback latency / Poll wall-time
  histograms. Reader data age includes the intentional backlog, not just fetch time.
- `result.json`: phase throughput/P50/P99, final ID verification and log statistics.
  Short phases without two samples report no rate rather than zero.
- `events.json`, `manifest.json`, `tables.json`: marker times, workload/SHA and
  created table names/IDs. `sdk-logs/` retains rotating client diagnostics.

`completed` means the clients finished and final ID verification passed. Cloud
version, ISR recovery, disk/OSS health and acceptable performance still need review.
Log drops reduce diagnostic coverage but do not fail successful ID verification.

Runner tests: `python3 -m unittest -v test_runner.py test_upgrade.py`.
