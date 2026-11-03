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

`fluss-lake` defines a bounded view of a Fluss table and provides
a complete default reader for that view. It supports append union reads,
deduplicate primary-key current views, and lake-only reads.

## Read semantics

The same entry accepts ordinary Fluss tables and lake-enabled tables. Without a lake seam, each bucket starts at the earliest retained offset captured during preparation; readers never resolve earliest again. With a seam, the pinned lake snapshot and `[seam, stop)` form the input. `with_lake_only(true)` requires a lake-enabled table; before its first snapshot it returns no lake rows.

PK reads without a baseline reconstruct only keys represented in retained history; they do not promise a complete historical current view. KV snapshot fallback is future work. All reads here are bounded. Streaming discovery, progress and checkpointing require a separate future contract, not a change to existing stop semantics.

The default PK UnionRead executor supports Fluss default deduplicate semantics and a deduplicate lake baseline; other merge engines fail with `UnsupportedMergeEngine`. It does not implement aggregation or partial-update reconciliation. Lake-only reads delegate merge semantics to the lake backend. A valid lake baseline plus the available log tail supports the current view without a KV snapshot fallback.

ReadContext exposes structured source boundaries. Engines own cross-process task serialization and compatibility; the context has no public wire format. Default tasks are local, cloneable values with no public serialization contract. Lake backend payloads remain an implementation-specific planning/reading interface.

## Rust version

`fluss-lake` requires Rust 1.91 or later. This package-level minimum applies
with or without the optional `paimon` feature, including preparation-only
integrations. Disabling Paimon avoids its dependencies, not this compiler
requirement. The core workspace's MSRV and the development `stable` toolchain
are unchanged.

## Choose an integration level

```text
FlussLakeTable -> FlussLakeScan -> plan() -> Plan -> Splits
                       |                            |
                       +-> new_reader() -> Reader <-+
                       |                   -> final Arrow batches
                       +-> with_lake_source()
                           LakeSource::plan/read (default: Paimon)

Advanced: table.capture_read_context() -> FlussLakeReadContext
                              |                 |
                     plan_with_context()   engine-native plan
                                           readers / reconciliation
                                           scheduling / memory policy
```

The scan creates both the plan and the reader; the reader consumes its splits.
Source-boundary preparation is internal to `scan.plan()`; most callers need no separate `capture_read_context()` step. `scan.plan()` extracts implied equality conditions on STRING partition keys (including AND conjuncts) before querying offsets. Other conditions conservatively keep a wider capture scope and remain available for later pruning and exact filtering. The resulting context records the extracted scope, including lake-only partitions; it cannot be reused as a table-wide boundary. Use `table.capture_read_context()` when several scans need a shared table-wide boundary, or `capture_read_context_with_partition_scope()` when the coordinator already knows their common input scope. `plan_with_context()` never recaptures or narrows that supplied boundary. There is no public `scan.capture_read_context()` method.
Like the Java lake SPI, a `LakeSource` owns lake planning, split payloads and reading.
Rust passes immutable request contexts rather than mutating shared pushdown
settings or introducing stateless planner/reader factory wrappers.

### Complete default reader

