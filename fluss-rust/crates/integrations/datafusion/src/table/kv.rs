// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! KV `TableProvider`.
//!
//! Surfaces a primary-keyed Fluss table to DataFusion with three read shapes, in
//! precedence order:
//!
//! 1. **Point lookup** — a COMPLETE primary-key equality (`pk1 = v AND ...`)
//!    resolves a single row via [`FlussKvLookupExec`].
//! 2. **Prefix lookup** — a COMPLETE bucket-key prefix equality (the bucket keys,
//!    plus every partition key when the table is partitioned) resolves every row
//!    whose primary key starts with that prefix via [`FlussKvPrefixLookupExec`].
//!    Applies only when the bucket key is a STRICT prefix of the physical primary
//!    key; a partitioned table additionally REQUIRES equality on every partition
//!    column (the client cannot route a prefix lookup without it).
//! 3. **Bounded scan** — any other predicate shape combined with a `LIMIT` runs a
//!    bounded scan (the same machinery the log table uses), reading each bucket's
//!    last-`limit` rows; DataFusion layers the final cross-target `LIMIT`. For a
//!    partitioned table, partition-column equality prunes the scanned partitions.
//! 4. **Full scan** — any other predicate shape with NO `LIMIT` runs a full-table
//!    scan via [`FlussKvFullScanExec`], merging each target bucket's CDC changelog
//!    into its current state (changelog-only: no kv-snapshot RPC, no RocksDB).
//!    The same `(partition, bucket)` target computation as the bounded scan is
//!    used, so partition-column equality prunes the scanned partitions. Its
//!    completeness is bounded by changelog retention.
//!
//! `supports_filters_pushdown` is precedence-aware so it is sound: a filter is
//! `Exact` (consumed, no residual `FilterExec`) ONLY when the matching exec will
//! actually apply it — a complete PK equality (point lookup) or a complete
//! bucket-key prefix equality (prefix lookup). Otherwise the bounded-scan / full-
//! scan path runs, which does NOT apply arbitrary filters, so partition equality
//! is only `Inexact` (drives pruning, residual `FilterExec` re-applies) and
//! everything else is `Unsupported`.

use std::sync::Arc;

use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use datafusion::catalog::{Session, TableProvider};
use datafusion::error::Result as DfResult;
use datafusion::logical_expr::{TableProviderFilterPushDown, TableType};
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::Expr;

use crate::source::{SharedFlussSource, TableRef};
use crate::execution::kv_full_scan::FlussKvFullScanExec;
use crate::execution::log_scan::FlussLogScanExec;
use crate::execution::lookup::FlussKvLookupExec;
use crate::execution::prefix_lookup::FlussKvPrefixLookupExec;
use crate::table::predicate::{
    analyze_kv_filters, analyze_kv_prefix_filters, analyze_partition_filters,
    is_complete_prefix_key_equality, is_complete_primary_key_equality,
    is_partition_equality, is_prefix_key_equality, is_primary_key_equality,
};
use crate::types::record_batch::normalize_projection;

/// `EXPLAIN`/`name()` label for the KV bounded-scan plan, distinct from the log
/// table's `FlussLogScanExec` so the two read shapes are distinguishable.
const KV_SCAN_PLAN_NAME: &str = "FlussKvScanExec";

/// A primary-keyed Fluss table backed by a point lookup or a bounded `LIMIT` scan.
pub(crate) struct FlussKvTableProvider {
    source: SharedFlussSource,
    table_ref: TableRef,
    schema: SchemaRef,
    primary_keys: Vec<String>,
    /// Bucket-key column names, in bucket-key order. When this is a STRICT prefix
    /// of the physical primary key, the table supports the prefix-lookup read
    /// shape.
    bucket_keys: Vec<String>,
    /// Bucket count = number of bounded-scan partitions read in parallel (per
    /// partition).
    num_buckets: i32,
    /// Partition-key column names; empty when the table is not partitioned.
    partition_keys: Vec<String>,
}

impl std::fmt::Debug for FlussKvTableProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlussKvTableProvider")
            .field("table_ref", &self.table_ref)
            .field("primary_keys", &self.primary_keys)
            .finish()
    }
}

impl FlussKvTableProvider {
    pub(crate) fn new(
        source: SharedFlussSource,
        table_ref: TableRef,
        schema: SchemaRef,
        primary_keys: Vec<String>,
        bucket_keys: Vec<String>,
        num_buckets: i32,
        partition_keys: Vec<String>,
    ) -> Self {
        Self {
            source,
            table_ref,
            schema,
            primary_keys,
            bucket_keys,
            num_buckets,
            partition_keys,
        }
    }
}

