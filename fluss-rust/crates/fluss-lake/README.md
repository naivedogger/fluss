<!--
Licensed to the Apache Software Foundation (ASF) under one
or more contributor license agreements. See the NOTICE file
distributed with this work for additional information
regarding copyright ownership. The ASF licenses this file
to you under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

  http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
-->

# Bounded UnionRead

`fluss-lake` defines a bounded view of a lake-enabled Fluss table and provides
a complete default reader for that view. It supports append union reads,
deduplicate primary-key current views, and lake-only reads.

## Rust version

`fluss-lake` requires Rust 1.91 or later. This package-level minimum applies
with or without the optional `paimon` feature, including preparation-only
integrations. Disabling Paimon avoids its dependencies, not this compiler
requirement. The core workspace's MSRV and the development `stable` toolchain
are unchanged.

## Choose an integration level

```text
FlussLakeTable -> FlussLakeScan -> plan() -> FlussLakeReadPlan
                       |                         |
                 with_lake_source()         new_reader()
                       |                         |
              LakeSource::plan/read       FlussLakeReader
              (default: Paimon)            -> final Arrow batches

Advanced: table.prepare() -> FlussLakeReadContext
                              |                 |
                     plan_with_context()   engine-native plan
                                           readers / reconciliation
                                           scheduling / memory policy
```

The main flow is **Table → Scan → Plan → Reader**. Source-boundary preparation
is internal to `scan.plan()`; most callers need no separate `prepare()` step.
Like Java's FIP-6, a `LakeSource` owns lake planning, split payloads and reading.
Rust passes immutable request contexts rather than mutating shared pushdown
settings or introducing stateless planner/reader factory wrappers.

### Complete default reader

Enable `paimon` when reading a Paimon baseline. The caller schedules logical
`(partition, bucket)` splits; the default reader opens lake files, reads the
bounded log tail, reconciles PK changes, and applies exact supported predicates.

```rust,no_run
use fluss::client::FlussConnection;
use fluss::metadata::TablePath;
use fluss::predicate::col;
use fluss_lake::{FlussLakeTable, Result};
use futures::TryStreamExt;
use std::sync::Arc;

async fn query(connection: Arc<FlussConnection>, path: &TablePath) -> Result<()> {
    let table = FlussLakeTable::open(connection, path).await?;
    let scan = table.new_scan()
        .with_filter(col("id").eq(42_i32))
        .with_projection_by_names(vec!["name".to_string()])
        .with_batch_size(4096);
    let plan = scan.plan().await?;
    let reader = plan.new_reader();
    for split in plan.splits() {
        let mut stream = reader.read_split(split).await?;
        while let Some(batch) = stream.try_next().await? {
            // Feed batches into a query attempt that can be discarded on error.
            assert_eq!(batch.schema(), plan.schema());
        }
    }
    Ok(())
}
```

The plan binds the projection, filter, mode, lake source and frozen boundaries.
Plan-created readers reject foreign or modified tasks; changing another scan
cannot reinterpret this plan. Both reader entry points reject duplicate task
IDs within one `read_splits` call. `read_splits(plan.splits())` reads an unordered
stream with at most eight active logical tasks. Use
`read_splits_with_concurrency(..., n)` to set a positive limit, or schedule
`read_split` in the host engine. Dropping the stream cancels active reads; the
first error is terminal and drops sibling streams.

`with_projection(vec![])` returns zero-column batches with correct row counts.
The reader retains hidden key/filter columns until reconciliation and filtering
are complete. This supports count scans but is not a metadata-only count.

### Execute transported splits on a worker

The coordinator distributes the original serialized splits. The engine supplies
the same projection, filter, read mode and batch-size configuration to workers,
along with a compatible backend and catalog mapping. Credentials stay local.

```rust,no_run
use fluss::client::FlussConnection;
use fluss::metadata::TablePath;
use fluss::predicate::col;
use fluss_lake::{FlussLakeReadSplit, FlussLakeTable, Result};
use futures::TryStreamExt;
use std::sync::Arc;

async fn worker(
    connection: Arc<FlussConnection>,
    path: &TablePath,
    assigned_splits: Vec<FlussLakeReadSplit>,
) -> Result<()> {
    let table = FlussLakeTable::open(connection, path).await?;
    // Restore the coordinator's scan configuration from the engine's own plan.
    let reader = table.new_scan()
        .with_filter(col("id").eq(42_i32))
        .with_projection_by_names(vec!["name".to_string()])
        .with_batch_size(4096)
        .new_reader();
    let mut stream = reader.read_splits(&assigned_splits).await?;
    while let Some(batch) = stream.try_next().await? {
        // Consume within a query attempt that can be discarded on error.
        let _ = batch;
    }
    Ok(())
}
```

`scan.new_reader()` performs no planning or I/O and captures immutable settings.
It reads the snapshot and log bounds already in the split, without calling
`prepare()`, `plan()` or `plan_with_context()`. The worker needs neither the
coordinator's plan object nor its complete task list.

