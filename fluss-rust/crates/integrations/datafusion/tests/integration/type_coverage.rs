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

//! Real-cluster COLUMN-TYPE coverage for the DataFusion SQL read path.
//!
//! Every other integration table uses only `int`/`bigint`/`string`, so the SQL
//! read path was untested for the rest of Fluss's column types. This module
//! drives the FULL scalar type matrix (plus nested array/map/row) through the
//! three decode paths that decode differently:
//!
//!   1. KV point lookup  — `WHERE id = <k>` on a KV table.
//!   2. KV full scan      — `SELECT *` (no filter / no LIMIT) on a KV table.
//!   3. Log scan          — `SELECT ... LIMIT n` on a log table.
//!
//! For every column the assertions check BOTH the Arrow column `DataType` AND
//! the decoded value (downcasting each array and checking the logical value).
//!
//! Gated by `integration_tests` (needs a container runtime). Run with:
//!   cargo test -p fluss-datafusion --features integration_tests -- type_coverage

#![cfg(feature = "integration_tests")]

use std::sync::Arc;

use arrow::array::{
    Array, BinaryArray, BooleanArray, Date32Array, Decimal128Array, FixedSizeBinaryArray,
    Float32Array, Float64Array, Int16Array, Int32Array, Int64Array, Int8Array, ListArray, MapArray,
    RecordBatch, StringArray, StructArray, Time32MillisecondArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, TimeUnit};
use datafusion::execution::context::SessionContext;

use fluss_datafusion::{FlussDatafusion, RegisterCatalogOptions};

use crate::integration::setup;
use crate::integration::utils::helpers::{options, total_rows, CATALOG};
use crate::integration::utils::names;

/// Dedicated name/port so this suite never collides with the other clusters in
/// the same `integration_tests` binary (existing ports: 9133-9139, 9145).
const CLUSTER_NAME: &str = "df-type-coverage";
const CLUSTER_PORT: u16 = 9148;

#[tokio::test]
async fn type_coverage_through_real_backend() {
    let cluster = setup::start_cluster(CLUSTER_NAME, CLUSTER_PORT).await;
    let connection = Arc::new(cluster.get_fluss_connection().await);

    setup::create_kv_wide_types(&connection).await;
    setup::create_log_wide_types(&connection).await;
    setup::create_log_nested_types(&connection).await;

    let fd = FlussDatafusion::new(connection.clone(), options())
        .await
        .expect("FlussDatafusion::new over real connection");
    let ctx = SessionContext::new();
    fd.register_catalog(&ctx, CATALOG, RegisterCatalogOptions::default())
        .await
        .expect("register_catalog");

    // Path 1: KV point lookup.
    kv_point_lookup_decodes_every_scalar_type(&ctx).await;
    // Path 2: KV full scan.
    kv_full_scan_decodes_every_scalar_type(&ctx).await;
    // Path 3: log scan.
    log_scan_decodes_every_scalar_type(&ctx).await;
    // Nested types over the log-scan path.
    log_scan_decodes_nested_types(&ctx).await;

    setup::drop_table_named(&connection, names::KV_WIDE_TYPES).await;
    setup::drop_table_named(&connection, names::LOG_WIDE_TYPES).await;
    setup::drop_table_named(&connection, names::LOG_NESTED_TYPES).await;
    cluster.stop();
}

fn kv_wide() -> String {
    format!("{CATALOG}.{}.{}", names::DATABASE, names::KV_WIDE_TYPES)
}
fn log_wide() -> String {
    format!("{CATALOG}.{}.{}", names::DATABASE, names::LOG_WIDE_TYPES)
}
fn log_nested() -> String {
    format!("{CATALOG}.{}.{}", names::DATABASE, names::LOG_NESTED_TYPES)
}

/// Index a batch's columns by field name so assertions are order-independent.
fn col<'a>(batch: &'a RecordBatch, name: &str) -> &'a Arc<dyn Array> {
    let idx = batch
        .schema()
        .index_of(name)
        .unwrap_or_else(|_| panic!("missing column {name}"));
    batch.column(idx)
}

fn assert_dt(batch: &RecordBatch, name: &str, expected: &DataType) {
    let idx = batch.schema().index_of(name).unwrap();
    let actual = batch.schema().field(idx).data_type().clone();
    assert_eq!(
        &actual, expected,
        "column {name} Arrow type mismatch: got {actual:?}, want {expected:?}"
    );
}

