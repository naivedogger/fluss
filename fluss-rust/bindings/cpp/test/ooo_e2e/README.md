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