#[async_trait]
impl TableProvider for FlussKvTableProvider {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DfResult<Vec<TableProviderFilterPushDown>> {
        // Precedence-aware and sound: only mark a filter `Exact` (planner drops the
        // matching `FilterExec`) when the matching lookup plan will actually apply
        // it.
        //
        // 1. If the FULL set forms a complete PK equality, a point lookup runs and
        //    consumes exactly the PK-equality conjuncts.
        // 2. Else if the FULL set forms a complete bucket-key prefix equality, a
        //    prefix lookup runs and consumes exactly the prefix-key conjuncts
        //    (partition keys + bucket keys).
        // 3. Else the bounded-scan / full-scan path runs, which does NOT apply
        //    arbitrary filters — so nothing may be `Exact`. There, partition
        //    equality is only `Inexact` (drives pruning; the residual `FilterExec`
        //    re-applies it), and everything else (including partial PK / prefix
        //    equality) is `Unsupported`.
        if is_complete_primary_key_equality(filters, &self.primary_keys) {
            return Ok(filters
                .iter()
                .map(|f| {
                    if is_primary_key_equality(f, &self.primary_keys) {
                        TableProviderFilterPushDown::Exact
                    } else {
                        TableProviderFilterPushDown::Unsupported
                    }
                })
                .collect());
        }

        if is_complete_prefix_key_equality(
            filters,
            &self.bucket_keys,
            &self.partition_keys,
            &self.primary_keys,
        ) {
            return Ok(filters
                .iter()
                .map(|f| {
                    if is_prefix_key_equality(f, &self.bucket_keys, &self.partition_keys) {
                        TableProviderFilterPushDown::Exact
                    } else {
                        TableProviderFilterPushDown::Unsupported
                    }
                })
                .collect());
        }

        // Bounded-scan path. A non-partitioned KV table supports no pushdown.
        if self.partition_keys.is_empty() {
            return Ok(filters
                .iter()
                .map(|_| TableProviderFilterPushDown::Unsupported)
                .collect());
        }
        // Partitioned: partition-column equality drives pruning, reported `Inexact`
        // so DataFusion still layers a `FilterExec` re-applying the predicate.
        Ok(filters
            .iter()
            .map(|f| {
                if is_partition_equality(f, &self.partition_keys) {
                    TableProviderFilterPushDown::Inexact
                } else {
                    TableProviderFilterPushDown::Unsupported
                }
            })
            .collect())
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        let (projection, projected_schema) = normalize_projection(projection, &self.schema)?;

        // 1. A COMPLETE primary-key equality => point lookup. `analyze_kv_filters`'s
        //    `Err` is the "not a full PK" signal; do NOT propagate it here, fall
        //    through to the prefix-lookup / bounded-scan / error branches instead.
        if let Ok(key) = analyze_kv_filters(filters, &self.primary_keys) {
            return Ok(Arc::new(FlussKvLookupExec::new(
                self.source.clone(),
                self.table_ref.clone(),
                key,
                projected_schema,
                projection,
            )));
        }

        // 2. Else a COMPLETE bucket-key prefix equality => prefix lookup. The
        //    required columns are the bucket keys, plus every partition key when
        //    the table is partitioned. A full PK equality was already checked
        //    above, so the point-lookup path always wins on overlap.
        if let Some(prefix) = analyze_kv_prefix_filters(
            filters,
            &self.bucket_keys,
            &self.partition_keys,
            &self.primary_keys,
        ) {
            return Ok(Arc::new(FlussKvPrefixLookupExec::new(
                self.source.clone(),
                self.table_ref.clone(),
                prefix.lookup_columns,
                prefix.key,
                projected_schema,
                projection,
            )));
        }

        // 3. & 4. Any other predicate shape => a scan across the `(partition,
        //    bucket)` targets, computed exactly like the log table. A `LIMIT`
        //    selects the bounded scan (each bucket's last-`limit` rows); its
        //    absence selects the unbounded KV full scan (changelog merge to
        //    current state).
        //
        // Compute the scan targets: one (partition_id, bucket) per DataFusion
        // output partition (mirrors `FlussLogTableProvider::scan`).
        let buckets = 0..self.num_buckets;
        let targets: Vec<(Option<i64>, i32)> = if self.partition_keys.is_empty() {
            // Non-partitioned: one target per bucket, no partition id.
            buckets.map(|b| (None, b)).collect()
        } else {
            // Partitioned: list partitions, keep those matching every equality
            // binding (no bindings => keep all), then cross with the buckets.
            let partitions = self.source.list_partitions(&self.table_ref).await?;
            let bindings = analyze_partition_filters(filters, &self.partition_keys);
            partitions
                .into_iter()
                .filter(|partition| {
                    bindings.iter().all(|(k, v)| {
                        partition
                            .values
                            .iter()
                            .any(|(pk, pv)| pk == k && pv == v)
                    })
                })
                .flat_map(|partition| {
                    buckets
                        .clone()
                        .map(move |b| (Some(partition.partition_id), b))
                })
                .collect()
        };