/// Asserts the full per-type Arrow `DataType` AND the decoded value for the
/// row whose `id` is `id`, against the values seeded by `wide_value_row(id)`.
/// `batch` must be a single row containing exactly that row.
fn assert_wide_value_row(batch: &RecordBatch, id: i32) {
    assert_eq!(batch.num_rows(), 1, "expected exactly one row for id={id}");
    let n = id as i64;
    let r = 0; // single-row batch

    // --- Arrow DataType per column (mirrors fluss::record::to_arrow_schema) ---
    assert_dt(batch, "c_boolean", &DataType::Boolean);
    assert_dt(batch, "c_tinyint", &DataType::Int8);
    assert_dt(batch, "c_smallint", &DataType::Int16);
    assert_dt(batch, "c_int", &DataType::Int32);
    assert_dt(batch, "c_bigint", &DataType::Int64);
    assert_dt(batch, "c_float", &DataType::Float32);
    assert_dt(batch, "c_double", &DataType::Float64);
    assert_dt(batch, "c_decimal", &DataType::Decimal128(10, 2));
    assert_dt(batch, "c_char", &DataType::Utf8);
    assert_dt(batch, "c_string", &DataType::Utf8);
    assert_dt(batch, "c_bytes", &DataType::Binary);
    assert_dt(batch, "c_binary", &DataType::FixedSizeBinary(4));
    assert_dt(batch, "c_date", &DataType::Date32);
    assert_dt(batch, "c_time", &DataType::Time32(TimeUnit::Millisecond));
    assert_dt(
        batch,
        "c_timestamp",
        &DataType::Timestamp(TimeUnit::Microsecond, None),
    );
    assert_dt(
        batch,
        "c_ts_ltz",
        &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
    );

    // --- Decoded values (must equal what wide_value_row(id) seeded) ---
    let down = |name: &str| col(batch, name);

    assert_eq!(
        down("c_boolean")
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap()
            .value(r),
        id % 2 == 0,
    );
    assert_eq!(
        down("c_tinyint")
            .as_any()
            .downcast_ref::<Int8Array>()
            .unwrap()
            .value(r),
        (id + 1) as i8,
    );
    assert_eq!(
        down("c_smallint")
            .as_any()
            .downcast_ref::<Int16Array>()
            .unwrap()
            .value(r),
        (id + 2) as i16,
    );
    assert_eq!(
        down("c_int")
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .value(r),
        id + 3,
    );
    assert_eq!(
        down("c_bigint")
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .value(r),
        n + 4,
    );
    assert!(
        (down("c_float")
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap()
            .value(r)
            - (id as f32 + 0.5_f32))
            .abs()
            < f32::EPSILON,
    );
    assert!(
        (down("c_double")
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap()
            .value(r)
            - (id as f64 + 0.25_f64))
            .abs()
            < f64::EPSILON,
    );
    // Decimal(10,2): raw i128 = unscaled value = id*100 + 45.
    assert_eq!(
        down("c_decimal")
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap()
            .value(r),
        ((n * 100) + 45) as i128,
    );
    assert_eq!(
        down("c_char")
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(r),
        format!("c{id}"),
    );
    assert_eq!(
        down("c_string")
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(r),
        format!("s{id}"),
    );
    assert_eq!(
        down("c_bytes")
            .as_any()
            .downcast_ref::<BinaryArray>()
            .unwrap()
            .value(r),
        &[id as u8, (id + 1) as u8],
    );
    assert_eq!(
        down("c_binary")
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap()
            .value(r),
        &[id as u8; 4],
    );
    // Date32: epoch-day value passes through unchanged.
    assert_eq!(
        down("c_date")
            .as_any()
            .downcast_ref::<Date32Array>()
            .unwrap()
            .value(r),
        20000 + id,
    );
    // Time32(ms): ms-since-midnight passes through unchanged.
    assert_eq!(
        down("c_time")
            .as_any()
            .downcast_ref::<Time32MillisecondArray>()
            .unwrap()
            .value(r),
        id * 1000,
    );
    // Timestamp(us): millis -> micros (no sub-ms nanos), so micros = millis*1000.
    assert_eq!(
        down("c_timestamp")
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap()
            .value(r),
        (1_700_000_000_000 + n) * 1000,
    );
    // Timestamp_ltz(us): same instant, carried with UTC zone.
    assert_eq!(
        down("c_ts_ltz")
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap()
            .value(r),
        (1_700_000_000_000 + n) * 1000,
    );
}

/// Asserts every value column is NULL for the single-row null batch (id=3).
fn assert_wide_null_row(batch: &RecordBatch) {
    assert_eq!(batch.num_rows(), 1);
    for name in [
        "c_boolean",
        "c_tinyint",
        "c_smallint",
        "c_int",
        "c_bigint",
        "c_float",
        "c_double",
        "c_decimal",
        "c_char",
        "c_string",
        "c_bytes",
        "c_binary",
        "c_date",
        "c_time",
        "c_timestamp",
        "c_ts_ltz",
    ] {
        assert!(
            col(batch, name).is_null(0),
            "column {name} should be NULL for the all-null row"
        );
    }
}

/// PATH 1: KV point lookup (`WHERE id = <k>`) decodes every scalar type.
async fn kv_point_lookup_decodes_every_scalar_type(ctx: &SessionContext) {
    // Fully-populated rows: id=1 and id=2.
    for id in [1, 2] {
        let batches = ctx
            .sql(&format!("SELECT * FROM {} WHERE id = {id}", kv_wide()))
            .await
            .expect("plan point lookup")
            .collect()
            .await
            .expect("collect point lookup");
        assert_eq!(total_rows(&batches), 1, "point lookup id={id} -> one row");
        assert_wide_value_row(&batches[0], id);
    }

    // All-null value row: id=3.
    let batches = ctx
        .sql(&format!("SELECT * FROM {} WHERE id = 3", kv_wide()))
        .await
        .expect("plan point lookup null")
        .collect()
        .await
        .expect("collect point lookup null");
    assert_eq!(total_rows(&batches), 1);
    assert_wide_null_row(&batches[0]);
}

