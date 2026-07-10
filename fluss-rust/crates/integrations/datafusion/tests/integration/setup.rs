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

//! Shared real-cluster setup for the `integration_tests` path.
//!
//! Brings up a one-off Fluss cluster and creates/populates the known Phase 1
//! tables. The end-to-end path drives SQL through the real backend on top of the
//! DDL/DML defined here.
//!
//! Gated by `integration_tests` (needs a container runtime).

#![cfg(feature = "integration_tests")]

use std::collections::HashMap;
use std::time::Duration;

use fluss::client::FlussConnection;
use fluss::metadata::{
    DataField, DataTypes, PartitionSpec, Schema, TableDescriptor, TableInfo, TablePath,
};
use fluss::row::binary_array::FlussArrayWriter;
use fluss::row::binary_map::FlussMapWriter;
use fluss::row::{Datum, Date, Decimal, GenericRow, Time, TimestampLtz, TimestampNtz};
use fluss::rpc::message::OffsetSpec;
use fluss_test_cluster::{FlussTestingCluster, FlussTestingClusterBuilder};

use crate::integration::utils::names;

const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Brings up a one-off plaintext cluster and waits for it to serve metadata.
///
/// `name` namespaces the containers; `port` is the coordinator's host port. Two
/// clusters in the same test binary must use distinct names and ports so they do
/// not collide when the harness runs tests concurrently.
pub async fn start_cluster(name: &str, port: u16) -> FlussTestingCluster {
    let cluster = FlussTestingClusterBuilder::new(name)
        .with_port(port)
        .build()
        .await;
    wait_for_ready(&cluster).await;
    cluster
}