        // No target (partitioned table with zero partitions, or none matched the
        // predicate) means zero rows: return an `EmptyExec` with the projected
        // schema rather than a 0-partition plan.
        if targets.is_empty() {
            return Ok(Arc::new(EmptyExec::new(projected_schema)));
        }

        match limit {
            // 3. Per-target pushdown of `limit` returns each bucket's last-`limit`
            //    rows; DataFusion layers a global `LIMIT` above this multi-partition
            //    scan.
            Some(limit) => Ok(Arc::new(FlussLogScanExec::new(
                self.source.clone(),
                self.table_ref.clone(),
                projection,
                Some(limit),
                false,
                targets,
                projected_schema,
                KV_SCAN_PLAN_NAME,
            ))),
            // 4. No complete PK/prefix equality and no `LIMIT`: full KV scan,
            //    merging each target bucket's changelog into its current state.
            None => Ok(Arc::new(FlussKvFullScanExec::new(
                self.source.clone(),
                self.table_ref.clone(),
                projection,
                targets,
                projected_schema,
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::RecordBatch;
    use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
    use datafusion::logical_expr::TableProviderFilterPushDown as PD;
    use datafusion::prelude::{col, lit, Expr};

    use super::*;
    use crate::source::{FlussPartition, FlussSource, FlussTableMeta, LookupKey};
    use crate::error::Result as CrateResult;

    /// A do-nothing source: `supports_filters_pushdown` is sync and never touches
    /// the source, so these unit tests need no real Fluss access.
    struct StubSource;

    #[async_trait]
    impl FlussSource for StubSource {
        async fn list_databases(&self) -> CrateResult<Vec<String>> {
            Ok(vec![])
        }
        async fn list_tables(&self, _database: &str) -> CrateResult<Vec<String>> {
            Ok(vec![])
        }
        async fn get_table_meta(&self, _table: &TableRef) -> CrateResult<FlussTableMeta> {
            unreachable!("not exercised by pushdown unit tests")
        }
        async fn lookup(&self, _table: &TableRef, _key: &LookupKey) -> CrateResult<RecordBatch> {
            unreachable!("not exercised by pushdown unit tests")
        }
        async fn prefix_lookup(
            &self,
            _table: &TableRef,
            _lookup_columns: &[String],
            _key: &LookupKey,
        ) -> CrateResult<RecordBatch> {
            unreachable!("not exercised by pushdown unit tests")
        }
        async fn list_partitions(&self, _table: &TableRef) -> CrateResult<Vec<FlussPartition>> {
            Ok(vec![])
        }
        async fn bounded_scan(
            &self,
            _table: &TableRef,
            _partition_id: Option<i64>,
            _bucket: i32,
            _projection: Option<&[usize]>,
            _limit: usize,
        ) -> CrateResult<Vec<RecordBatch>> {
            unreachable!("not exercised by pushdown unit tests")
        }
        async fn log_scan(
            &self,
            _table: &TableRef,
            _partition_id: Option<i64>,
            _bucket: i32,
            _projection: Option<&[usize]>,
            _row_limit: Option<usize>,
        ) -> CrateResult<Vec<RecordBatch>> {
            unreachable!("not exercised by pushdown unit tests")
        }
        async fn kv_full_scan(
            &self,
            _table: &TableRef,
            _partition_id: Option<i64>,
            _bucket: i32,
            _projection: Option<&[usize]>,
        ) -> CrateResult<Vec<RecordBatch>> {
            unreachable!("not exercised by pushdown unit tests")
        }
    }

    fn pks(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn schema(cols: &[&str]) -> SchemaRef {
        Arc::new(ArrowSchema::new(
            cols.iter()
                .map(|c| Field::new(*c, DataType::Int32, true))
                .collect::<Vec<_>>(),
        ))
    }

    fn provider(
        primary_keys: Vec<String>,
        bucket_keys: Vec<String>,
        partition_keys: Vec<String>,
    ) -> FlussKvTableProvider {
        // Schema is irrelevant to `supports_filters_pushdown`; supply a minimal one.
        FlussKvTableProvider::new(
            Arc::new(StubSource),
            TableRef::new("db", "t"),
            schema(&["a", "b", "c"]),
            primary_keys,
            bucket_keys,
            1,
            partition_keys,
        )
    }

    fn pushdown(provider: &FlussKvTableProvider, filters: &[Expr]) -> Vec<PD> {
        let refs: Vec<&Expr> = filters.iter().collect();
        provider.supports_filters_pushdown(&refs).unwrap()
    }

    #[test]
    fn complete_single_pk_equality_is_exact() {
        let p = provider(pks(&["id"]), pks(&["id"]), vec![]);
        let filters = vec![col("id").eq(lit(1i32))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Exact]);
    }

    #[test]
    fn complete_composite_pk_equality_marks_each_pk_filter_exact() {
        let p = provider(pks(&["region", "id"]), pks(&["region"]), vec![]);
        let filters = vec![col("region").eq(lit("us")), col("id").eq(lit(2i32))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Exact, PD::Exact]);
    }

    #[test]
    fn complete_pk_equality_with_extra_non_pk_filter_keeps_extra_unsupported() {
        // The full set is NOT a complete PK equality (the extra `name` conjunct
        // breaks it), so this is the bounded-scan path: nothing is `Exact`.
        let p = provider(pks(&["id"]), pks(&["id"]), vec![]);
        let filters = vec![col("id").eq(lit(1i32)), col("name").eq(lit("x"))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Unsupported, PD::Unsupported]);
    }

    #[test]
    fn partial_composite_pk_without_prefix_lookup_is_unsupported_not_exact() {
        // Soundness guard: a partial PK equality + LIMIT must NOT be `Exact`
        // unless it is also a valid prefix lookup. Here the bucket key equals the
        // full physical PK, so prefix lookup is NOT applicable and the bounded
        // scan path must keep the filter `Unsupported`.
        let p = provider(pks(&["region", "id"]), pks(&["region", "id"]), vec![]);
        let filters = vec![col("region").eq(lit("us"))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Unsupported]);
    }

    #[test]
    fn complete_prefix_equality_is_exact_for_prefix_key_columns() {
        let p = provider(pks(&["c1", "c2"]), pks(&["c1"]), vec![]);
        let filters = vec![col("c1").eq(lit(7i32))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Exact]);
    }

