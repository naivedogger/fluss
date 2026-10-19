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

# Bounded Append UnionRead

`fluss-lake` reads a frozen lake snapshot and complementary bounded Fluss log tails. Append and supported lake-only reads are available. PK union execution is explicitly rejected until reconciliation is added.

```rust,no_run
# use fluss_lake::FlussLakeTable;
# async fn read(table: &FlussLakeTable) -> fluss_lake::Result<()> {
let scan = table.new_scan().with_projection(vec![0]);
let plan = scan.plan().await?;
let reader = scan.new_reader();
let stream = reader.read_splits(plan.splits()).await?;
# Ok(())
# }
```

Local callers and workers use the same scan-created reader. Workers receive original transported splits and restore matching filters, projection, read mode, batch size and compatible runtime lake-source mappings. They do not replan or retain the coordinator plan. Credentials stay local. Task validation does not authenticate input or detect every configuration mismatch.

Append lake tasks and nonempty Fluss bucket tails are independently schedulable. The reader supports bounded task concurrency, lazy I/O, cancellation and terminal errors. Hidden filter columns are retained until exact filtering; an empty output projection preserves row counts. Missing required frozen inputs fail the attempt instead of refreshing the snapshot or skipping data.

Enable `paimon` for the Paimon 0.3 Parquet backend, or inject `LakeSource` for another compatible implementation. Source preparation works without Paimon. Contexts are not global transactional snapshots or retention leases, and their transport is not a stable cross-language ABI. The crate requires Rust 1.91 independently of core.

Run `cargo test -p fluss-lake --features paimon --lib` for unit and local Parquet tests. Fluss cluster tests use the `integration_tests` feature. The existing client-integration workflow contains a change-scoped `rust-union-read` job: it precompiles the Rust verifier, then runs the Java-owned `RustUnionReadITCase` fixture against the same local warehouse. This layer requires three Append scenarios (ordinary, partition default-count growth and shrinkage), with zero skips. The ordinary Java suite does not require Rust.