Enable `paimon` when reading a Paimon baseline. The caller schedules append
lake/log tasks or PK bucket splits; the default reader opens lake files, reads the
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
    let reader = scan.new_reader();
    let mut stream = reader.read_splits(plan.splits())?;
    while let Some(batch) = stream.try_next().await? {
        // Feed batches into a query attempt that can be discarded on error.
        assert_eq!(batch.schema(), plan.schema());
    }
    Ok(())
}
```

The plan freezes source boundaries and logical tasks, while the reader captures
scan settings. Callers must use the same projection, filter, mode, batch size
and compatible lake backend for planning and execution. The reader does not
retain a plan or check task membership. It rejects duplicate task
IDs within one `read_splits` call. `read_splits(plan.splits())` consumes tasks
sequentially in the supplied order; row order within each task is unspecified.
For concurrent execution, callers schedule `read_split()` streams themselves.
Dropping the stream cancels the active read; the first error is terminal and
remaining tasks are not started. Each `read_splits` call lazily shares
one Fluss table metadata load across its tasks. Separate calls, including
`read_split`, start fresh metadata sessions; frozen schemas and boundaries are
unchanged, and core routing refresh remains active.

`with_projection(vec![])` returns zero-column batches with correct row counts.
The reader retains hidden key/filter columns until reconciliation and filtering
are complete. This supports count scans but is not a metadata-only count.

### Schedule local tasks in an engine

The default planner and reader can be used independently of the sequential `read_splits()` helper. An in-process engine can retain or clone selected tasks and schedule `read_split()` itself. Tasks share frozen source metadata and can outlive their plan; no plan identity check or replanning is required.

```rust,no_run
# use fluss_lake::{FlussLakeScan, Result};
# use futures::TryStreamExt;
# async fn execute(scan: FlussLakeScan) -> Result<()> {
let plan = scan.plan().await?;
let tasks = plan.splits().to_vec();
let reader = scan.new_reader();
drop(plan);
for task in tasks {
    let mut batches = reader.read_split(&task)?;
    while let Some(batch) = batches.try_next().await? {
        // Consume this task's output; an engine can schedule tasks concurrently.
        let _ = batch;
    }
}
# Ok(())
# }
```

Use the scan settings that produced the tasks. Splits expose read-only identifiers and estimates, not filter/projection configuration. Using a different filter or read mode after planning can invalidate pruning decisions. The reader checks live source identity without replacing the captured schema or offsets.

There is no default-task wire format or descriptor version. A future distributed adapter can define its own task encoding and recovery contract when needed. Native engines instead use the structured boundary API below and own their physical tasks.

### Replace lake planning and reading without replacing UnionRead

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
API, not a stable cross-language ABI or universal file-list protocol. PK tasks
must follow the Fluss partition/bucket layout for tail reconciliation. Append
lake tasks are independent of log buckets and may use a different fixed layout
or the bucket-unaware pair `bucket_id = bucket_count = -1`. Their source owns
lake bucket pruning; Fluss bucket pruning applies to log tasks.
Both modes preserve the table's logical partition identities.
`reconcile_primary_key` in lake request contexts distinguishes a PK baseline
from a baseline that must also support the default deduplicate overlay.

### Reuse one boundary across table references

Preparation captures the full table before projection and predicate pruning.
This lets a query, such as a DataFusion self-join, share source state without
sharing its physical plan or filter.

```rust,no_run
use fluss::predicate::col;
use fluss_lake::{FlussLakeTable, Result};

async fn plan_references(table: &FlussLakeTable) -> Result<()> {
    let context = table.capture_read_context().await?;

    let left = table.new_scan().with_filter(col("id").eq(1_i32));
    let right = table.new_scan().with_projection(vec![0]);
    let left_plan = left.plan_with_context(&context).await?;
    let right_plan = right.plan_with_context(&context).await?;
    assert_eq!(
        left_plan.read_context().log_ranges(),
        right_plan.read_context().log_ranges()
    );
    // Execute left_plan with left.new_reader(), right_plan with right.new_reader().
    Ok(())
}
```

### Coordinator-defined partition scope

The explicit scope is an advanced input restriction, not a second row-filter API. Ordinary queries express conditions through `with_filter()`.

```rust,no_run
use fluss::metadata::PartitionSpec;
use fluss::predicate::col;
use fluss_lake::{FlussLakeTable, Result};
use std::collections::HashMap;

