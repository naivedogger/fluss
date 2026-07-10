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

//! Proves the catalog is fully live: DDL is visible in the SAME `SessionContext`
//! immediately, with no re-`register_catalog`.
//!
//! This is the regression guard for the A4 decision (zero snapshot, no cache):
//!   1. a table created AFTER `register_catalog` is queryable in the same session,
//!   2. it shows up in the live table listing, and
//!   3. a dropped table disappears from that same session's listing/visibility.
//!
//! Gated by `integration_tests` (needs a container runtime). Run with:
//!   cargo test -p fluss-datafusion --features integration_tests -- live_metadata
//!
//! Uses its own cluster name + port so it can run alongside the e2e suite in the
//! same `integration_tests` binary without colliding.
//!
//! Module-level gating lives in `mod.rs` (`#[cfg(feature = "integration_tests")]`).

#![cfg(feature = "integration_tests")]

use std::sync::Arc;

use arrow::array::{Array, StringArray};
use datafusion::execution::context::SessionContext;
use datafusion::prelude::SessionConfig;

use fluss_datafusion::{FlussDatafusion, RegisterCatalogOptions};

use crate::integration::setup;
use crate::integration::utils::helpers::{CATALOG, options, total_rows};
use crate::integration::utils::names;

/// Dedicated name/port so this suite does not collide with the e2e cluster.
/// (e2e=9133, kv_bounded=9134, kv_prefix_lookup=9137, kv_full_scan=9145,
///  type_coverage=9148.)
const CLUSTER_NAME: &str = "df-live-meta";
const CLUSTER_PORT: u16 = 9139;

#[tokio::test]
async fn live_metadata_reflects_post_registration_ddl() {
    let cluster = setup::start_cluster(CLUSTER_NAME, CLUSTER_PORT).await;
    let connection = Arc::new(cluster.get_fluss_connection().await);

    // A table that exists at registration time and is dropped later, to prove
    // drops are reflected live in the same session.
    setup::create_log_named(&connection, names::LOG_TRANSIENT).await;

    let fd = FlussDatafusion::new(connection.clone(), options())
        .await
        .expect("FlussDatafusion::new over real connection");
    // information_schema is opt-in; enable it so `SHOW TABLES` works.
    let ctx =
        SessionContext::new_with_config(SessionConfig::new().with_information_schema(true));
    fd.register_catalog(&ctx, CATALOG, RegisterCatalogOptions::default())
        .await
        .expect("register_catalog");

    // The transient table is visible right after registration.
    assert!(
        list_tables_via_schema(&ctx).contains(&names::LOG_TRANSIENT.to_string()),
        "table present at registration must be listed"
    );

    post_registration_table_is_queryable(&ctx, &connection).await;
    dropped_table_disappears_from_same_session(&ctx, &connection).await;

    // Best-effort cleanup so reruns start clean; the cluster also stops on drop.
    setup::drop_table_named(&connection, names::LOG_LIVE).await;
    setup::drop_table_named(&connection, names::LOG_TRANSIENT).await;
    cluster.stop();
}

/// (1)+(2): a table created AFTER `register_catalog` is both queryable and listed
/// in the SAME session, with no re-registration.
async fn post_registration_table_is_queryable(
    ctx: &SessionContext,
    connection: &fluss::client::FlussConnection,
) {
    // Created strictly after register_catalog above.
    setup::create_log_named(connection, names::LOG_LIVE).await;

    // (2) It appears in the live listing (catalog tree + SHOW TABLES) without
    // re-registering.
    assert!(
        list_tables_via_schema(ctx).contains(&names::LOG_LIVE.to_string()),
        "post-registration table must appear in the live catalog listing"
    );
    assert!(
        list_tables_via_show_tables(ctx).await.contains(&names::LOG_LIVE.to_string()),
        "post-registration table must appear in SHOW TABLES"
    );

    // (1) It is queryable in the same session (the seeded log_batch has 6 rows).
    let batches = ctx
        .sql(&format!(
            "SELECT id FROM {CATALOG}.{}.{} LIMIT 4",
            names::DATABASE,
            names::LOG_LIVE
        ))
        .await
        .expect("plan")
        .collect()
        .await
        .expect("collect");
    assert_eq!(
        total_rows(&batches),
        4,
        "post-registration table must be queryable in the same session"
    );
}

/// (3): after dropping a table, the same session no longer lists or sees it.
async fn dropped_table_disappears_from_same_session(
    ctx: &SessionContext,
    connection: &fluss::client::FlussConnection,
) {
    setup::drop_table_named(connection, names::LOG_TRANSIENT).await;

    assert!(
        !list_tables_via_schema(ctx).contains(&names::LOG_TRANSIENT.to_string()),
        "dropped table must vanish from the live catalog listing"
    );
    assert!(
        !list_tables_via_show_tables(ctx).await.contains(&names::LOG_TRANSIENT.to_string()),
        "dropped table must vanish from SHOW TABLES"
    );

    // The schema provider's live `table()` returns None for the dropped table.
    let schema = ctx
        .catalog(CATALOG)
        .expect("catalog registered")
        .schema(names::DATABASE)
        .expect("database schema present");
    assert!(
        schema
            .table(names::LOG_TRANSIENT)
            .await
            .expect("table lookup ok")
            .is_none(),
        "dropped table must resolve to None on live lookup"
    );
}

/// Lists tables via the catalog tree's synchronous `table_names()` (the live
/// `SchemaProvider` path).
fn list_tables_via_schema(ctx: &SessionContext) -> Vec<String> {
    ctx.catalog(CATALOG)
        .expect("catalog registered")
        .schema(names::DATABASE)
        .expect("database schema present")
        .table_names()
}

/// Lists tables via `SHOW TABLES`, exercising the SQL surface a user sees.
async fn list_tables_via_show_tables(ctx: &SessionContext) -> Vec<String> {
    let batches = ctx
        .sql("SHOW TABLES")
        .await
        .expect("SHOW TABLES plan")
        .collect()
        .await
        .expect("collect");

    let mut found = Vec::new();
    for b in &batches {
        // information_schema's SHOW TABLES columns: catalog, schema, table, type.
        // Scan every string column so we do not couple to a fixed column index.
        for col in b.columns() {
            if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                for i in 0..arr.len() {
                    if arr.is_valid(i) {
                        found.push(arr.value(i).to_string());
                    }
                }
            }
        }
    }
    found
}
