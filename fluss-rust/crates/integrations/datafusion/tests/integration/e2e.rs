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

//! End-to-end path: drives real `ctx.sql(...)` through the production public API
//! (`FlussDatafusion::new` -> `RealFlussSource`) against a live Fluss cluster.
//!
//! This is the primary integration suite: the real backend, the sync/async
//! metadata bridge, the catalog tree, and the custom execution plans all run
//! against actual Fluss. There is no cluster-free mirror — the catalog is fully
//! live (no cache), and the pushdown contract is verified here against the real
//! server. Live post-DDL visibility is covered by the `live_metadata` suite.
//!
//! Gated by `integration_tests` (needs a container runtime). Run with:
//!   cargo test -p fluss-datafusion --features integration_tests -- e2e
//!
//! The whole flow runs in a single test against one cluster: bringing up a Fluss
//! cluster is expensive, and the assertions are read-only and independent.

#![cfg(feature = "integration_tests")]

use std::sync::Arc;

use arrow::array::{Array, Int32Array, Int64Array, StringArray};
use arrow::datatypes::DataType;
use datafusion::execution::context::SessionContext;
use datafusion::prelude::SessionConfig;

use fluss_datafusion::{FlussDatafusion, RegisterCatalogOptions};

use crate::integration::setup;
use crate::integration::utils::helpers::{
    CATALOG, collect_i32, collect_strings, expect_query_error, options, render_explain, total_rows,
};
use crate::integration::utils::names;

/// Dedicated name/port for the real-cluster e2e suite so it can run without
/// colliding with any other cluster in the same `integration_tests` binary.
const CLUSTER_NAME: &str = "df-e2e";
const CLUSTER_PORT: u16 = 9133;

#[tokio::test]
async fn end_to_end_sql_through_real_backend() {
    let cluster = setup::start_cluster(CLUSTER_NAME, CLUSTER_PORT).await;
    let connection = Arc::new(cluster.get_fluss_connection().await);

    setup::create_kv_simple(&connection).await;
    setup::create_kv_composite(&connection).await;
    setup::create_log_basic(&connection).await;
    setup::create_log_multi_bucket(&connection).await;
    setup::create_log_partitioned(&connection).await;
    setup::create_kv_partitioned(&connection).await;

    // The production constructor over the real connection: the path under test.
    let fd = FlussDatafusion::new(connection.clone(), options())
        .await
        .expect("FlussDatafusion::new over real connection");
    // information_schema is opt-in; enable it so the SQL catalog-listing assertion
    // can query `information_schema.tables`.
    let ctx =
        SessionContext::new_with_config(SessionConfig::new().with_information_schema(true));
    fd.register_catalog(&ctx, CATALOG, RegisterCatalogOptions::default())
        .await
        .expect("register_catalog");

    catalog_lists_phase1_tables(&ctx);
    catalog_lists_tables_via_information_schema(&ctx).await;
    table_provider_exposes_arrow_schema(&ctx).await;
    catalog_unknown_database_and_table(&ctx);
    kv_single_pk_equality_returns_row(&ctx).await;
    kv_absent_key_returns_no_rows(&ctx).await;
    kv_composite_pk_equality_returns_row(&ctx).await;
    kv_non_primary_key_predicate_fails(&ctx).await;
    kv_partial_composite_key_fails(&ctx).await;
    kv_full_scan_without_filter_fails(&ctx).await;
    kv_in_list_predicate_fails(&ctx).await;
    log_head_first_limit_returns_rows(&ctx).await;
    log_multi_bucket_scan_returns_limited_rows(&ctx).await;
    log_projection_keeps_only_projected_column(&ctx).await;
    log_without_limit_returns_full_snapshot(&ctx).await;
    log_partitioned_pruned_scan_returns_only_matching_partition(&ctx).await;
    log_partitioned_scan_without_predicate_reads_all_partitions(&ctx).await;
    kv_partitioned_full_pk_lookup_returns_row(&ctx).await;
    explain_shows_custom_plans(&ctx).await;

    // Best-effort cleanup so reruns start clean; the cluster also stops on drop.
    setup::drop_phase1_tables(&connection).await;
    cluster.stop();
}

fn catalog_lists_phase1_tables(ctx: &SessionContext) {
    let schema = ctx
        .catalog(CATALOG)
        .expect("catalog registered")
        .schema(names::DATABASE)
        .expect("database schema present");
    let tables = schema.table_names();
    for expected in [names::KV_SIMPLE, names::KV_COMPOSITE, names::LOG_BASIC] {
        assert!(
            tables.iter().any(|t| t == expected),
            "expected table {expected} in {tables:?}"
        );
    }
}

