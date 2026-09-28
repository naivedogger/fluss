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

# Bounded UnionRead source context

This layer exposes source preparation without a lake backend. It captures the
readable lake snapshot and table-wide, half-open Fluss log ranges before query
pruning. A context is not a retention lease or a global transactional snapshot.

```rust,no_run
use fluss_lake::{FlussLakeTable, FlussLakeReadContext, Result};

async fn capture(table: &FlussLakeTable) -> Result<()> {
    let context = table.prepare().await?;
    let received = FlussLakeReadContext::from_json(&context.to_json()?)?;
    for range in received.log_ranges() {
        range.validate_available()?;
    }
    Ok(())
}
```

Native engines must validate live table identity, use the exact captured lake
snapshot, include selected lake-only partitions, and reject unavailable required
logs. Without a baseline the required log start is zero, not the earliest retained
offset. Retry the whole attempt on missing frozen inputs rather than mixing bounds.

This commit has no default lake planner or UnionRead reader. Those capabilities
follow in separate PRs; no public methods return unimplemented placeholders.
The new lake crate declares Rust 1.91. Core and binding MSRV declarations stay
unchanged. There is no Paimon dependency in this layer.

The V2 context carries each live partition's actual bucket count; the table
count is only the default for new partitions. Preparation captures every bucket
using its partition's range, including old and new layouts after a default
change. Legacy metadata without a count uses the table default. Invalid counts
and inconsistent or incomplete transported layouts fail validation. V1 contexts
are rejected rather than interpreted with different semantics. Core metadata
preserves routing counts and offset/fetch RPCs send the actual partition count.

Run from the Rust workspace:

```bash
cargo +1.91.0 test -p fluss-lake --locked
cargo +1.91.0 test -p fluss-lake --features integration_tests --test test_prepare --locked
```

The integration test uses the existing Docker Fluss cluster utilities and needs
no shared warehouse mount or running tiering job.
