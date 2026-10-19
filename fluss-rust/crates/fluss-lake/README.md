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

`fluss-lake` provides reusable source boundaries, a `LakeSource` interface and default split planning. The combined lake/log executor is delivered separately; this layer supports lake baseline reads through `LakeSource::read`.

Use `FlussLakeTable::prepare()` to freeze source inputs. Inject a source with `scan.with_lake_source(source)` and call `scan.plan()` or `scan.plan_with_context(&context)`. The resulting plan exposes splits, output schema and estimates, without retaining a reader. Native engines may consume the context directly instead of using the default split representation.

A lake source owns its task payload and baseline semantics. Append tasks are independent of each other and of nonempty log tails. For PK reconciliation, the complete supplied bucket group must produce at most one live row per full key, with lake versions and deletes already resolved. PK tasks retain their complete baseline and tail together. Lake-only expired partitions are included. Pruning is conservative and does not replace exact result filtering.

Contexts and tasks carry frozen work, not credentials or a complete worker configuration. Callers must preserve matching filters, projection, read mode and runtime mappings. Neither context capture nor task planning acquires a retention lease. Transport is an evolving Rust contract, not a stable cross-language ABI.

The crate requires Rust 1.91. Run `cargo test -p fluss-lake --lib` for source-contract and planner tests.

At this layer a caller must inject a `LakeSource`; no default format backend is installed.