async fn wait_for_ready(cluster: &FlussTestingCluster) {
    let start = std::time::Instant::now();
    loop {
        let connection = cluster.get_fluss_connection().await;
        if connection
            .get_metadata()
            .get_cluster()
            .get_one_available_server()
            .is_some()
        {
            return;
        }
        if start.elapsed() >= READY_TIMEOUT {
            panic!("cluster did not become ready in {READY_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Creates and populates the single-PK KV table; returns its `TableInfo`.
pub async fn create_kv_simple(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::KV_SIMPLE);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .column("age", DataTypes::bigint())
                .primary_key(vec!["id"])
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_upsert().unwrap().create_writer().unwrap();
    let rows = [(1, "Verso", 32i64), (2, "Noco", 25), (3, "Esquie", 35)];
    for (id, name, age) in &rows {
        let mut row = GenericRow::new(3);
        row.set_field(0, *id);
        row.set_field(1, *name);
        row.set_field(2, *age);
        writer.upsert(&row).unwrap();
    }
    writer.flush().await.unwrap();

    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates and populates the composite-PK KV table; returns its `TableInfo`.
pub async fn create_kv_composite(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::KV_COMPOSITE);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("region", DataTypes::string())
                .column("id", DataTypes::int())
                .column("score", DataTypes::bigint())
                .primary_key(vec!["region", "id"])
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_upsert().unwrap().create_writer().unwrap();
    let rows = [("us", 1, 100i64), ("us", 2, 200), ("eu", 1, 300)];
    for (region, id, score) in &rows {
        let mut row = GenericRow::new(3);
        row.set_field(0, *region);
        row.set_field(1, *id);
        row.set_field(2, *score);
        writer.upsert(&row).unwrap();
    }
    writer.flush().await.unwrap();

    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates and populates the single-bucket log table, waits until its bucket can
/// serve reads, and returns its `TableInfo`.
pub async fn create_log_basic(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::LOG_BASIC);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .build()
                .unwrap(),
        )
        // Single bucket keeps the bounded scan deterministic.
        .distributed_by(Some(1), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_append().unwrap().create_writer().unwrap();
    writer.append_arrow_batch(log_batch()).unwrap();
    writer.flush().await.unwrap();

    // Wait until the bucket can serve reads before any bounded scan runs.
    wait_for_offsets(connection, &table_path).await;

    admin.get_table_info(&table_path).await.unwrap()
}

/// Bucket count for the multi-bucket log table.
const MULTI_BUCKET_COUNT: i32 = 3;

/// Creates and populates the multi-bucket log table, waits until ALL of its
/// buckets can serve reads, and returns its `TableInfo`.
///
/// Seeds 12 rows across 3 buckets so a bounded scan exercises per-bucket
/// parallel partitions plus DataFusion's cross-bucket final `LIMIT`.
pub async fn create_log_multi_bucket(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::LOG_MULTI_BUCKET);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .build()
                .unwrap(),
        )
        // Multiple buckets exercise the per-bucket parallel scan path.
        .distributed_by(Some(MULTI_BUCKET_COUNT), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_append().unwrap().create_writer().unwrap();
    writer.append_arrow_batch(multi_bucket_log_batch()).unwrap();
    writer.flush().await.unwrap();

    // Every bucket is read as its own partition, so all must be ready first.
    let buckets: Vec<i32> = (0..MULTI_BUCKET_COUNT).collect();
    wait_for_bucket_offsets(connection, &table_path, &buckets).await;

    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates and populates a single-bucket log table under an arbitrary name,
/// waiting until its bucket can serve reads. Used to create tables on demand
/// (e.g. AFTER `register_catalog`) so the live-metadata test can prove visibility.
pub async fn create_log_named(connection: &FlussConnection, table_name: &str) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, table_name);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .build()
                .unwrap(),
        )
        // Single bucket keeps the bounded scan deterministic.
        .distributed_by(Some(1), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_append().unwrap().create_writer().unwrap();
    writer.append_arrow_batch(log_batch()).unwrap();
    writer.flush().await.unwrap();

    wait_for_offsets(connection, &table_path).await;

    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates and populates the partitioned log table (partition key `region`, one
/// bucket per partition), waits until each partition's bucket can serve reads, and
/// returns its `TableInfo`. Used to exercise partition pruning of bounded scans.
pub async fn create_log_partitioned(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::LOG_PARTITIONED);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .column("region", DataTypes::string())
                .build()
                .unwrap(),
        )
        .partitioned_by(vec!["region"])
        // Single bucket per partition keeps the bounded scan deterministic.
        .distributed_by(Some(1), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    // Create the two partitions up front; the writer routes rows by region.
    for region in ["US", "EU"] {
        let spec = PartitionSpec::new(HashMap::from([("region", region)]));
        admin
            .create_partition(&table_path, &spec, true)
            .await
            .unwrap();
        wait_for_partition_offsets(connection, &table_path, region).await;
    }

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_append().unwrap().create_writer().unwrap();
    let rows = [
        (1, "a", "US"),
        (2, "b", "US"),
        (3, "c", "EU"),
        (4, "d", "EU"),
    ];
    for (id, name, region) in &rows {
        let mut row = GenericRow::new(3);
        row.set_field(0, *id);
        row.set_field(1, *name);
        row.set_field(2, *region);
        writer.append(&row).unwrap();
    }
    writer.flush().await.unwrap();

    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates and populates the partitioned KV table (partition key `region`, which
/// is also part of the primary key), waits until each partition can serve reads,
/// and returns its `TableInfo`. Used to prove a full-PK lookup resolves the
/// partition automatically via the Fluss `Lookuper`.
pub async fn create_kv_partitioned(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::KV_PARTITIONED);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("region", DataTypes::string())
                .column("id", DataTypes::int())
                .column("score", DataTypes::bigint())
                // For a partitioned PK table the partition key must be part of the PK.
                .primary_key(vec!["region", "id"])
                .build()
                .unwrap(),
        )
        .partitioned_by(vec!["region"])
        .distributed_by(Some(1), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    for region in ["US", "EU"] {
        let spec = PartitionSpec::new(HashMap::from([("region", region)]));
        admin
            .create_partition(&table_path, &spec, true)
            .await
            .unwrap();
        wait_for_partition_offsets(connection, &table_path, region).await;
    }

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_upsert().unwrap().create_writer().unwrap();
    let rows = [("US", 1, 100i64), ("US", 2, 200), ("EU", 1, 300)];
    for (region, id, score) in &rows {
        let mut row = GenericRow::new(3);
        row.set_field(0, *region);
        row.set_field(1, *id);
        row.set_field(2, *score);
        writer.upsert(&row).unwrap();
    }
    writer.flush().await.unwrap();

    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates and populates a single-bucket KV table with several rows, waits until
/// its bucket can serve reads, and returns its `TableInfo`. Used to prove a
/// bounded `LIMIT` scan over a primary-keyed table returns exactly `LIMIT` rows
/// while a full-PK equality still resolves a single row.
pub async fn create_kv_bounded(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::KV_BOUNDED);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .primary_key(vec!["id"])
                .build()
                .unwrap(),
        )
        // Single bucket keeps the bounded scan deterministic and ready-to-read.
        .distributed_by(Some(1), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_upsert().unwrap().create_writer().unwrap();
    let rows = [
        (1, "a"),
        (2, "b"),
        (3, "c"),
        (4, "d"),
        (5, "e"),
        (6, "f"),
    ];
    for (id, name) in &rows {
        let mut row = GenericRow::new(2);
        row.set_field(0, *id);
        row.set_field(1, *name);
        writer.upsert(&row).unwrap();
    }
    writer.flush().await.unwrap();

    // Wait until the bucket can serve reads before any bounded scan runs.
    wait_for_offsets(connection, &table_path).await;

    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates and populates a KV table whose bucket key is a STRICT prefix of the
/// PK: `PRIMARY KEY (c1, c2)` with `bucket_keys=["c1"]`. Seeded so one `c1`
/// value matches several rows, proving the prefix lookup returns multiple rows.
pub async fn create_kv_prefix(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::KV_PREFIX);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("c1", DataTypes::int())
                .column("c2", DataTypes::int())
                .column("name", DataTypes::string())
                .primary_key(vec!["c1", "c2"])
                .build()
                .unwrap(),
        )
        .distributed_by(Some(1), vec!["c1".to_string()])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_upsert().unwrap().create_writer().unwrap();
    let rows = [
        (10, 1, "a"),
        (10, 2, "b"),
        (10, 3, "c"),
        (20, 1, "d"),
        (30, 1, "e"),
    ];
    for (c1, c2, name) in &rows {
        let mut row = GenericRow::new(3);
        row.set_field(0, *c1);
        row.set_field(1, *c2);
        row.set_field(2, *name);
        writer.upsert(&row).unwrap();
    }
    writer.flush().await.unwrap();

    wait_for_offsets(connection, &table_path).await;

    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates a single-bucket KV table, seeds inserts, then UPDATES some rows and
/// DELETES one, waiting until its bucket can serve reads. Returns its
/// `TableInfo`. Used to prove a full-table scan (no filter / no `LIMIT`) merges
/// the CDC changelog into the correct current state.
///
/// Seeds ids 1..=5, then updates id=2 (name -> "bravo-v2") and id=4
/// (name -> "delta-v2"), then deletes id=3. The expected current state is
/// {1:alpha, 2:bravo-v2, 4:delta-v2, 5:echo}.
pub async fn create_kv_full_scan(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::KV_FULL_SCAN);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .primary_key(vec!["id"])
                .build()
                .unwrap(),
        )
        // Single bucket keeps the whole table on bucket 0 and the merge deterministic.
        .distributed_by(Some(1), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_upsert().unwrap().create_writer().unwrap();

    // Inserts.
    for (id, name) in [(1, "alpha"), (2, "bravo"), (3, "charlie"), (4, "delta"), (5, "echo")] {
        let mut row = GenericRow::new(2);
        row.set_field(0, id);
        row.set_field(1, name);
        writer.upsert(&row).unwrap();
    }
    writer.flush().await.unwrap();

    // Updates: id=2 and id=4.
    for (id, name) in [(2, "bravo-v2"), (4, "delta-v2")] {
        let mut row = GenericRow::new(2);
        row.set_field(0, id);
        row.set_field(1, name);
        writer.upsert(&row).unwrap();
    }
    writer.flush().await.unwrap();

    // Delete: id=3.
    let mut delete_row = GenericRow::new(2);
    delete_row.set_field(0, 3);
    writer.delete(&delete_row).unwrap();
    writer.flush().await.unwrap();

    wait_for_offsets(connection, &table_path).await;

    admin.get_table_info(&table_path).await.unwrap()
}

// ===========================================================================
// Wide column-type-coverage tables.
//
// These exercise the SQL read path for EVERY scalar Fluss type (plus nested
// types) through the three decode paths that matter:
//   * KV point lookup  (`WHERE id = <k>`)
//   * KV full scan      (`SELECT *`)
//   * Log scan          (`SELECT ... LIMIT n`)
//
// Seeded values are chosen so the expected Arrow integer/decimal value is easy
// to reason about (see the per-type notes in `type_coverage.rs`).
// ===========================================================================

/// Ordered list of `(name, DataType)` value columns covering every scalar Fluss
/// type. The wide KV and wide log tables share this column set so the same
/// per-type assertions apply across all three decode paths. `id INT` (the PK /
/// first column) is added separately by each table builder.
pub fn wide_scalar_columns() -> Vec<(&'static str, fluss::metadata::DataType)> {
    vec![
        ("c_boolean", DataTypes::boolean()),
        ("c_tinyint", DataTypes::tinyint()),
        ("c_smallint", DataTypes::smallint()),
        ("c_int", DataTypes::int()),
        ("c_bigint", DataTypes::bigint()),
        ("c_float", DataTypes::float()),
        ("c_double", DataTypes::double()),
        ("c_decimal", DataTypes::decimal(10, 2)),
        ("c_char", DataTypes::char(8)),
        ("c_string", DataTypes::string()),
        ("c_bytes", DataTypes::bytes()),
        ("c_binary", DataTypes::binary(4)),
        ("c_date", DataTypes::date()),
        ("c_time", DataTypes::time_with_precision(3)),
        ("c_timestamp", DataTypes::timestamp_with_precision(6)),
        ("c_ts_ltz", DataTypes::timestamp_ltz_with_precision(6)),
    ]
}

/// Builds a fully-populated value row for `id` over `wide_scalar_columns()`.
///
/// All seeded values are deterministic and distinct per row so the full-scan and
/// point-lookup assertions can key on `id`. The logical values are documented in
/// `type_coverage.rs` next to the assertions that read them back.
fn wide_value_row(id: i32) -> GenericRow<'static> {
    // The first field is `id`; value columns follow `wide_scalar_columns()` order.
    let mut row = GenericRow::new(1 + wide_scalar_columns().len());
    let n = id as i64;
    row.set_field(0, id);
    row.set_field(1, id % 2 == 0); // c_boolean
    row.set_field(2, (id + 1) as i8); // c_tinyint
    row.set_field(3, (id + 2) as i16); // c_smallint
    row.set_field(4, id + 3); // c_int
    row.set_field(5, n + 4); // c_bigint
    row.set_field(6, id as f32 + 0.5_f32); // c_float
    row.set_field(7, id as f64 + 0.25_f64); // c_double
    // c_decimal(10,2): unscaled = id*100 + 45 -> logical value id.45
    row.set_field(
        8,
        Decimal::from_unscaled_long((n * 100) + 45, 10, 2).unwrap(),
    );
    // c_char(8): owned String kept alive via the row's lifetime.
    row.set_field(9, Datum::from(format!("c{id}")));
    row.set_field(10, Datum::from(format!("s{id}"))); // c_string
    row.set_field(11, Datum::from(vec![id as u8, (id + 1) as u8])); // c_bytes
    row.set_field(12, Datum::from(vec![id as u8; 4])); // c_binary(4)
    row.set_field(13, Datum::Date(Date::new(20000 + id))); // c_date (epoch day)
    // c_time(3): ms since midnight = id*1000 (whole seconds, simple to reason about).
    row.set_field(14, Datum::Time(Time::new(id * 1000)));
    // c_timestamp(6): millis since epoch, no sub-ms nanos -> micros = millis*1000.
    row.set_field(
        15,
        Datum::TimestampNtz(TimestampNtz::new(1_700_000_000_000 + n)),
    );
    // c_ts_ltz(6): same instant model.
    row.set_field(
        16,
        Datum::TimestampLtz(TimestampLtz::new(1_700_000_000_000 + n)),
    );
    row
}

/// Builds a row for `id` whose value columns are ALL null (the PK stays set).
fn wide_null_row(id: i32) -> GenericRow<'static> {
    let count = 1 + wide_scalar_columns().len();
    let mut row = GenericRow::new(count);
    row.set_field(0, id);
    for i in 1..count {
        row.set_field(i, Datum::Null);
    }
    row
}

