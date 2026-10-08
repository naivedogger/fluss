<!-- SPDX-License-Identifier: Apache-2.0 -->

# FIP-48: Umbrella and child issue drafts

Updated: October 8, 2026.

This file contains one umbrella issue and five child issues. The English Title and Body sections are ready to copy into GitHub after replacing the issue-number placeholders. The coordination notes below are for the two contributors, not part of the public issue bodies.

## Coordination notes

### Implementation reference and publication status

The complete PoC is on [`fip48-rust-union-read`](https://github.com/naivedogger/fluss/tree/fip48-rust-union-read). These drafts describe the integrated implementation, including the unified scan-created reader, independent Append lake/log tasks, test cleanup and documentation fixes.

The five existing split branches have not been regenerated from this version. Use the complete PoC for integrated review, then extract and validate the five incremental PRs separately. Confirm the remote branch matches the intended commit before sharing its link.

The PoC is a review reference, not an upstream merge. The execution-extension clarifications still need to be reconciled with the published FIP and discussed with the community.

### Ownership and merge order

Each child issue corresponds to one implementation PR. Keep tests, documentation and the CI changes with the capability they verify; do not create additional test-only or integration-only PRs for this milestone.

| Issue / PR | Responsibility | Owner | Priority | Merge prerequisite |
| --- | --- | --- | --- | --- |
| U1 / PR1 | Shared schema-bound predicates and exact Arrow filtering | Colleague | P0 | None |
| U2 / PR2 | Reusable source boundaries and backend-free preparation | You | P0 | U1 |
| U3 / PR3 | LakeSource, Paimon backend and default planning | You | P0 | U2 |
| U4 / PR4 | Append/lake-only reader and tiering integration tests | Colleague | P0 | U3 |
| U5 / PR5 | Deduplicate current view and PK UnionRead | You | P0 | U4 |

All five capabilities have PoC implementations; incremental branch extraction, cross-review and upstream acceptance remain pending. Replace the owner labels with agreed GitHub accounts when assigning issues. The 3/2 split assigns submission responsibility, not equal line counts. U3 includes substantial backend and dependency work.

```text
main -> U1 -> U2 -> U3 -> U4 -> U5
```

This is the merge order, not a requirement to work sequentially. Agree on the handoff contracts first, then review U4's executor and U5's pure reconciliation algorithm in parallel. Reuse main's per-partition bucket metadata and routing; there is no sixth task to implement them again.

### Handoff contracts

| Producer -> consumer | Interface | Contract |
| --- | --- | --- |
| U1 -> U2/U3/U4/U5 | BoundPredicate binding, referenced fields and Arrow evaluation | Field indexes refer to the full table schema. Retain hidden filter columns until exact evaluation. Pushdown does not replace exact filtering. |
| U2 -> U3/native engines | FlussLakeReadContext and FlussLakeLogRange | Freeze table/schema identity, physical lake table path, readable snapshot and table-wide log ranges. Query settings and runtime resources stay outside the context. |
| U3 -> U4/U5 | LakeSource::plan/read and LakeSplit | Backend owns payloads and lake semantics. Append tasks are independently readable; a PK reconciliation baseline must resolve versions/deletes across its complete bucket group. |
| U3 -> U4 | FlussLakeReadPlan::splits/schema/statistics | Original frozen tasks, final output schema and estimates. No execution-side source instance or owning scan is stored in the plan. |
| Caller -> U4 | Matching scan settings and original splits | Local callers and workers use scan.new_reader(). Preserve projection, filter, mode, batch size and compatible source/catalog mapping. Workers do not replan. |
| U4 -> U5 | Shared executor and final output processing | Reuse bounded log reading, lazy I/O, hidden columns, exact filtering, projection, cancellation and terminal-error handling. Do not add a second PK reader API. |
| U5 -> U4 executor | DeduplicateCurrentView | Key positions refer to the physical Arrow schema. Fold the ordered tail, suppress overwritten baseline keys and emit tail survivors once, then apply the full filter. |

Shared files such as `table.rs`, `planner.rs`, `plan.rs` and the README should move through this stack with their corresponding capability. U4 must explicitly reject PK union execution until U5 adds it. Each intermediate PR must expose working functionality, without placeholder readers or `todo!()` paths.

### Publication checklist

- [ ] Sync the complete PoC branch before sending its remote link.
- [ ] Agree on owners and the handoff contracts above.
- [ ] Create the umbrella, then the five child issues.
- [ ] Replace `#UMBRELLA_ISSUE` and `#U1_ISSUE` through `#U5_ISSUE`.
- [ ] Extract five incremental PRs from the current implementation and validate each layer.
- [ ] Each PR closes its own child issue and references the umbrella.
- [ ] Keep dependent PRs in Draft while prerequisites are outstanding; explain cumulative diffs.
- [ ] After each prerequisite merges, rebase the next incremental PR and check its scope.
- [ ] Close the umbrella only after all children and the full acceptance criteria are complete.

The complete PoC branch is for integrated review. It is not a sixth PR that duplicates the five incremental PRs.

---

## Umbrella issue

### Title

[Umbrella][FIP-48][rust] Add extensible bounded UnionRead

### Body

#### Motivation

Rust consumers need a bounded view of a lake-enabled Fluss table: a readable lake snapshot plus its complementary Fluss log tail. Primary-key tables also need reconciliation of updates and deletes.

Some consumers need a complete Rust reader. Engines with their own lake readers and execution infrastructure need to reuse the same source boundaries without adopting the default file reader or scheduler.

Implement FIP-48 with a complete default reader and two extension levels: replace lake planning/reading through `LakeSource`, or use a frozen `FlussLakeReadContext` for engine-native execution.

Reference: [FIP-48](https://cwiki.apache.org/confluence/spaces/FLUSS/pages/444334625/FIP-48+Introduce+a+Union+Read+Kernel+for+fluss-rust). The PoC's execution-extension clarifications are proposed amendments; this issue does not imply that they have already received community approval.

#### Design

```text
FlussLakeTable -> FlussLakeScan -> plan() -> Plan -> Splits
                       |                            |
                       +-> new_reader() -> Reader <-+
                       |                   -> final Arrow batches
                       +-> with_lake_source()
                           LakeSource::plan/read

Advanced: table.prepare() -> FlussLakeReadContext
                              |                 |
                     plan_with_context()   engine-native execution
```

The default path preserves the table/scan/plan/reader structure. Local callers and distributed workers use the same `scan.new_reader()` API and execute the original tasks with matching scan settings. Readers do not retain an owning plan or replan on workers.

A `LakeSource` owns lake planning, versioned task payloads and baseline reading. The default executor retains Fluss log reading, PK reconciliation and final filtering. A replacement reader must understand its backend's payload and baseline contract; raw concatenation of PK files is insufficient.

Native engines may use `ReadContext` and own physical planning, reading, reconciliation and scheduling. They must preserve the frozen lake/log boundaries and equivalent result semantics. They do not decode the default reader's private split payloads.

#### Child issues

- [ ] #U1_ISSUE: Share schema-bound predicates and exact Arrow filtering.
- [ ] #U2_ISSUE: Add reusable source boundaries and backend-free preparation.
- [ ] #U3_ISSUE: Add extensible lake sources and pinned Paimon planning/reading.
- [ ] #U4_ISSUE: Add Append/lake-only execution and real-tiering integration coverage.
- [ ] #U5_ISSUE: Add deduplicate current views and PK UnionRead execution.

These form five implementation PRs in the order above. Each PR closes its own child issue and references this umbrella. Close the umbrella after integrated acceptance, rather than automatically from the last PR.

#### Acceptance criteria

- [ ] Source preparation freezes table/schema identity, the physical lake table path, the readable snapshot and half-open log ranges `[start, stop)` independently of projection/filter.
- [ ] Context reuse and task transport preserve those boundaries; later writes do not advance the captured stops.
- [ ] Custom lake sources work without the Paimon feature and can retain the default log reader and reconciliation.
- [ ] Local and worker execution use one scan-created reader without retaining or rebuilding the plan.
- [ ] Append lake tasks and nonempty bucket tails are independently schedulable; lake-only expired partitions are included.
- [ ] PK updates, deletes and inserts produce a deduplicate current view, with mutable-column filters applied after reconciliation.
- [ ] Hidden key/filter columns do not leak into output; zero-column batches preserve row counts.
- [ ] Selected ranges, routing and pruning use actual partition bucket counts, including old/new partitions after a default-count change.
- [ ] Missing required frozen inputs fail the query attempt instead of refreshing boundaries or silently omitting data.
- [ ] Credentials remain local; unsupported transport/payload versions and incompatible source identities/layouts fail explicitly.
- [ ] Default and Paimon-enabled builds satisfy the lake crate's declared Rust 1.91 MSRV without raising the core workspace declaration.
- [ ] Java-owned real tiering explicitly executes the Rust verifier; the final suite requires all six Append/PK scenarios with no skips.
- [ ] Public documentation states caller responsibilities, integration choices and unsupported behavior.

#### Scope and limitations

The first milestone covers bounded Append UnionRead, lake-only reads and deduplicate PK UnionRead. The initial backend uses Paimon 0.3 and Parquet, bridging Paimon's Arrow 58 batches to workspace Arrow 59 through the Arrow C Data Interface. Other lake formats have an extension point, not a completed backend.

Append lake tasks may use a different fixed bucket layout from Fluss or be bucket-unaware. Default PK lake/log reconciliation requires aligned logical partition/bucket ownership and a unique live baseline row per full key. Changing the table's default bucket count affects new partitions; this work does not redistribute existing partitions. Core per-partition routing is reused from main.

Contexts are neither global transactional snapshots nor retention leases. Transported splits carry frozen source work, not complete worker requests. The caller restores matching projection/filter/mode/batch size and compatible runtime resources. Identity and structure checks do not authenticate tasks or detect every configuration mismatch.

The default PK overlay is in memory, without spill, a hard memory cap or engine memory-pool integration. PK tasks remain grouped per partition/bucket. If a frozen input disappears, the engine must discard that attempt's output before considering a fresh whole-query plan; successful recovery is not guaranteed.

Streaming UnionRead, additional default PK merge engines, a stable cross-language ABI, SR/DataFusion adapters, retention protection and performance benchmarks are outside this milestone. Lake-only reads may use the backend's supported native snapshot semantics.

---

## U1 / PR1

### Title

[FIP-48][rust] Share schema-bound predicates and exact Arrow filtering

### Body

Parent: #UMBRELLA_ISSUE

#### Purpose

UnionRead needs the same predicate binding rules for source pruning, safe pushdown and exact Arrow evaluation. Reuse the core representation rather than adding a lake-specific expression language.

#### Scope

- Expose `BoundPredicate` and `BoundLiteral` for reuse by the companion lake crate.
- Reuse binding and literal conversion in the existing protobuf predicate path.
- Provide exact Arrow batch evaluation with existing supported type and null semantics.
- Report invalid fields/types explicitly and expose referenced table-field indexes for physical projection.

This is a core-only change. It introduces no lake catalog dependency, PK reconciliation or workspace-wide MSRV change.

#### Dependencies and handoff

No preceding FIP-48 implementation PR is required.

Downstream consumers use `BoundPredicate::bind()`, `referenced_field_indexes()` and `evaluate_batch()`. Referenced indexes address the full table schema; evaluation reads the corresponding physical columns by name. U4/U5 must retain hidden filter columns. U3/U5 decide which parts are safe to apply before PK reconciliation.

#### Acceptance criteria

- [ ] Existing predicate/protobuf behavior remains compatible.
- [ ] Binding and Arrow evaluation cover supported literals, compound predicates, nulls and invalid input.
- [ ] Exact filtering works when physical columns are reordered or contain hidden filter columns.
- [ ] The change remains independent of Paimon and UnionRead execution.

---

## U2 / PR2

### Title

[FIP-48][rust] Add reusable UnionRead source boundaries

### Body

Parent: #UMBRELLA_ISSUE

#### Purpose

Default readers and native engines need the same readable lake snapshot and complementary bounded log ranges. Capture table-wide source state once so multiple scans can reuse it with different filters and projections.

#### Scope

- Expose the required core readable-snapshot and log-boundary metadata primitives.
- Introduce `fluss-lake`, `FlussLakeTable`, scan configuration and backend-free `prepare()`.
- Add `FlussLakeReadContext` and `FlussLakeLogRange` with validated transport.
- Freeze table/schema identity, the resolved physical lake table path, the readable snapshot and complete live-partition bucket ranges.
- Capture partition offsets with bounded concurrency and consume core's resolved per-partition counts and routing.
- Preserve the required start offset even if retention has already passed it; do not reinterpret retained suffixes as complete history.
- Declare Rust 1.91 for the lake package and add default/all-feature MSRV checks without changing core's declaration.
- Add preparation integration tests using the existing Fluss cluster test support.

The context contains no projection, filter, batch size, credentials or lake-file tasks. Preparation opens no lake catalog and works without the Paimon feature.

#### Dependencies and handoff

Depends on #U1_ISSUE.

U3 consumes the context through `scan.plan_with_context(&context)`. Native engines can consume it without the default planner. `log_ranges()` lists live Fluss buckets, not every lake partition: U3 must also discover lake-only expired partitions from the pinned snapshot.

Context transport is V3 in the current PoC. It is an evolving Rust contract, not a stable cross-language ABI. Consumers validate needed log ranges after safe pruning. No lease or global transactional snapshot is implied.

#### Acceptance criteria

- [ ] Preparation requires only Fluss metadata/log APIs, with no lake-file planning or I/O.
- [ ] Context round trips preserve identity, physical lake mapping, schema, snapshot and `[start, stop)` ranges.
- [ ] Malformed versions, duplicate/incomplete coverage and incompatible table metadata fail explicitly.
- [ ] Different scan filters/projections reuse one context, and writes after preparation leave its stops unchanged.
- [ ] Actual old/new partition bucket counts are preserved; unresolved or invalid counts do not fall back to a new table default.
- [ ] Required missing log prefixes are represented and fail when selected, rather than being silently skipped.
- [ ] The backend-free preparation IT runs against a real Fluss cluster.
- [ ] Package-level MSRV checks cover both default and all-feature targets.

---

## U3 / PR3

### Title

[FIP-48][rust] Add extensible lake sources and pinned Paimon planning

### Body

Parent: #UMBRELLA_ISSUE

#### Purpose

Provide a usable default lake backend and allow other Rust consumers to replace lake planning/reading while retaining Fluss boundary selection, log reading and default reconciliation.

#### Scope

- Add `LakeSource`, `LakePlannerContext`, `LakeReaderContext`, `LakeReadSemantics` and versioned `LakeSplit` envelopes.
- Pass immutable request contexts, including `reconcile_primary_key`, rather than shared mutable pushdown state.
- Provide `PaimonLakeSource` with pinned snapshot planning and working baseline reading under the optional Paimon feature.
- Respect the resolved physical lake database/table mapping.
- Add `FlussLakeReadPlan` with frozen tasks, output schema and input estimates.
- Emit one default split per Append lake task and one per nonempty Fluss bucket tail.
- Group the complete baseline and tail per partition/bucket for default PK reconciliation.
- Include lake-only expired partitions and apply only semantically safe pruning/pushdown.
- Bound bucket-pruning candidate/hash work; retain all candidate buckets if the budget is exceeded.
- Bridge Arrow 58/59 and normalize compatible Paimon output types without silent precision loss.

#### Dependencies and handoff

Depends on #U2_ISSUE and consumes U1's bound predicates.

U4 invokes `LakeSource::read()` with the frozen snapshot, original lake tasks, physical projection, expected schema and safe filter. Projection indexes refer to the full table schema and include hidden columns. Returned columns must match the requested order and compatible physical types.

Append tasks must be independently readable. Their lake bucket layout may differ from Fluss, including the bucket-unaware pair `(-1, -1)`; the source owns lake bucket pruning. Fluss bucket pruning applies to log tasks.

For default PK reconciliation, lake tasks must align with Fluss ownership and the entire supplied bucket group must produce at most one live row per full key. The backend handles lake-specific versions, merges, deletes and deletion vectors when supported. The generic executor does not repair raw-file output. `reconcile_primary_key = false` allows lake-only reads to use supported native lake merge semantics.

Payloads belong to the backend, not to a universal file-list protocol. A custom reader can delegate planning to `PaimonLakeSource` only if it implements the same payload and baseline contracts. The current PoC uses default descriptor V4 and Paimon payload V1, separately from context V3.

#### Acceptance criteria

- [ ] A custom source can plan/read without enabling Paimon.
- [ ] Planning and reading use exactly the pinned snapshot and physical lake table mapping, without substitution.
- [ ] Append tasks remain independent across differing/bucket-unaware lake layouts; PK reconciliation rejects conflicting layouts.
- [ ] Expired lake-only partitions use their captured lake layout and are not lost through live-Fluss pruning.
- [ ] PK baselines reconcile the complete supplied lake group without requiring sorted output.
- [ ] Payload round trips exclude credentials and reject unsupported versions.
- [ ] Pruning-budget exhaustion cannot omit matching rows.
- [ ] Real local Parquet tests cover Append/PK baselines, hidden/reordered projections, nulls and representative nested/binary/temporal adaptation.
- [ ] Incompatible types, invalid binary lengths and temporal precision loss fail explicitly.
- [ ] This layer provides usable planning and baseline reading, without an incomplete default UnionRead executor.

---

## U4 / PR4

### Title

[FIP-48][rust] Add Append UnionRead execution and real-tiering coverage

### Body

Parent: #UMBRELLA_ISSUE

#### Purpose

Execute bounded Append and supported lake-only reads through one scan-created reader, usable both locally and on workers receiving transported tasks.

#### Scope

- Add `scan.new_reader()` with immutable settings and no planning or I/O at creation.
- Execute original tasks through `read_split()`, `read_splits()` and `read_splits_with_concurrency()`.
- Read independently planned lake tasks and bounded Fluss tails without coupling their progress.
- Share physical-column handling, exact filtering, final projection and batch sizing.
- Support hidden columns and zero-column batches with correct row counts.
- Add lazy opening, bounded active-task concurrency, drop cancellation and terminal first-error handling that releases siblings.
- Validate task version, structure, source identity and layout; reject duplicate IDs within one task collection.
- Add log-only Docker ITs and Java-owned real-tiering Append tests with a precompiled Rust verifier.
- Add a path-scoped integration workflow and reproducible local instructions.

#### Dependencies and handoff

Depends on #U3_ISSUE and uses U1's exact predicate evaluation.

Local callers and workers use the same `scan.new_reader()` API. The engine delivers the original tasks and restores matching projection, filter, read mode, batch size and compatible source/catalog mapping. Credentials remain local. The reader requires neither the original plan nor its full task list and does not call `prepare()` or `plan()` on workers.

Splits are not complete worker requests. Reader checks do not verify remote configuration equality, authenticate tasks or provide cross-worker exactly-once delivery. Callers must supply trusted tasks and preserve the execution contract.

U5 reuses this executor and output-processing path. This PR supports Append and available lake-only semantics; it explicitly rejects PK union execution until U5 adds reconciliation.

#### Acceptance criteria

- [ ] Local and transported-task reads return the same bounded results after dropping the coordinator plan and scan.
- [ ] A worker test fails if execution invokes lake planning, proving there is no worker-side replan.
- [ ] A blocked lake read does not prevent an independent log task from progressing; same-bucket lake tasks do not duplicate the tail.
- [ ] Empty log polls and fully filtered tails still terminate at the frozen stop.
- [ ] Hidden filter columns are retained until exact filtering and excluded from final output; empty projection preserves row counts.
- [ ] Invalid configuration/tasks and missing required inputs fail; the first stream error is terminal and cancellation releases active siblings.
- [ ] The Java IT tiers real data, stops tiering, writes a log-only tail and explicitly runs Rust against the same local warehouse.
- [ ] Three Append scenarios cover ordinary reads and default-count growth/shrinkage, including old/new live and expired lake-only partitions.
- [ ] Shrinkage includes a nonempty baseline and tail in old buckets above the new default, read locally and after transport.
- [ ] The integration job requires all three Append scenarios with zero skips and rejects zero-test verifier runs.

The ordinary Java suite does not require Rust. The dedicated enabled IT owns the fixture lifecycle; no production helper CLI, copied warehouse or separate fixture-generation script is required.

---

## U5 / PR5

### Title

[FIP-48][rust] Add deduplicate current views and PK UnionRead

### Body

Parent: #UMBRELLA_ISSUE

#### Purpose

Complete bounded PK UnionRead by reconciling the ordered Fluss changelog tail with a current-view lake baseline. Keep the core reconciliation algorithm independent of the lake format.

#### Scope

- Add and export core `DeduplicateCurrentView`, with focused algorithm tests.
- Fold `INSERT`/`UPDATE_AFTER` rows and `DELETE`/`UPDATE_BEFORE` tombstones in per-bucket offset order.
- Suppress baseline rows whose full keys appear in the overlay and emit surviving tail rows once.
- Integrate reconciliation into U4's reader instead of adding another reader API.
- Retain hidden full-key columns and apply mutable-column predicates after reconciliation.
- Release fully superseded batches and materialize survivor output incrementally.
- Extend the existing tiering IT/verifier with ordinary and mixed-layout PK scenarios.
- Complete documentation of deduplicate semantics, failure handling and memory limitations.

#### Dependencies and handoff

Depends on #U4_ISSUE and U3's current-view baseline contract.

```text
DeduplicateCurrentView::try_new(physical_schema, key_positions)
  -> fold_changelog_batch()       ordered complete tail
  -> reconcile_baseline_batch()  suppress overwritten/deleted keys
  -> finish_batches()            emit surviving tail rows once
  -> shared output processing    exact filter, then final projection
```

Key positions address the physical Arrow schema, not full-table field indexes. Preserve change types and required tail rows even when their mutable columns do not match the final filter. U3 must already resolve lake versions/deletions; this overlay reconciles lake versus log, not individual lake files.

The default PK union path supports deduplicate semantics only. It must not treat partial-update or aggregation merge engines as overwrite semantics. Lake-only reads retain the backend's supported native semantics.

The first implementation uses an in-memory overlay and one combined logical task per selected partition/bucket. It introduces no spill, hard cap, broadcast overlay or bucket-internal parallel merge protocol.

#### Acceptance criteria

- [ ] Core and reader tests cover cross-batch updates/deletes, inserts, composite keys and tombstones.
- [ ] Hidden key columns participate in reconciliation without appearing in the final projection.
- [ ] Mutable-column filters cannot resurrect old or deleted baseline rows.
- [ ] Local and transported-task execution agree on bounded PK results.
- [ ] Unsupported union merge engines, invalid changelog inputs and missing required tail data fail explicitly.
- [ ] Ordinary PK and default-count growth/shrinkage scenarios verify updates, deletes and inserts across the lake/log seam.
- [ ] Old high-numbered buckets after shrinkage retain their baseline and nonempty tail in local and worker execution.
- [ ] The same dedicated workflow now requires six scenarios: ordinary/growth/shrinkage for both Append and PK, with zero skips.
- [ ] Focused tests retain zero-column output, post-merge filtering and custom-source delegation coverage.
- [ ] The final incremental implementation matches the complete PoC's agreed capability set, without the intermediate Append-only rejection.

The local-warehouse tiering suite does not claim coverage of object storage, real deletion-vector files, missing-file full-chain failures, distributed SR/DataFusion integration or performance benchmarks.
