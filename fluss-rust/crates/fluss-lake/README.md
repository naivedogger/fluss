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

# Bounded UnionRead planning

The portable `prepare()` path remains lake-backend-independent. This stage
adds `plan()` and `plan_with_context()` with pinned Paimon file planning,
opaque logical splits, statistics and conservative pruning. No default reader
is exposed yet. Enable `paimon` to plan a nonempty lake baseline.

## Rust version

`fluss-lake` requires Rust 1.91 or later. This package-level minimum applies
with or without the optional `paimon` feature, including preparation-only
integrations. Disabling Paimon avoids its dependencies, not this compiler
requirement. The core workspace's MSRV and the development `stable` toolchain
are unchanged.

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
    // Default execution is added in the next stage.
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
not provide SR FE/BE bindings, a DataFusion provider, or a scheduler. Native engines implement reconciliation under the same semantics.
