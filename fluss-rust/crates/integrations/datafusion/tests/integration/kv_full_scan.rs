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

//! FINAL acceptance: real-cluster proof that a primary-keyed (KV) table supports
//! a full-table scan with NO filter and NO `LIMIT`.
//!
//! `SELECT * FROM kv` must return the table's correct CURRENT state — merging the
//! CDC changelog so a deleted PK is absent, updated PKs carry their new values,
//! and the row count is correct. `EXPLAIN` must show the dedicated
//! `FlussKvFullScanExec` plan, and a column-subset projection must work.
//!
//! Gated by `integration_tests` (needs a container runtime). Run with:
//!   cargo test -p fluss-datafusion --features integration_tests -- kv_full_scan

#![cfg(feature = "integration_tests")]

use std::sync::Arc;

use arrow::array::{Int32Array, StringArray};
use datafusion::execution::context::SessionContext;

use fluss_datafusion::{FlussDatafusion, RegisterCatalogOptions};

use crate::integration::setup;
use crate::integration::utils::helpers::{options, render_explain, total_rows, CATALOG};
use crate::integration::utils::names;

/// Dedicated name/port so this suite never collides with the other clusters in
/// the same `integration_tests` binary (e2e=9133, kv_bounded=9134,
/// kv_prefix_lookup=9137, live_metadata=9139, type_coverage=9148).
const CLUSTER_NAME: &str = "df-kv-full-scan";
const CLUSTER_PORT: u16 = 9145;

#[tokio::test]
async fn kv_full_scan_returns_current_state_through_real_backend() {
    let cluster = setup::start_cluster(CLUSTER_NAME, CLUSTER_PORT).await;
    let connection = Arc::new(cluster.get_fluss_connection().await);

    setup::create_kv_full_scan(&connection).await;

    let fd = FlussDatafusion::new(connection.clone(), options())
        .await
        .expect("FlussDatafusion::new over real connection");
    let ctx = SessionContext::new();
    fd.register_catalog(&ctx, CATALOG, RegisterCatalogOptions::default())
        .await
        .expect("register_catalog");

    full_scan_returns_merged_current_state(&ctx).await;
    full_scan_shows_in_explain(&ctx).await;
    full_scan_projection_returns_subset_columns(&ctx).await;

    setup::drop_table_named(&connection, names::KV_FULL_SCAN).await;
    cluster.stop();
}

fn table() -> String {
    format!("{CATALOG}.{}.{}", names::DATABASE, names::KV_FULL_SCAN)
}

/// `SELECT * FROM kv` (no filter, no LIMIT) returns the merged CURRENT state:
/// id=3 deleted, id=2 and id=4 carry their updated names, count is correct.
async fn full_scan_returns_merged_current_state(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!("SELECT * FROM {}", table()))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(
        total_rows(&batches),
        4,
        "full scan must return the 4 surviving rows (5 inserted, 1 deleted)"
    );

    let mut rows: Vec<(i32, String)> = Vec::new();
    for b in &batches {
        let ids = b
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("id int32");
        let names = b
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("name utf8");
        for i in 0..b.num_rows() {
            rows.push((ids.value(i), names.value(i).to_string()));
        }
    }
    rows.sort_by_key(|(id, _)| *id);

    let expected = vec![
        (1, "alpha".to_string()),
        (2, "bravo-v2".to_string()),
        (4, "delta-v2".to_string()),
        (5, "echo".to_string()),
    ];
    assert_eq!(
        rows, expected,
        "full scan must reflect updates (2,4) and the delete (3)"
    );
}

/// The full-table KV scan surfaces its dedicated custom plan in `EXPLAIN`,
/// distinct from the log table's `FlussLogScanExec` and the KV bounded
/// `FlussKvScanExec`.
async fn full_scan_shows_in_explain(ctx: &SessionContext) {
    let explained = ctx
        .sql(&format!("EXPLAIN SELECT * FROM {}", table()))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");
    let rendered = render_explain(&explained);
    assert!(
        rendered.contains("FlussKvFullScanExec"),
        "EXPLAIN should show the KV full-scan plan, got:\n{rendered}"
    );
}

/// `SELECT name FROM kv` (projection, still no filter / no LIMIT) returns just the
/// projected column for every surviving row.
async fn full_scan_projection_returns_subset_columns(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!("SELECT name FROM {}", table()))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(total_rows(&batches), 4, "projection keeps the 4 surviving rows");
    for b in &batches {
        assert_eq!(b.num_columns(), 1, "projection must return a single column");
        assert!(
            b.column(0).as_any().downcast_ref::<StringArray>().is_some(),
            "projected column must be the name (Utf8) column"
        );
    }
}
