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

//! Real-cluster proof that a primary-keyed (KV) table whose bucket key is a
//! strict prefix of its PK supports bucket-key prefix lookup, while a full-PK
//! equality still resolves through the point-lookup path.
//!
//! Gated by `integration_tests` (needs a container runtime). Run with:
//!   cargo test -p fluss-datafusion --features integration_tests -- kv_prefix

#![cfg(feature = "integration_tests")]

use std::sync::Arc;

use datafusion::execution::context::SessionContext;

use fluss_datafusion::{FlussDatafusion, RegisterCatalogOptions};

use crate::integration::setup;
use crate::integration::utils::helpers::{collect_i32, collect_strings, options, render_explain, total_rows, CATALOG};
use crate::integration::utils::names;

/// Dedicated name/port so this suite can run without colliding with any other
/// cluster in the same `integration_tests` binary. Uses ports 9137 and 9138.
const CLUSTER_NAME: &str = "df-kv-prefix";
const CLUSTER_PORT: u16 = 9137;

#[tokio::test]
async fn kv_prefix_lookup_and_point_lookup_through_real_backend() {
    let cluster = setup::start_cluster(CLUSTER_NAME, CLUSTER_PORT).await;
    let connection = Arc::new(cluster.get_fluss_connection().await);

    setup::create_kv_prefix(&connection).await;

    let fd = FlussDatafusion::new(connection.clone(), options())
        .await
        .expect("FlussDatafusion::new over real connection");
    let ctx = SessionContext::new();
    fd.register_catalog(&ctx, CATALOG, RegisterCatalogOptions::default())
        .await
        .expect("register_catalog");

    kv_bucket_key_prefix_returns_all_matching_rows(&ctx).await;
    kv_prefix_lookup_shows_in_explain(&ctx).await;
    kv_full_pk_lookup_still_uses_point_lookup(&ctx).await;

    setup::drop_table_named(&connection, names::KV_PREFIX).await;
    cluster.stop();
}

async fn kv_bucket_key_prefix_returns_all_matching_rows(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!(
            "SELECT c1, c2, name FROM {CATALOG}.{}.{} WHERE c1 = 10",
            names::DATABASE,
            names::KV_PREFIX
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");

    assert_eq!(
        total_rows(&batches),
        3,
        "bucket-key prefix lookup on c1 = 10 must return all matching rows"
    );
    assert_eq!(collect_i32(&batches, 0), vec![10, 10, 10]);
    assert_eq!(collect_i32(&batches, 1), vec![1, 2, 3]);
    assert_eq!(collect_strings(&batches, 2), vec!["a", "b", "c"]);
}

async fn kv_prefix_lookup_shows_in_explain(ctx: &SessionContext) {
    let explained = ctx
        .sql(&format!(
            "EXPLAIN SELECT * FROM {CATALOG}.{}.{} WHERE c1 = 10",
            names::DATABASE,
            names::KV_PREFIX
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");
    let rendered = render_explain(&explained);
    assert!(
        rendered.contains("FlussKvPrefixLookupExec"),
        "EXPLAIN should show the KV prefix-lookup plan, got:\n{rendered}"
    );
}

async fn kv_full_pk_lookup_still_uses_point_lookup(ctx: &SessionContext) {
    let explained = ctx
        .sql(&format!(
            "EXPLAIN SELECT * FROM {CATALOG}.{}.{} WHERE c1 = 10 AND c2 = 2",
            names::DATABASE,
            names::KV_PREFIX
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");
    let rendered = render_explain(&explained);
    assert!(
        rendered.contains("FlussKvLookupExec"),
        "EXPLAIN should keep the full PK equality on point lookup, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("FlussKvPrefixLookupExec"),
        "full PK equality must not route to the prefix-lookup plan, got:\n{rendered}"
    );

    let batches = ctx
        .sql(&format!(
            "SELECT c1, c2, name FROM {CATALOG}.{}.{} WHERE c1 = 10 AND c2 = 2",
            names::DATABASE,
            names::KV_PREFIX
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");
    assert_eq!(total_rows(&batches), 1, "full PK equality must match one row");
    assert_eq!(collect_i32(&batches, 0), vec![10]);
    assert_eq!(collect_i32(&batches, 1), vec![2]);
    assert_eq!(collect_strings(&batches, 2), vec!["b"]);
}