/// PATH 2: KV full scan (`SELECT *`, no filter / no LIMIT) decodes every scalar
/// type for every id; the changelog merge yields all three seeded rows.
async fn kv_full_scan_decodes_every_scalar_type(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!("SELECT * FROM {}", kv_wide()))
        .await
        .expect("plan full scan")
        .collect()
        .await
        .expect("collect full scan");
    assert_eq!(total_rows(&batches), 3, "full scan returns all 3 rows");

    // Slice out each id into a single-row batch and run the same per-type checks.
    for id in [1, 2] {
        let row = single_row_for_id(&batches, id);
        assert_wide_value_row(&row, id);
    }
    let null_row = single_row_for_id(&batches, 3);
    assert_wide_null_row(&null_row);
}

/// PATH 3: log scan (`SELECT ... LIMIT n`) decodes every scalar type.
async fn log_scan_decodes_every_scalar_type(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!("SELECT * FROM {} LIMIT 3", log_wide()))
        .await
        .expect("plan log scan")
        .collect()
        .await
        .expect("collect log scan");
    assert_eq!(total_rows(&batches), 3, "log LIMIT 3 returns 3 rows");

    for id in [1, 2] {
        let row = single_row_for_id(&batches, id);
        assert_wide_value_row(&row, id);
    }
    let null_row = single_row_for_id(&batches, 3);
    assert_wide_null_row(&null_row);
}

/// Nested types (array/map/row) over the log-scan path: assert the List / Map /
/// Struct Arrow types AND the decoded values.
async fn log_scan_decodes_nested_types(ctx: &SessionContext) {
    let batches = ctx
        .sql(&format!("SELECT * FROM {} LIMIT 1", log_nested()))
        .await
        .expect("plan nested scan")
        .collect()
        .await
        .expect("collect nested scan");
    assert_eq!(total_rows(&batches), 1);
    let batch = &batches[0];

    // --- ARRAY<INT> -> Arrow List(Int32) ---
    match col(batch, "c_array").data_type() {
        DataType::List(field) => {
            assert_eq!(field.data_type(), &DataType::Int32, "list element type");
        }
        other => panic!("c_array should be List, got {other:?}"),
    }
    let list = col(batch, "c_array")
        .as_any()
        .downcast_ref::<ListArray>()
        .expect("c_array as ListArray");
    let elems = list.value(0);
    let elems = elems.as_any().downcast_ref::<Int32Array>().unwrap();
    assert_eq!(elems.len(), 3);
    assert_eq!(
        (0..3).map(|i| elems.value(i)).collect::<Vec<_>>(),
        vec![10, 20, 30],
    );

    // --- MAP<STRING,INT> -> Arrow Map(Struct{key,value}) ---
    assert!(
        matches!(col(batch, "c_map").data_type(), DataType::Map(_, _)),
        "c_map should be a Map type, got {:?}",
        col(batch, "c_map").data_type()
    );
    let map = col(batch, "c_map")
        .as_any()
        .downcast_ref::<MapArray>()
        .expect("c_map as MapArray");
    let keys = map.keys().as_any().downcast_ref::<StringArray>().unwrap();
    let vals = map.values().as_any().downcast_ref::<Int32Array>().unwrap();
    let mut pairs: Vec<(String, i32)> = (0..keys.len())
        .map(|i| (keys.value(i).to_string(), vals.value(i)))
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![("a".to_string(), 1), ("b".to_string(), 2)],
        "map entries",
    );

    // --- ROW<seq INT, label STRING> -> Arrow Struct ---
    match col(batch, "c_row").data_type() {
        DataType::Struct(fields) => {
            assert_eq!(fields.len(), 2, "struct field count");
            assert_eq!(fields[0].data_type(), &DataType::Int32);
            assert_eq!(fields[1].data_type(), &DataType::Utf8);
        }
        other => panic!("c_row should be Struct, got {other:?}"),
    }
    let st = col(batch, "c_row")
        .as_any()
        .downcast_ref::<StructArray>()
        .expect("c_row as StructArray");
    let seq = st
        .column_by_name("seq")
        .unwrap()
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    let label = st
        .column_by_name("label")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(seq.value(0), 7);
    assert_eq!(label.value(0), "open");
}

/// Extracts the single row with the given `id` across all batches into a
/// one-row `RecordBatch`, preserving schema. Panics if not found / not unique.
fn single_row_for_id(batches: &[RecordBatch], id: i32) -> RecordBatch {
    for b in batches {
        let ids = col(b, "id")
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("id Int32");
        for r in 0..b.num_rows() {
            if ids.value(r) == id {
                return b.slice(r, 1);
            }
        }
    }
    panic!("row id={id} not found");
}
