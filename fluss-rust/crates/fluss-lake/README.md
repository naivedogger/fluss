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

# Bounded UnionRead source preparation

This stage provides a portable, table-wide source context. It does not provide
lake file planning or a default reader yet. No Paimon dependency is resolved.

```rust,no_run
use fluss_lake::{FlussLakeReadContext, FlussLakeTable, Result};
async fn prepare(table: &FlussLakeTable) -> Result<()> {
    let context = table.prepare().await?;
    let received = FlussLakeReadContext::from_json(&context.to_json()?)?;
    assert_eq!(context.to_json()?, received.to_json()?);
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

## Source contract limitations

The context is not a global transactional snapshot or a retention lease.
Preparation queries offsets for all live partitions before pruning. Offsets
are comparable only within one bucket. Transport validates structure, not
origin or authenticity; only accept contexts from trusted coordinators.

Run `cargo test -p fluss-lake` for context validation and source-boundary tests.
The `integration_tests` feature adds a preparation-only Docker/Fluss test;
it does not require a Paimon reader or a Java tiering job.