fn wide_schema(pk: Option<&[&str]>) -> Schema {
    let mut sb = Schema::builder().column("id", DataTypes::int());
    for (name, dt) in wide_scalar_columns() {
        sb = sb.column(name, dt);
    }
    if let Some(keys) = pk {
        sb = sb.primary_key(keys.iter().copied());
    }
    sb.build().expect("wide schema build")
}

/// Creates and populates the wide KV table (PK `id INT`, one value column per
/// scalar type). Seeds two fully-populated rows (id=1, id=2) plus one all-null
/// row (id=3) so both the point-lookup and full-scan paths can assert per-type
/// values AND null handling. Returns its `TableInfo`.
pub async fn create_kv_wide_types(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::KV_WIDE_TYPES);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(wide_schema(Some(&["id"])))
        // Single bucket keeps the full scan deterministic and ready-to-read.
        .distributed_by(Some(1), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_upsert().unwrap().create_writer().unwrap();
    writer.upsert(&wide_value_row(1)).unwrap();
    writer.upsert(&wide_value_row(2)).unwrap();
    writer.upsert(&wide_null_row(3)).unwrap();
    writer.flush().await.unwrap();

    wait_for_offsets(connection, &table_path).await;
    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates and populates the wide log table (no PK, single bucket, one value
/// column per scalar type). Seeds three rows (id=1, id=2 fully populated; id=3
/// all-null value columns) and waits until its bucket can serve reads. Returns
/// its `TableInfo`.
pub async fn create_log_wide_types(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::LOG_WIDE_TYPES);
    let admin = connection.get_admin().unwrap();
    let descriptor = TableDescriptor::builder()
        .schema(wide_schema(None))
        // Single bucket keeps the bounded LIMIT scan deterministic (head-first).
        .distributed_by(Some(1), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_append().unwrap().create_writer().unwrap();
    writer.append(&wide_value_row(1)).unwrap();
    writer.append(&wide_value_row(2)).unwrap();
    writer.append(&wide_null_row(3)).unwrap();
    writer.flush().await.unwrap();

    wait_for_offsets(connection, &table_path).await;
    admin.get_table_info(&table_path).await.unwrap()
}

/// Creates and populates the nested-types log table: `id INT` plus an
/// `ARRAY<INT>`, a `MAP<STRING,INT>`, and a `ROW<seq INT, label STRING>` value
/// column. Single bucket; waits until its bucket can serve reads. Used to prove
/// the log-scan path decodes List / Map / Struct Arrow types.
pub async fn create_log_nested_types(connection: &FlussConnection) -> TableInfo {
    let table_path = TablePath::new(names::DATABASE, names::LOG_NESTED_TYPES);
    let admin = connection.get_admin().unwrap();
    let row_type = DataTypes::row(vec![
        DataField::new("seq", DataTypes::int(), None),
        DataField::new("label", DataTypes::string(), None),
    ]);
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("c_array", DataTypes::array(DataTypes::int()))
                .column("c_map", DataTypes::map(DataTypes::string(), DataTypes::int()))
                .column("c_row", row_type)
                .build()
                .unwrap(),
        )
        .distributed_by(Some(1), vec![])
        .build()
        .unwrap();
    admin
        .create_table(&table_path, &descriptor, true)
        .await
        .unwrap();

    let table = connection.get_table(&table_path).await.unwrap();
    let writer = table.new_append().unwrap().create_writer().unwrap();

    // Row id=1: array [10,20,30], map {a:1,b:2}, row {seq:7,label:"open"}.
    let arr = {
        let mut w = FlussArrayWriter::new(3, &DataTypes::int());
        w.write_int(0, 10);
        w.write_int(1, 20);
        w.write_int(2, 30);
        w.complete().expect("c_array")
    };
    let map = {
        let mut w = FlussMapWriter::new(2, &DataTypes::string(), &DataTypes::int());
        w.write_entry("a".into(), 1.into()).unwrap();
        w.write_entry("b".into(), 2.into()).unwrap();
        w.complete().expect("c_map")
    };
    let mut nested = GenericRow::new(2);
    nested.set_field(0, 7_i32);
    nested.set_field(1, "open");

    let mut row = GenericRow::new(4);
    row.set_field(0, 1_i32);
    row.set_field(1, arr);
    row.set_field(2, Datum::Map(map));
    row.set_field(3, Datum::Row(Box::new(nested)));
    writer.append(&row).unwrap();
    writer.flush().await.unwrap();

    wait_for_offsets(connection, &table_path).await;
    admin.get_table_info(&table_path).await.unwrap()
}

/// Best-effort drop of a single table under `names::DATABASE`.
pub async fn drop_table_named(connection: &FlussConnection, table_name: &str) {
    let admin = connection.get_admin().unwrap();
    let _ = admin
        .drop_table(&TablePath::new(names::DATABASE, table_name), true)
        .await;
}

/// Builds the log table's input batch with explicit Arrow arrays.
pub fn log_batch() -> arrow::array::RecordBatch {
    use std::sync::Arc;

    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};

    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("id", DataType::Int32, true),
        Field::new("name", DataType::Utf8, true),
    ]));
    let ids = Int32Array::from(vec![1, 2, 3, 4, 5, 6]);
    let names = StringArray::from(vec!["a", "b", "c", "d", "e", "f"]);
    arrow::array::RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(names)])
        .expect("build log batch")
}