    #[test]
    fn full_pk_still_wins_over_prefix_lookup() {
        // Even though `c1` is also the bucket-key prefix, the complete PK equality
        // must take the point-lookup precedence and mark both PK filters `Exact`.
        let p = provider(pks(&["c1", "c2"]), pks(&["c1"]), vec![]);
        let filters = vec![col("c1").eq(lit(7i32)), col("c2").eq(lit(9i32))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Exact, PD::Exact]);
    }

    #[test]
    fn prefix_lookup_with_extra_non_key_filter_is_not_exact() {
        let p = provider(pks(&["c1", "c2"]), pks(&["c1"]), vec![]);
        let filters = vec![col("c1").eq(lit(7i32)), col("name").eq(lit("x"))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Unsupported, PD::Unsupported]);
    }

    #[test]
    fn partitioned_prefix_lookup_marks_partition_and_bucket_equalities_exact() {
        // Physical PK is `[c1, c2]` after removing the partition key `part`, so the
        // strict bucket-key prefix `[c1]` enables a prefix lookup only when the
        // partition equality is also present.
        let p = provider(pks(&["part", "c1", "c2"]), pks(&["c1"]), pks(&["part"]));
        let filters = vec![col("part").eq(lit("p1")), col("c1").eq(lit(7i32))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Exact, PD::Exact]);
    }

    #[test]
    fn partitioned_prefix_lookup_without_partition_falls_back_to_bounded_scan_rules() {
        let p = provider(pks(&["part", "c1", "c2"]), pks(&["c1"]), pks(&["part"]));
        let filters = vec![col("c1").eq(lit(7i32))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Unsupported]);
    }

    #[test]
    fn non_pk_filter_on_nonpartitioned_table_is_unsupported() {
        let p = provider(pks(&["id"]), pks(&["id"]), vec![]);
        let filters = vec![col("name").eq(lit("x"))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Unsupported]);
    }

    #[test]
    fn partition_equality_on_partitioned_table_is_inexact() {
        // Bounded-scan path on a partitioned table: partition equality drives
        // pruning and is reported `Inexact` (residual FilterExec re-applies it),
        // while a non-PK/non-partition filter stays `Unsupported`.
        let p = provider(pks(&["region", "id"]), vec![], pks(&["region"]));
        let filters = vec![col("region").eq(lit("us")), col("name").eq(lit("x"))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Inexact, PD::Unsupported]);
    }

    #[test]
    fn complete_pk_on_partitioned_table_still_exact() {
        // When the full set is a complete PK equality, the point-lookup path wins
        // even on a partitioned table: PK-equality conjuncts are `Exact`.
        let p = provider(pks(&["region", "id"]), vec![], pks(&["region"]));
        let filters = vec![col("region").eq(lit("us")), col("id").eq(lit(2i32))];
        assert_eq!(pushdown(&p, &filters), vec![PD::Exact, PD::Exact]);
    }
}
