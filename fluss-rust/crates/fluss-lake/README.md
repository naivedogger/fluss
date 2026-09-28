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

# Plan-bound append and lake-only UnionRead

The default API is Table -> Scan -> immutable Plan -> Reader. This layer reads
append unions and lake-only views. It explicitly rejects PK union execution;
the next PR adds the current-view overlay and removes that restriction.

```rust,no_run
use fluss_lake::{FlussLakeTable, Result};
use futures::TryStreamExt;

async fn read(table: &FlussLakeTable) -> Result<()> {
    let plan = table.new_scan().with_batch_size(4096).plan().await?;
    let batches = plan.new_reader().read_splits(plan.splits()).await?
        .try_collect::<Vec<_>>().await?;
    assert!(batches.iter().all(|batch| batch.schema() == plan.schema()));
    Ok(())
}
```

The reader is created from the plan, never from a separately configured scan.
The plan holds its bound predicate, projection, mode, source and frozen inputs.
Serialized tasks can be retried with that plan. Foreign, modified or duplicate
tasks are rejected. A split alone is not a complete distributed execution plan;
worker-side reconstruction of the default plan is not provided.

`read_splits` activates at most eight logical tasks. A positive override is
available through `read_splits_with_concurrency`. Engines may schedule
`read_split` themselves. The first error is terminal and drops sibling streams;
cancellation drops active reads. Discard the attempt's output before replanning.

Exact filtering precedes removal of hidden columns. An empty output projection
returns zero-column batches with correct row counts, not a metadata-only count.
Live metadata is checked against the frozen context before reading. An append
read concatenates the pinned lake baseline and bounded complementary log tail.
Lake-only execution ignores the unused tail, including its captured retention gap.

Enable `paimon` for the default `PaimonLakeSource`, or inject a `LakeSource` through
`with_lake_source`. A backend receives immutable contexts, a pinned snapshot and
safe predicates. It owns its versioned lake task payloads. PK lake-only reads
require a resolved current-view baseline across each supplied partition/bucket
group, not raw concatenation of files. No other production lake backend is included.

For native execution, `table.prepare()` supplies a table-wide source context
without opening a lake catalog. The engine owns lake tasks, reconciliation and
scheduling. It must use the pinned snapshot, include selected lake-only partitions,
validate log availability and reject missing inputs. No SR/DataFusion adapter is
included. The default layout remains Fluss partition/bucket aligned.

`fluss-lake` declares Rust 1.91 independently of the core workspace. Paimon 0.3
uses Arrow 58; the backend imports batches into Arrow 59 via the C Data Interface.
Context transport V2, private logical descriptors V3 and backend payload versions
are independent. Credentials remain runtime-only. Reading is bounded; contexts
are not a retention lease or a global transactional snapshot.

Different live partitions retain their actual bucket counts after changing the
table default. Preparation and routing use each partition's count, as does bucket
pruning. Lake tasks carry their partition counts, including expired lake-only
partitions; conflicting layouts fail rather than silently lose data. Existing
partitions are not redistributed.

## Tests

Run from the Rust workspace:

```bash
cargo +1.91.0 test -p fluss-lake --features paimon --lib --locked
cargo +1.91.0 test -p fluss-lake --features integration_tests --test test_prepare --test test_union_read --locked
```

The service ITs use the existing Docker cluster utilities. Real tiering is tested
by Java's `RustUnionReadITCase`, reusing `FlinkPaimonTieringTestBase`. Java creates
the baseline, tiers it through Flink into a local Paimon warehouse, stops the job,
writes a log-only tail and invokes a precompiled Rust verifier. Rust discovers
snapshot and log boundaries through the public API. The append scenario covers
transport/retry, lake-only reads, empty projection, custom source delegation and
invalid-task rejection.

The path-scoped `Rust UnionRead Integration` workflow builds both runtimes and
runs three append scenarios (ordinary, default-count growth and shrinkage).
Mixed-layout cases include old live, new and expired lake-only partitions.
It asserts three executed Java scenarios and a nonempty
Rust test run. The verifier is ignored in standalone Cargo runs; ordinary Java
tests do not require Rust. The next PR adds PK coverage and raises the scenario
count to six. This suite does not cover object-storage or missing-file full chains.