/// Builds the multi-bucket log table's input batch: 12 rows that spread across
/// the table's buckets, so a bounded scan must merge several partitions.
pub fn multi_bucket_log_batch() -> arrow::array::RecordBatch {
    use std::sync::Arc;

    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};

    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("id", DataType::Int32, true),
        Field::new("name", DataType::Utf8, true),
    ]));
    let ids: Vec<i32> = (1..=12).collect();
    let names: Vec<String> = ids.iter().map(|i| format!("r{i}")).collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    arrow::array::RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int32Array::from(ids)),
            Arc::new(StringArray::from(name_refs)),
        ],
    )
    .expect("build multi-bucket log batch")
}

/// Waits until bucket 0 of `table_path` can serve reads.
async fn wait_for_offsets(connection: &FlussConnection, table_path: &TablePath) {
    wait_for_bucket_offsets(connection, table_path, &[0]).await;
}

/// Waits until every bucket in `buckets` of `table_path` can serve reads.
///
/// A multi-bucket bounded scan reads each bucket as its own partition, so all of
/// them must be ready before the scan runs or a bucket could return zero rows.
async fn wait_for_bucket_offsets(
    connection: &FlussConnection,
    table_path: &TablePath,
    buckets: &[i32],
) {
    let admin = connection.get_admin().unwrap();
    let start = std::time::Instant::now();
    loop {
        if admin
            .list_offsets(table_path, buckets, OffsetSpec::Latest)
            .await
            .is_ok()
        {
            return;
        }
        if start.elapsed() >= READY_TIMEOUT {
            panic!("table {table_path} buckets not ready in {READY_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Waits until bucket 0 of partition `partition_value` of `table_path` can serve
/// reads. A partition-qualified bounded scan reads each partition's bucket, so the
/// partition must be ready before the scan runs.
async fn wait_for_partition_offsets(
    connection: &FlussConnection,
    table_path: &TablePath,
    partition_value: &str,
) {
    let admin = connection.get_admin().unwrap();
    let start = std::time::Instant::now();
    loop {
        if admin
            .list_partition_offsets(table_path, partition_value, &[0], OffsetSpec::Latest)
            .await
            .is_ok()
        {
            return;
        }
        if start.elapsed() >= READY_TIMEOUT {
            panic!("partition {partition_value} of {table_path} not ready in {READY_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Best-effort drop of the Phase 1 tables so reruns start clean.
pub async fn drop_phase1_tables(connection: &FlussConnection) {
    let admin = connection.get_admin().unwrap();
    for name in [
        names::KV_SIMPLE,
        names::KV_COMPOSITE,
        names::LOG_BASIC,
        names::LOG_MULTI_BUCKET,
        names::LOG_PARTITIONED,
        names::KV_PARTITIONED,
        names::KV_BOUNDED,
        names::KV_PREFIX,
    ] {
        let _ = admin
            .drop_table(&TablePath::new(names::DATABASE, name), true)
            .await;
    }
}
