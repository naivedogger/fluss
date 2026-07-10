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

//! Shared helpers for fluss-datafusion integration tests.
//!
//! There is a single integration backend: a real Fluss cluster, gated by
//! `integration_tests` (needs a container runtime). These helpers cover the
//! shared table identifiers and SQL-path utilities used by that suite.

#![allow(dead_code)]

/// Known table identifiers shared by the real-cluster setup and assertions, so
/// DDL and queries stay in sync instead of being stringly-typed.
pub mod names {
    pub const DATABASE: &str = "fluss";
    /// KV table with a single-column primary key.
    pub const KV_SIMPLE: &str = "df_kv_simple";
    /// KV table with a composite primary key.
    pub const KV_COMPOSITE: &str = "df_kv_composite";
    /// Log (append-only) table for bounded scans.
    pub const LOG_BASIC: &str = "df_log_basic";
    /// Multi-bucket log table proving per-bucket parallel scan + global LIMIT.
    pub const LOG_MULTI_BUCKET: &str = "df_log_multi_bucket";
    /// Log table created AFTER `register_catalog` to prove live visibility.
    pub const LOG_LIVE: &str = "df_log_live";
    /// Log table created up front, then dropped, to prove drops are reflected live.
    pub const LOG_TRANSIENT: &str = "df_log_transient";
    /// Partitioned log table proving partition pruning of bounded scans.
    pub const LOG_PARTITIONED: &str = "df_log_partitioned";
    /// Partitioned KV table proving full-PK lookup resolves the partition.
    pub const KV_PARTITIONED: &str = "df_kv_partitioned";
    /// KV table seeded with several rows to prove a bounded `LIMIT` scan.
    pub const KV_BOUNDED: &str = "df_kv_bounded";
    /// KV table whose bucket key is a strict prefix of the PK, proving prefix lookup.
    pub const KV_PREFIX: &str = "df_kv_prefix";
    /// KV table seeded with inserts + an update + a delete, proving a full-table
    /// scan (no filter / no LIMIT) merges the changelog to current state.
    pub const KV_FULL_SCAN: &str = "df_kv_full_scan";
    /// Wide KV table carrying one value column per scalar Fluss type, proving the
    /// KV point-lookup and KV full-scan decode paths handle every scalar type.
    pub const KV_WIDE_TYPES: &str = "df_kv_wide_types";
    /// Wide log table carrying one value column per scalar Fluss type, proving the
    /// log-scan decode path handles every scalar type.
    pub const LOG_WIDE_TYPES: &str = "df_log_wide_types";
    /// Log table carrying nested (array/map/row) value columns, proving the
    /// log-scan path decodes List/Map/Struct Arrow types.
    pub const LOG_NESTED_TYPES: &str = "df_log_nested_types";
}

/// Shared SQL-path helpers for the real-cluster integration suite.
#[cfg(feature = "integration_tests")]
pub mod helpers {
    use arrow::array::{Array, Int32Array, RecordBatch, StringArray};
    use datafusion::execution::context::SessionContext;

    use fluss_datafusion::FlussDatafusionOptions;

    /// Catalog name every SQL test registers the Fluss catalog under.
    pub const CATALOG: &str = "fluss";

    /// Default integration options for the tests.
    pub fn options() -> FlussDatafusionOptions {
        FlussDatafusionOptions::default()
    }

    /// Total row count across all batches.
    pub fn total_rows(batches: &[RecordBatch]) -> usize {
        batches.iter().map(|b| b.num_rows()).sum()
    }

    /// Flattens the `i32` values of column `col` across all batches, in order.
    pub fn collect_i32(batches: &[RecordBatch], col: usize) -> Vec<i32> {
        batches
            .iter()
            .flat_map(|b| {
                b.column(col)
                    .as_any()
                    .downcast_ref::<Int32Array>()
                    .expect("expected int32 column")
                    .values()
                    .to_vec()
            })
            .collect()
    }

    /// Flattens the string values of column `col` across all batches, in order.
    pub fn collect_strings(batches: &[RecordBatch], col: usize) -> Vec<String> {
        batches
            .iter()
            .flat_map(|b| {
                let arr = b
                    .column(col)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("expected string column");
                (0..arr.len()).map(|i| arr.value(i).to_string())
            })
            .collect()
    }

    /// Concatenates every string-column cell across all batches, one per line.
    ///
    /// `EXPLAIN` renders its plan into string columns; flattening them this way
    /// gives a single haystack a test can assert a custom plan name appears in.
    pub fn render_explain(batches: &[RecordBatch]) -> String {
        let mut rendered = String::new();
        for b in batches {
            for col in b.columns() {
                if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                    for i in 0..arr.len() {
                        if arr.is_valid(i) {
                            rendered.push_str(arr.value(i));
                            rendered.push('\n');
                        }
                    }
                }
            }
        }
        rendered
    }

    /// Runs a query expected to be rejected and returns the rendered error.
    ///
    /// Failure may surface at planning (`sql`) or at execution (`collect`); accept
    /// either so the conservative-failure contract is fully covered.
    pub async fn expect_query_error(ctx: &SessionContext, sql: &str) -> String {
        match ctx.sql(sql).await {
            Err(e) => e.to_string(),
            Ok(df) => match df.collect().await {
                Err(e) => e.to_string(),
                Ok(_) => panic!("expected query to fail but it succeeded: {sql}"),
            },
        }
    }
}