/// Lists the Phase 1 tables via `information_schema.tables`, exercising the async
/// `table()` path through SQL rather than the direct catalog-tree walk above.
async fn catalog_lists_tables_via_information_schema(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_catalog = '{CATALOG}' AND table_schema = '{}' \
             ORDER BY table_name",
            names::DATABASE
        ))
        .await
        .expect("sql plan")
        .collect()
        .await
        .expect("collect");

    let mut found: Vec<String> = Vec::new();
    for b in &batches {
        let col = b
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("table_name string column");
        for i in 0..col.len() {
            found.push(col.value(i).to_string());
        }
    }
    for expected in [names::KV_SIMPLE, names::KV_COMPOSITE, names::LOG_BASIC] {
        assert!(
            found.iter().any(|t| t == expected),
            "information_schema missing {expected}: {found:?}"
        );
    }
}

/// Proves the `TableProvider` exposes the table's Arrow schema (column presence +
/// Fluss `int` -> Arrow `Int32` mapping).
async fn table_provider_exposes_arrow_schema(ctx: &SessionContext) {
    let schema = ctx
        .catalog(CATALOG)
        .expect("catalog")
        .schema(names::DATABASE)
        .expect("schema");
    let table = schema
        .table(names::KV_SIMPLE)
        .await
        .expect("table lookup ok")
        .expect("kv_simple present");

    let arrow_schema = table.schema();
    let field_names: Vec<&str> = arrow_schema
        .fields()
        .iter()
        .map(|f| f.name().as_str())
        .collect();
    assert!(
        field_names.contains(&"id"),
        "expected id column, got {field_names:?}"
    );
    let id_field = arrow_schema.field_with_name("id").expect("id field");
    assert_eq!(
        id_field.data_type(),
        &DataType::Int32,
        "id should map to Int32"
    );
}

/// Unknown database and unknown table are absent from the catalog tree, mirroring
/// the source-level `DatabaseNotFound` / `TableNotFound` contract at the catalog
/// surface a SQL user actually sees.
fn catalog_unknown_database_and_table(ctx: &SessionContext) {
    let catalog = ctx.catalog(CATALOG).expect("catalog registered");
    assert!(
        catalog.schema("does_not_exist").is_none(),
        "unknown database must not appear as a schema"
    );
    let schema = catalog
        .schema(names::DATABASE)
        .expect("database schema present");
    assert!(
        !schema.table_names().iter().any(|t| t == "no_such_table"),
        "unknown table must not appear in the table listing"
    );
}

async fn kv_single_pk_equality_returns_row(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT id, name, age FROM {CATALOG}.{}.{} WHERE id = 2",
            names::DATABASE,
            names::KV_SIMPLE
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(total_rows(&batches), 1, "id=2 should match exactly one row");
    let batch = &batches[0];
    let ids = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .expect("id int32");
    assert_eq!(ids.value(0), 2);
    let names_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("name string");
    assert_eq!(names_col.value(0), "Noco");
    let ages = batch
        .column(2)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("age int64");
    assert_eq!(ages.value(0), 25);
}

async fn kv_absent_key_returns_no_rows(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT * FROM {CATALOG}.{}.{} WHERE id = 999",
            names::DATABASE,
            names::KV_SIMPLE
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(
        total_rows(&batches),
        0,
        "absent key yields zero rows, no error"
    );
}

async fn kv_composite_pk_equality_returns_row(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT region, id, score FROM {CATALOG}.{}.{} WHERE region = 'us' AND id = 2",
            names::DATABASE,
            names::KV_COMPOSITE
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(total_rows(&batches), 1);
    let scores = batches[0]
        .column(2)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("score int64");
    assert_eq!(scores.value(0), 200);
}

async fn kv_non_primary_key_predicate_fails(ctx: &SessionContext) {
    let err = expect_query_error(
        ctx,
        &format!(
            "SELECT * FROM {CATALOG}.{}.{} WHERE name = 'x'",
            names::DATABASE,
            names::KV_SIMPLE
        ),
    )
    .await;
    assert!(
        err.contains("unsupported query pattern"),
        "expected unsupported-query error, got: {err}"
    );
}

async fn kv_partial_composite_key_fails(ctx: &SessionContext) {
    let err = expect_query_error(
        ctx,
        &format!(
            "SELECT * FROM {CATALOG}.{}.{} WHERE region = 'us'",
            names::DATABASE,
            names::KV_COMPOSITE
        ),
    )
    .await;
    assert!(
        err.contains("unsupported query pattern"),
        "expected unsupported-query error, got: {err}"
    );
}

async fn kv_full_scan_without_filter_fails(ctx: &SessionContext) {
    let err = expect_query_error(
        ctx,
        &format!(
            "SELECT * FROM {CATALOG}.{}.{}",
            names::DATABASE,
            names::KV_SIMPLE
        ),
    )
    .await;
    assert!(
        err.contains("unsupported query pattern"),
        "expected unsupported-query error, got: {err}"
    );
}

async fn kv_in_list_predicate_fails(ctx: &SessionContext) {
    let err = expect_query_error(
        ctx,
        &format!(
            "SELECT * FROM {CATALOG}.{}.{} WHERE id IN (1, 2)",
            names::DATABASE,
            names::KV_SIMPLE
        ),
    )
    .await;
    assert!(
        err.contains("unsupported query pattern"),
        "expected unsupported-query error, got: {err}"
    );
}

