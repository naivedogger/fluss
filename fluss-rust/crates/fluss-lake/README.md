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

# UnionRead lake sources and planning

This layer adds `scan.plan()` and `plan_with_context()` to the source contract.
Plans expose a frozen context, output schema, logical partition/bucket tasks and
best-effort statistics. Readers over complete UnionRead plans follow in the next
PR; this layer has no `new_reader()` API or execution placeholders.

```rust,no_run
use fluss_lake::{FlussLakeTable, Result};

async fn plan(table: &FlussLakeTable) -> Result<()> {
    let context = table.prepare().await?;
    let plan = table.new_scan().with_projection(vec![0])
        .plan_with_context(&context).await?;
    assert_eq!(plan.read_context().to_json()?, context.to_json()?);
    Ok(())
}
```

`scan.with_lake_source(Arc<dyn LakeSource>)` selects a backend. Like Java FIP-6,
the source owns lake planning, task payloads and reading. Rust passes immutable
`LakePlannerContext` and `LakeReaderContext` rather than mutable pushdown state.
The source can be used directly to plan and read its lake baseline; it does not
read the Fluss tail or perform UnionRead reconciliation in this layer.

Enable `paimon` for `PaimonLakeSource`. It plans the exact pinned snapshot and
reads complete partition/bucket task groups, checks layout and deduplicate lake
semantics, and imports Paimon Arrow 58 into workspace Arrow 59 through the C Data
Interface. Parquet is the supported file format. Without `paimon`, a custom
source can implement the same contract without pulling in Paimon dependencies.

A primary-key baseline must contain at most one live row per full key after lake
version/deletion resolution. It need not be sorted. Custom sources must own a
versioned payload codec, reject unknown versions, and keep credentials out of
payloads. These are Rust extension points, not a stable binary ABI or a universal
file-list protocol. Tasks must preserve Fluss partition/bucket ownership.

Context JSON is V2; private logical split descriptors are V3; lake payloads have
backend-owned versions. Source boundaries are not retention leases. Missing
required inputs must fail rather than refresh the snapshot. Preparation does
not apply a scan filter to its table-wide context. Default planning may prune
selected buckets and includes lake-only partitions absent from live Fluss.

Each live partition keeps its captured bucket count after a table-default
change. Lake tasks carry their actual partition count, including expired
lake-only partitions. The planner validates live/lake and sibling consistency,
and uses one bucket pruner per distinct count. Paimon verifies payload counts
against task envelopes rather than treating its current default as every
partition's layout. Existing partitions are not redistributed.

`fluss-lake` uses Rust 1.91 with or without the optional Paimon 0.3 backend.
The core workspace MSRV is unchanged. Run from the Rust workspace:

```bash
cargo +1.91.0 test -p fluss-lake --locked
cargo +1.91.0 test -p fluss-lake --features paimon --locked
cargo +1.91.0 clippy -p fluss-lake --all-features --all-targets --locked -- -D warnings
```