Configuration consistency and task delivery are the engine's responsibility.
Splits contain no projection/filter or configuration fingerprint; a mismatch
may produce incomplete results or a wrong output schema. Workers still validate
descriptor versions, table/schema identity, partition/key layout and backend
payloads. Those checks do not authenticate tasks or compare remote scan settings:
accept tasks only from a trusted coordinator. Plan-created readers additionally
check task membership and the complete frozen read context's metadata.

### Replace lake reading without replacing UnionRead

Supply an `Arc<dyn LakeSource>` using `scan.with_lake_source(source)`. This does
not replace boundary selection, default logical grouping, Fluss log reading,
PK reconciliation or final filtering.

- `LakeSource::plan(LakePlannerContext)` receives the exact snapshot, table
  metadata, required baseline semantics and a safe predicate. It returns
  versioned `LakeSplit` envelopes; their payloads belong to the backend.
- `LakeSource::read(LakeReaderContext)` receives one append task or the selected
  PK task group for a partition/bucket, physical projection, schema and safe predicate. It returns
  owned Arrow batches. It must not refresh the snapshot or silently skip inputs.
- A wrapper can delegate planning to the public `PaimonLakeSource` and use its
  own reader. That reader must understand the **Paimon backend's payload
  contract**, not parse `FlussLakeReadSplit` internals. Raw file concatenation
  is not enough for PK tables: lake versions and deletes must already be
  reconciled across the supplied task group.
- PK baseline rows must be unique by full key; they need not be sorted.
  Only key-safe predicates are passed down during PK union reconciliation.
  The executor always evaluates the complete predicate after reconciliation.

The optional `paimon` feature provides the only production backend in this
crate. Custom sources also work without this feature. This is an evolving Rust
API, not a stable cross-language ABI or universal file-list protocol. Tasks
must follow the Fluss partition/bucket layout; a bucket-less or differently
partitioned lake needs an adapter restoring that ownership or a native plan.

### Reuse one boundary across table references

Preparation captures the full table before projection and predicate pruning.
This lets a query, such as a DataFusion self-join, share source state without
sharing its physical plan or filter.

```rust,no_run
use fluss::predicate::col;
use fluss_lake::{FlussLakeReadContext, FlussLakeTable, Result};

async fn plan_references(table: &FlussLakeTable) -> Result<()> {
    let context = table.prepare().await?;
    let received = FlussLakeReadContext::from_json(&context.to_json()?)?;

    let left = table.new_scan().with_filter(col("id").eq(1_i32));
    let right = table.new_scan().with_projection(vec![0]);
    let left_plan = left.plan_with_context(&received).await?;
    let right_plan = right.plan_with_context(&received).await?;
    assert_eq!(
        left_plan.read_context().to_json()?,
        right_plan.read_context().to_json()?
    );
    // Execute each plan with its own plan.new_reader().
    Ok(())
}
```

### Engine-native execution

`prepare()` and context transport do not open a lake catalog and do not require
the `paimon` feature. A native adapter consumes the full Fluss schema, table
identity, pinned lake snapshot ID, partition/bucket layout, and half-open log
ranges. Engine expressions, physical file tasks, runtime handles, credentials,
and memory pools stay outside this context.
This is an advanced Rust boundary-sharing helper, not a Java API counterpart.
Engines must not decode default logical splits to build a native file reader.

The native adapter must:

1. Validate the context against live Fluss table metadata, using
   `context.validate_table(&table_info)`, and validate the lake table mapping,
   schema, merge engine, partition layout, and bucketing.
2. Plan exactly `context.lake_snapshot_id()`, never a fresh latest snapshot.
   Include selected lake-only partitions that have expired from Fluss.
   `log_ranges()` is the inventory of captured **live Fluss** buckets, not an
   inventory of all lake partitions or files.
3. Safely prune ranges, then call `validate_available()` on every needed log
   range. Read `[start_offset(), stop_offset())` and detect unavailable data
   during execution. Without a baseline, the required start is zero, not the
   earliest retained offset.
4. For PK reads, obtain a lake current view, not raw concatenated Parquet rows.
   Respect Paimon merge/sequence/deletion semantics. Preserve log change types
   and per-bucket offset ordering. Reconcile each key exactly once before
   evaluating mutable-column predicates or limits.
5. Share overlay state safely or exchange rows by full key when splitting a
   bucket into several tasks. Independent per-file overlays must not emit the
   same surviving tail row more than once.
6. Fail the whole query attempt if a frozen input disappears. Discard its
   output before preparing another context. Do not mix old and new boundaries.

SR can implement these operators in its own execution layer. This crate does
not provide SR FE/BE bindings, a DataFusion provider, or a scheduler. Rust
consumers may reuse core `DeduplicateCurrentView`; native engines can use an
equivalent algorithm under the same semantics.

## Limits and compatibility

- A context is neither a global transactional snapshot nor a retention lease.
  Offsets are captured per bucket and cannot be compared across buckets.