async fn log_head_first_limit_returns_rows(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT * FROM {CATALOG}.{}.{} LIMIT 2",
            names::DATABASE,
            names::LOG_BASIC
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(total_rows(&batches), 2, "limit 2 should yield two rows");
    assert_eq!(collect_i32(&batches, 0), vec![1, 2]);
}

/// A multi-bucket log table seeds 12 rows across 3 buckets. The source applies a
/// per-target first-N cap and DataFusion still caps the merged result at the
/// global `LIMIT`. `LIMIT 5` (smaller than the row count, not a multiple of the
/// bucket count) must yield EXACTLY 5 rows. Cross-bucket order is not guaranteed,
/// so only the COUNT is asserted.
async fn log_multi_bucket_scan_returns_limited_rows(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT * FROM {CATALOG}.{}.{} LIMIT 5",
            names::DATABASE,
            names::LOG_MULTI_BUCKET
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(
        total_rows(&batches),
        5,
        "LIMIT 5 across buckets must yield exactly five rows (global limit holds)"
    );

    // The custom multi-bucket scan still surfaces in EXPLAIN.
    let explained = ctx
        .sql(&format!(
            "EXPLAIN SELECT * FROM {CATALOG}.{}.{} LIMIT 5",
            names::DATABASE,
            names::LOG_MULTI_BUCKET
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");
    let rendered = render_explain(&explained);
    assert!(
        rendered.contains("FlussLogScanExec"),
        "EXPLAIN should show the multi-bucket log-scan plan, got:\n{rendered}"
    );
}

async fn log_projection_keeps_only_projected_column(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT id FROM {CATALOG}.{}.{} LIMIT 4",
            names::DATABASE,
            names::LOG_BASIC
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(total_rows(&batches), 4);
    assert_eq!(
        batches[0].num_columns(),
        1,
        "projection to id should keep exactly one column"
    );
    assert_eq!(batches[0].schema().field(0).name(), "id");
    assert_eq!(collect_i32(&batches, 0), vec![1, 2, 3, 4]);
}

async fn log_without_limit_returns_full_snapshot(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT * FROM {CATALOG}.{}.{}",
            names::DATABASE,
            names::LOG_BASIC
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(total_rows(&batches), 6, "full snapshot should return all rows");
    assert_eq!(collect_i32(&batches, 0), vec![1, 2, 3, 4, 5, 6]);
}

/// A partition-column equality prunes the finite snapshot scan to the matching
/// partition: `region = 'US'` returns only the two US rows. The residual
/// `FilterExec` (the `Inexact` pushdown) keeps correctness even if pruning were
/// imperfect.
async fn log_partitioned_pruned_scan_returns_only_matching_partition(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT id, name, region FROM {CATALOG}.{}.{} WHERE region = 'US' LIMIT 10",
            names::DATABASE,
            names::LOG_PARTITIONED
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(
        total_rows(&batches),
        2,
        "region = 'US' should return exactly the two US rows"
    );
    for region in collect_strings(&batches, 2) {
        assert_eq!(region, "US", "pruned scan must only return US rows");
    }
}

/// With no partition predicate, the scan reads ALL partitions (pruning is optional,
/// never required): all four rows across US and EU are returned.
async fn log_partitioned_scan_without_predicate_reads_all_partitions(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT id FROM {CATALOG}.{}.{} LIMIT 10",
            names::DATABASE,
            names::LOG_PARTITIONED
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(
        total_rows(&batches),
        4,
        "no partition predicate must scan all partitions (four rows)"
    );
}

/// A partitioned KV table resolves the partition from the full primary key via the
/// Fluss `Lookuper`: the existing full-PK lookup path needs no partition-specific
/// code. `region = 'US' AND id = 1` returns the single matching row.
async fn kv_partitioned_full_pk_lookup_returns_row(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT region, id, score FROM {CATALOG}.{}.{} WHERE region = 'US' AND id = 1",
            names::DATABASE,
            names::KV_PARTITIONED
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(total_rows(&batches), 1, "full PK lookup should match one row");
    let scores = batches[0]
        .column(2)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("score int64");
    assert_eq!(scores.value(0), 100);
}

async fn explain_shows_custom_plans(ctx: &SessionContext) {
    let kv = ctx
        .sql(&format!(
            "EXPLAIN SELECT * FROM {CATALOG}.{}.{} WHERE id = 2",
            names::DATABASE,
            names::KV_SIMPLE
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");
    let kv_rendered = render_explain(&kv);
    assert!(
        kv_rendered.contains("FlussKvLookupExec"),
        "EXPLAIN should show the custom lookup plan, got:\n{kv_rendered}"
    );

    let log = ctx
        .sql(&format!(
            "EXPLAIN SELECT * FROM {CATALOG}.{}.{} LIMIT 4",
            names::DATABASE,
            names::LOG_BASIC
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");
    let log_rendered = render_explain(&log);
    assert!(
        log_rendered.contains("FlussLogScanExec"),
        "EXPLAIN should show the custom log-scan plan, got:\n{log_rendered}"
    );
}