async fn plan_in_scope(table: &FlussLakeTable) -> Result<()> {
    let scope = PartitionSpec::new(HashMap::from([
        ("region".to_string(), "US".to_string()),
    ]));
    let context = table.capture_read_context_with_partition_scope(&scope).await?;
    let scan = table.new_scan().with_filter(col("id").eq(42_i32));
    let plan = scan.plan_with_context(&context).await?;
    assert_eq!(plan.read_context().partition_filter(), context.partition_filter());
    Ok(())
}
```

### Engine-native execution

`capture_read_context()` and structured context import do not open a lake catalog and do not require
the `paimon` feature. A native adapter consumes the full Fluss schema, table
identity, resolved `lake_table_path()`, pinned lake snapshot ID, partition/bucket layout, and half-open log
ranges. Engine expressions, physical file tasks, runtime handles, credentials,
and memory pools stay outside this context.

Java coordinators call `UnionReadBoundaryPlanner.captureReadContext(admin, tablePath[, scope])` for a structured UnionReadContext. Its accessors expose schema, lake snapshot, partition scope and bucket offsets for FE task planning. The engine owns task serialization and distribution. Rust workers import engine-decoded fields using `from_boundary()` or `from_boundary_with_partition_filter()`, with the captured metadata rather than fresh metadata. Capture blocks a planning thread without opening lake files; the caller owns Admin.

Engines must not decode default logical splits to build a native file reader.

Each log range associates its physical `table_bucket` with `FlussLakeLogPartition::Ordinary(identity)`. Consumers must handle every log partition kind or reject the read before emitting results. Engines must preserve this meaning in their task descriptors; unknown semantics cannot be treated as optional hints.

Shared historical logs (`__historical__`) remain follow-up work. They can contain changes for several business partitions and need a distinct log partition kind, snapshot-aligned offsets and record-level partition filtering. Reading lake-only expired partitions does not cover these pending changes. No historical kind is emitted or accepted yet.

Native engines own physical task decomposition and scheduling. A log range is a source boundary, not a scheduling unit: native tasks need not map one-to-one to Fluss buckets or default splits. The current default Append/PK split strategies are implementation choices; finer splitting and shared PK overlays belong to subsequent planning/execution work.

The native adapter must:

1. Validate the context against live Fluss table metadata, using
   `context.validate_table(&table_info)`, and validate the lake table mapping,
   schema, merge engine, partition layout, and bucketing.
2. Plan exactly `context.lake_snapshot_id()`, never a fresh latest snapshot.
   Include selected lake-only partitions that have expired from Fluss.
   `log_ranges()` is the inventory of captured **live Fluss** buckets, not an
   inventory of all lake partitions or files.
3. Check the log partition kind before partition or bucket pruning; bucket counts alone do not establish routing compatibility. Then call `validate_available()` on every needed log
   range. Read `[start_offset(), stop_offset())` and detect unavailable data
   during execution. Without a baseline, start is the concrete earliest offset captured during preparation, not a sentinel resolved by each worker.
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
- `capture_read_context()` captures the whole table. `capture_read_context_with_partition_scope()` applies an exact, possibly partial `PartitionSpec` before offset queries. Its scope also constrains lake tasks, including lake-only partitions, and cannot be widened by a later scan.
- Default append plans expose each lake split and each nonempty bucket log tail
  as independent tasks, including lake-only partitions. Reading multiple tasks
  does not guarantee output order. Default PK plans keep one logical split per
  selected `(partition, bucket)` so baseline reconciliation and tail survivors
  execute together. Native plans may choose their own task granularity.
- Partitioned tables may retain different bucket counts after changing the table
  default. Preparation freezes each live partition's actual count, and pruning
  uses that partition's hash modulus. PK lake tasks carry their partition count,
  including expired lake-only partitions; conflicting PK live/lake layouts fail.
  Core Admin resolves legacy metadata without a count using the table default
  only while its bucket-count epoch is zero. Missing counts after a rescale
  fail instead of silently using a different layout. UnionRead consumes these
  resolved counts; routing and bounded-read validation use the core client.
  This supports changing the default for new partitions, not redistributing
  existing partitions.
- The default PK overlay has no spill or hard memory cap. Fully superseded
  batches are released; partially live batches can retain obsolete buffers.
  Survivor output is incremental, but survivor indexes also consume memory.
- The optional backend uses Paimon 0.3 and supports Parquet. Its Arrow 58 batches
  cross into workspace Arrow 59 through the Arrow C Data Interface. The adapter
  then reconciles nested Arrow field representations, fixed-length binary and
  temporal units with the frozen physical schema. Invalid binary lengths,
  temporal precision loss/overflow and unrelated type changes fail with
  `SchemaIncompatible`; values are not silently truncated or replaced with null.
- Bucket-key pruning has a bounded candidate/hashing budget and does not
  materialize the Cartesian product of `IN` lists. When the budget is exceeded,
  it keeps all buckets and leaves correctness to the final exact filter.
- Contexts and default tasks have no public wire format. Default tasks share frozen metadata within the process; their getters expose scheduling metadata without allowing callers to rewrite task identities. Lake backend payload versions remain source-owned.
- Context-based planning and local task execution use the frozen schema and source layout. ADD COLUMN does not invalidate a task. Core log scanners map batches by column ID; the lake provider must supply compatible projected columns or fail. Runtime catalog credentials remain local. Source identity checks do not authenticate a runtime catalog.

- Snapshot selection is source-owned. There is no arbitrary historical
  `with_snapshot_id`, target-parallelism knob, or public memory-pool API.

## Implementation map

The modules separate source boundaries, default task planning and execution. They are internal implementation boundaries, not additional public APIs or planning stages that engines must implement.

| Module | Responsibility |
| --- | --- |
| `table.rs` | Public table/scan configuration; shared schema binding, table semantics and runtime lake-source selection. |
| `boundary.rs` | Capture the readable lake snapshot and server-issued log ranges. No lake catalog, file planning or task scheduling. |
| `read_context.rs`, `partition.rs` | Source identity and validated structured read boundaries. |
| `planner.rs` | Apply safe pruning, call `LakeSource::plan`, and construct the default append or PK tasks. |
| `plan.rs` | Immutable planning result: context, output schema, splits and statistics. No reader or I/O. |
| `split.rs`, `split_descriptor.rs` | Local task metadata, shared frozen source information and construction-time validation. |
| `reader.rs` | The single scan-created reader API; lazy task streams, sequential consumption, first-error termination and cancellation. |
| `executor.rs` | Execute one task, read bounded logs, reconcile PK rows, then apply exact filtering, output projection and batch sizing. |
| `source.rs` | The lake-format extension contract and predicates safe to push below reconciliation. |
| `paimon.rs`, `paimon/arrow_conversion.rs` | Paimon catalog/snapshot access, portable task payloads, native baseline reads and lossless Arrow adaptation. |
| `pruning.rs`, `bucket_pruning.rs` | Conservative partition and bucket pruning. Neither replaces the exact result filter. |
| `error.rs` | Public error categories, shared client-error classification and cause diagnostics. |

```text
scan.plan()
  -> scan.prepare_bound(): resolve table metadata and bind scan settings
  -> boundary::freeze_read_context(): freeze source inputs
  -> planner::plan_prepared(): prune and construct tasks
  -> FlussLakeReadPlan