- Full-table preparation currently costs offset RPCs for all live partitions,
  even when a later scan selects only one. It avoids unsafe reuse of a pruned
  context. Partition metadata requests use bounded concurrency; scoped contexts
  would need an explicit coverage contract.
- Default append plans expose each lake split and each nonempty bucket log tail
  as independent tasks, including lake-only partitions. Reading multiple tasks
  does not guarantee output order. Default PK plans keep one logical split per
  selected `(partition, bucket)` so baseline reconciliation and tail survivors
  execute together. Native plans may choose their own task granularity.
- Partitioned tables may retain different bucket counts after changing the table
  default. Preparation freezes each live partition's actual count, and pruning
  uses that partition's hash modulus. Lake tasks carry their partition count,
  including expired lake-only partitions; conflicting live/lake layouts fail.
  Legacy metadata without a count uses the table default. This supports changing
  the default for new partitions, not redistributing existing partitions.
- The default PK overlay has no spill or hard memory cap. Fully superseded
  batches are released; partially live batches can retain obsolete buffers.
  Survivor output is incremental, but survivor indexes also consume memory.
- The optional backend uses Paimon 0.3 and supports Parquet. Its Arrow 58 batches
  cross into workspace Arrow 59 through the Arrow C Data Interface.
- Context version 2, default split descriptor version 3 and backend payload
  versions are separate. This revision does not accept old V1 contexts or V1/V2 default splits.
  Unknown versions fail explicitly. Receive contexts from trusted coordinators;
  decoding validates structure, not authenticity or retention.
- Default splits can round-trip for local retries or remote worker execution.
  They carry frozen source work, not the scan's filter, projection, runtime
  source or credentials. Engines restore those settings and use
  `scan.new_reader()`; no full-plan transport or worker-side replanning is needed.
- Context-based planning and plan-created readers conservatively check source
  table modification time along with schema and layout. Worker readers check
  the identity/layout recorded in the split, not the complete source context.
  Runtime Fluss cluster and lake catalog/table mappings must remain consistent;
  neither a context nor a split authenticates a catalog.
- Snapshot selection is source-owned. There is no arbitrary historical
  `with_snapshot_id`, target-parallelism knob, or public memory-pool API.

## Tests

Run from the Rust workspace:

```bash
cargo test -p fluss-lake
cargo test -p fluss-lake --features paimon
cargo test -p fluss-rs --lib current_view
cargo test -p fluss-lake --features integration_tests --test test_prepare --test test_union_read
```

The `lake-msrv` CI job checks default and all-feature targets on Rust 1.91.0
and runs the Paimon library unit tests. Its all-feature check compiles, but
does not execute, service-backed integration tests.

The `integration_tests` feature requires Docker for the Fluss log-only tests.
Preparation and log-only cases share the existing cluster support; no shared
warehouse directory or host bind mount is needed.

Real tiering interoperability is tested separately by `RustUnionReadITCase` in
`fluss-lake-paimon`. Java owns the Fluss cluster, Flink MiniCluster, temporary
Paimon warehouse, baseline writes and log tail. A precompiled Rust test discovers
the readable snapshot and log boundaries through the public UnionRead API and
reads the same local warehouse. Append and PK update/delete/insert scenarios
check both sides, transported split retries, lake-only reads, PK filtering,
zero-column count scans, injected lake sources and invalid task rejection.
Four additional scenarios cover append/PK reads after increasing and decreasing
the default bucket count. Each includes an old live partition, a new partition,
an expired lake-only partition, a real log tail, and bucket/partition filters.
The workflow requires all six scenarios to execute without skips.
No Docker, S3 service, warehouse copying or production CLI is needed for this suite.

The path-scoped `Rust UnionRead Integration` workflow builds both runtimes and
runs this suite in one Linux job. It is triggered by relevant Rust code/build
files, the Java driver and the workflow itself, not by every Java tiering/server
change; run it manually when validating an upstream contract change. Fork PRs
may require maintainer approval. The ordinary Java suite does not require Rust.

To reproduce locally, from the repository root:

```bash
./mvnw -pl fluss-lake/fluss-lake-paimon -am -DskipTests install
cd fluss-rust
cargo +1.91.0 test -p fluss-lake --locked --features paimon \
  --test test_tiered_union_read --no-run --message-format=json > /tmp/union-read-build.json
# Use the compiler-artifact executable for target test_tiered_union_read from
# that JSON output (see the workflow for automatic extraction).
export FLUSS_RUST_UNION_READ_TEST_BIN=/absolute/path/to/test_tiered_union_read-HASH
cd ..
./mvnw -pl fluss-lake/fluss-lake-paimon -Dtest=RustUnionReadITCase \
  -Dfluss.rust.union-read.enabled=true test
```

The Rust test is ignored in standalone Cargo runs because it needs a live
Java-owned fixture. The dedicated job explicitly executes it; a missing binary,
missing fixture setting, timeout or failed assertion fails the suite.
Object-storage transport, missing-file and expired-partition full-chain scenarios
from the earlier manual fixture are not claimed as coverage of this smaller suite.
