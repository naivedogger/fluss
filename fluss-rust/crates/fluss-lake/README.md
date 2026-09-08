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
a default reader for append union reads and lake-only reads. Primary-key union execution is explicitly rejected in
this stage; the following reconciliation and PK-reader PRs add that path.

## Rust version

`fluss-lake` requires Rust 1.91 or later. This package-level minimum applies
with or without the optional `paimon` feature, including preparation-only
integrations. Disabling Paimon avoids its dependencies, not this compiler
requirement. The core workspace's MSRV and the development `stable` toolchain
are unchanged.

## Choose an integration level

```text
FlussLakeTable -> FlussLakeScan
                       |
                    prepare()
                       |
             FlussLakeReadContext
                 /            \
        plan_with_context()    engine-native physical plan
                |              native lake/log readers
       FlussLakeReadPlan       native reconciliation
                |              engine scheduling and memory policy
       FlussLakeReadSplit
                |
       FlussLakeReader -> final Arrow batches
```

`scan.plan()` remains the short form of preparation plus default planning.
Engines must not decode default splits to implement a native reader.

### Complete default reader

Enable `paimon` when reading a Paimon baseline. The caller schedules logical
`(partition, bucket)` splits; the default reader opens lake files, reads the
bounded append log tail, and applies exact supported predicates. PK union
execution is not yet available.

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
    // Execute each plan with its own matching scan.new_reader().
    Ok(())
}
```

### Engine-native execution

`prepare()` and context transport do not open a lake catalog and do not require
the `paimon` feature. A native adapter consumes the full Fluss schema, table
identity, pinned lake snapshot ID, partition/bucket layout, and half-open log
ranges. Engine expressions, physical file tasks, runtime handles, credentials,
and memory pools stay outside this context.

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
not provide SR FE/BE bindings, a DataFusion provider, or a scheduler. Native
engines implement reconciliation under the same semantics.

## Limits and compatibility

- A context is neither a global transactional snapshot nor a retention lease.
  Offsets are captured per bucket and cannot be compared across buckets.
- Full-table preparation currently costs offset RPCs for all live partitions,
  even when a later scan selects only one. It avoids unsafe reuse of a pruned
  context. Scoped contexts would need an explicit coverage contract.
- Default plans use one logical split per selected `(partition, bucket)`.
  Native plans may use file/row-group tasks and engine-controlled parallelism.
- The optional backend uses Paimon 0.3 and supports Parquet. Its Arrow 58 batches
  cross into workspace Arrow 59 through the Arrow C Data Interface.
- Context version 1 is separate from the default split descriptor version.
  Unknown versions fail explicitly. Receive contexts from trusted coordinators;
  decoding validates structure, not authenticity or retention.
- Source table modification time is checked conservatively along with schema
  and layout. Runtime Fluss cluster and lake catalog/table mappings must remain
  consistent; the context does not authenticate a catalog.
- Snapshot selection is source-owned. There is no arbitrary historical
  `with_snapshot_id`, target-parallelism knob, or public memory-pool API.

## Tests

Run from the Rust workspace:

```bash
cargo test -p fluss-lake
cargo test -p fluss-lake --features paimon
cargo test -p fluss-lake --features integration_tests --test test_union_read
```

The `lake-msrv` CI job checks default and all-feature targets on Rust 1.91.0
and runs the Paimon library unit tests. Its all-feature check compiles, but
does not execute, service-backed integration tests.

The `integration_tests` feature requires Docker for the Fluss log-only tests.
If the Docker VM cannot bind-mount the worktree, set
`FLUSS_RUST_UNION_READ_TEST_DATA_DIR` to an absolute directory shared with that VM.

Real tiering interoperability is tested separately by `RustUnionReadITCase` in
`fluss-lake-paimon`. Java owns the Fluss cluster, Flink MiniCluster, temporary
Paimon warehouse, baseline writes and log tail. A precompiled Rust test discovers
the readable snapshot and log boundaries through the public UnionRead API and
reads the same local warehouse. The append scenario checks both sides, transported split retries and lake-only
reads. PK union coverage is added with PK execution in PR7.
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
