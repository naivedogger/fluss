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

//! Real-cluster proof that a primary-keyed (KV) table supports BOTH read shapes:
//! a bounded `SELECT ... LIMIT k` scan and a full-primary-key point lookup.
//!
//! `SELECT * FROM kv LIMIT k` (with `k` < the seeded row count) must return
//! exactly `k` rows through the bounded-scan path, while `WHERE id = v` must still
//! resolve the single matching row through the point-lookup path.
//!
//! Gated by `integration_tests` (needs a container runtime). Run with:
//!   cargo test -p fluss-datafusion --features integration_tests -- kv_bounded

#![cfg(feature = "integration_tests")]

use std::sync::Arc;

use arrow::array::{Array, Int32Array};
use datafusion::execution::context::SessionContext;

use fluss_datafusion::{FlussDatafusion, RegisterCatalogOptions};

use crate::integration::setup;
use crate::integration::utils::helpers::{options, render_explain, total_rows, CATALOG};
use crate::integration::utils::names;

/// Dedicated name/port so this suite can run without colliding with any other
/// cluster in the same `integration_tests` binary.
const CLUSTER_NAME: &str = "df-kv-bounded";
const CLUSTER_PORT: u16 = 9134;

/// `LIMIT` used by the bounded scan. The KV table is seeded with six rows
/// (ids 1..=6), so this `k` is below the row count and proves the scan caps the
/// result at exactly `k`.
const LIMIT_K: usize = 4;

#[tokio::test]
async fn kv_bounded_scan_and_point_lookup_through_real_backend() {
    let cluster = setup::start_cluster(CLUSTER_NAME, CLUSTER_PORT).await;
    let connection = Arc::new(cluster.get_fluss_connection().await);

    setup::create_kv_bounded(&connection).await;

    let fd = FlussDatafusion::new(connection.clone(), options())
        .await
        .expect("FlussDatafusion::new over real connection");
    let ctx = SessionContext::new();
    fd.register_catalog(&ctx, CATALOG, RegisterCatalogOptions::default())
        .await
        .expect("register_catalog");

    kv_bounded_limit_returns_exactly_k_rows(&ctx).await;
    kv_bounded_scan_shows_in_explain(&ctx).await;
    kv_full_pk_lookup_still_returns_single_row(&ctx).await;

    setup::drop_table_named(&connection, names::KV_BOUNDED).await;
    cluster.stop();
}

/// `SELECT * FROM kv LIMIT k` with `k` < seeded rows returns EXACTLY `k` rows via
/// the bounded-scan path.
async fn kv_bounded_limit_returns_exactly_k_rows(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT * FROM {CATALOG}.{}.{} LIMIT {LIMIT_K}",
            names::DATABASE,
            names::KV_BOUNDED
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(
        total_rows(&batches),
        LIMIT_K,
        "a bounded KV scan must return exactly LIMIT rows"
    );
}

/// The bounded KV scan surfaces its custom plan in `EXPLAIN` under the KV-specific
/// label, distinct from the log table's `FlussLogScanExec`.
async fn kv_bounded_scan_shows_in_explain(ctx: &SessionContext) {
    let explained = ctx
        .sql(&format!(
            "EXPLAIN SELECT * FROM {CATALOG}.{}.{} LIMIT {LIMIT_K}",
            names::DATABASE,
            names::KV_BOUNDED
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");
    let rendered = render_explain(&explained);
    assert!(
        rendered.contains("FlussKvScanExec"),
        "EXPLAIN should show the KV bounded-scan plan, got:\n{rendered}"
    );
}

/// A full-primary-key equality still resolves the single matching row via the
/// point-lookup path (the bounded-scan fallback must not displace it).
async fn kv_full_pk_lookup_still_returns_single_row(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT id, name FROM {CATALOG}.{}.{} WHERE id = 3",
            names::DATABASE,
            names::KV_BOUNDED
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(total_rows(&batches), 1, "id = 3 must match exactly one row");
    let ids = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .expect("id int32");
    assert_eq!(ids.value(0), 3);
}