scan.new_reader().read_split(original_split)
  -> reader: validate configuration and establish the stream lifecycle
  -> executor: validate the task and live identity, then bind matching settings
  -> lake-only baseline / append input / PK tail plus baseline
  -> exact filter -> output projection -> batch sizing

table.capture_read_context() or table.capture_read_context_with_partition_scope(scope)
  -> the same source preparation, without the default task planner
  -> host-engine planning, reading and reconciliation
```

The planner and executor share `FlussLakeScan::bind` and runtime source selection; the executor does not call or import the planner. The core client owns predicate binding/evaluation, bucket hashing, log routing, bounded append reading and `DeduplicateCurrentView`. The lake crate composes these facilities instead of maintaining second implementations. Paimon representation adaptation remains backend-specific and must not become a generic permission to cast source columns.

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

Paimon library tests also write and read real local Parquet files for append and
PK baselines, checking ARRAY, BINARY and TIMESTAMP adaptation with nulls and
reordered/pruned projections. This backend regression runs without a Fluss
cluster, Java or Docker; it does not replace the tiering interoperability suite.

The `integration_tests` feature requires Docker for the Fluss log-only tests.
Preparation and log-only cases share the existing cluster support; no shared
warehouse directory or host bind mount is needed.

Real tiering interoperability is tested separately by `RustUnionReadITCase` in
`fluss-lake-paimon`. Java owns the Fluss cluster, Flink MiniCluster, temporary
Paimon warehouse, baseline writes and log tail. A precompiled Rust test discovers
the readable snapshot and log boundaries through the public UnionRead API and
reads the same local warehouse. Append and PK update/delete/insert scenarios
check both sides, cloned local task reads, lake-only reads, PK filtering,
projected count scans, injected lake sources and invalid task rejection.
Four additional scenarios cover append/PK reads after increasing and decreasing
the default bucket count. Each includes an old live partition, a new partition,
an expired lake-only partition, a real log tail, and bucket/partition filters.
Shrinking scenarios require a nonempty lake baseline and log tail in old
buckets above the new table default, through local task execution
using the same scan-created reader.
Append filters also check that a fully pruned log tail reaches its frozen stop,
both when the lake returns rows and when the whole result is empty.
The workflow requires all six scenarios to execute without skips.
No Docker, S3 service, warehouse copying or production CLI is needed for this suite.

The `rust-union-read` job in the existing `Client Integration` workflow builds both runtimes and runs this suite through the Java module's Maven test entry. PR change detection selects relevant core/lake Rust code, Java client/Paimon/tiering changes, server/protocol changes and workflow changes; main pushes and manual runs follow the workflow's existing all-suite policy. The job uses the Java-owned fixture and does not need the Docker server-image job. Fork PRs may require maintainer approval. The ordinary Java suite does not require Rust.

To reproduce locally, from the repository root:

```bash
./mvnw -pl fluss-lake/fluss-lake-paimon -am -DskipTests install
cd fluss-rust
cargo +1.91.0 test -p fluss-lake --locked --features paimon \
  --test test_tiered_union_read --no-run --message-format=json > target/union-read-build.json
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

### Predicate negotiation and deletion vectors

The ordinary `with_filter` path binds supported predicates strictly and evaluates the full predicate after reconciliation. An engine adapter can negotiate accepted/remaining predicates; a predicate accepted only for pruning must remain for exact evaluation. The shared negotiation helper is separate follow-up work, not part of boundary capture.

A LakeSource owns snapshot-specific deletion vectors and lake version reconciliation. Its opaque task payload must retain the data-file/deletion-file association through planning and path rewriting. Native readers must preserve physical row positions when applying DVs. A Parquet reader alone is not a PK current-view reader. There is no generic DV trait or stable reader-only Paimon payload ABI in this API.

Shared historical-log support is tracked in [#5001](https://github.com/apache/fluss/issues/5001); it is separate from reading expired partitions already present in the lake snapshot.
