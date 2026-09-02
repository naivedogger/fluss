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

#![cfg(feature = "integration_tests")]

mod support;

use arrow::array::{Array, ArrayRef, Int32Array, StringArray};
use arrow::record_batch::RecordBatch;
use fluss::metadata::{
    AddColumn, AlterTableChanges, ColumnPositionType, DataTypes, JsonSerde, PartitionSpec, Schema,
    TableDescriptor, TablePath,
};
use fluss::predicate::col;
#[cfg(feature = "paimon")]
use fluss::row::GenericRow;
use fluss_lake::{FlussLakeError, FlussLakePartitionIdentity, FlussLakeReadSplit, FlussLakeTable};
use futures::TryStreamExt;
#[cfg(feature = "paimon")]
use std::collections::HashMap;
#[cfg(feature = "paimon")]
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn append_batch(ids: Vec<i32>, names: Vec<&str>) -> RecordBatch {
    RecordBatch::try_from_iter(vec![
        ("id", Arc::new(Int32Array::from(ids)) as ArrayRef),
        ("name", Arc::new(StringArray::from(names)) as ArrayRef),
    ])
    .expect("Failed to build append record batch")
}

#[cfg(feature = "paimon")]
fn partitioned_append_batch(ids: Vec<i32>, names: Vec<&str>, region: &str) -> RecordBatch {
    let regions = vec![region; ids.len()];
    RecordBatch::try_from_iter(vec![
        ("id", Arc::new(Int32Array::from(ids)) as ArrayRef),
        ("name", Arc::new(StringArray::from(names)) as ArrayRef),
        ("region", Arc::new(StringArray::from(regions)) as ArrayRef),
    ])
    .expect("Failed to build partitioned append record batch")
}

#[cfg(feature = "paimon")]
async fn create_ready_partition(
    admin: &fluss::client::FlussAdmin,
    table_path: &TablePath,
    region: &str,
) -> i64 {
    let partition_spec = PartitionSpec::new(HashMap::from([("region", region)]));
    admin
        .create_partition(table_path, &partition_spec, false)
        .await
        .expect("Failed to create UnionRead test partition");

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(partitions) = admin.list_partition_infos(table_path).await
                && let Some(partition) = partitions
                    .iter()
                    .find(|partition| partition.get_partition_name() == region)
                && admin
                    .list_partition_offsets(
                        table_path,
                        region,
                        &[0],
                        fluss::rpc::message::OffsetSpec::Latest,
                    )
                    .await
                    .is_ok()
            {
                return partition.get_partition_id();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Timed out waiting for UnionRead test partition")
}

#[cfg(feature = "paimon")]
fn extract_ids(batches: &[RecordBatch]) -> Vec<i32> {
    let mut ids: Vec<i32> = batches
        .iter()
        .flat_map(|batch| {
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("id column should be an Int32Array");
            (0..ids.len()).map(|row| ids.value(row)).collect::<Vec<_>>()
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[cfg(feature = "paimon")]
fn pk_row(id: i32, name: Option<&str>) -> GenericRow<'_> {
    let mut row = GenericRow::new(2);
    row.set_field(0, id);
    if let Some(name) = name {
        row.set_field(1, name);
    }
    row
}

#[cfg(feature = "paimon")]
fn extract_pk_rows(batches: &[RecordBatch]) -> Vec<(i32, String)> {
    let mut rows = Vec::new();
    for batch in batches {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("id column should be an Int32Array");
        let names = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("name column should be a StringArray");
        for row in 0..batch.num_rows() {
            rows.push((ids.value(row), names.value(row).to_string()));
        }
    }
    rows.sort_unstable();
    rows
}

#[cfg(feature = "paimon")]
fn find_table_parquet_file(table_name: &str) -> PathBuf {
    fn visit(directory: &Path, table_name: &str, result: &mut Option<PathBuf>) {
        if result.is_some() {
            return;
        }
        let entries = std::fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("Failed to list {}: {error}", directory.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|error| {
                    panic!(
                        "Failed to read entry below {}: {error}",
                        directory.display()
                    )
                })
                .path();
            if path.is_dir() {
                visit(&path, table_name, result);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "parquet")
                && path
                    .components()
                    .any(|component| component.as_os_str() == table_name)
            {
                *result = Some(path);
                return;
            }
        }
    }

    let mut result = None;
    visit(support::paimon_warehouse_path(), table_name, &mut result);
    result.unwrap_or_else(|| {
        panic!(
            "No Parquet data file found for table {table_name} below {}",
            support::paimon_warehouse_path().display()
        )
    })
}

#[cfg(feature = "paimon")]
fn s3_catalog_overrides(
    fixture: &support::S3Fixture,
    include_credentials: bool,
) -> HashMap<String, String> {
    let mut properties = HashMap::from([
        (
            "table.datalake.paimon.warehouse".to_string(),
            fixture.warehouse(),
        ),
        (
            "table.datalake.paimon.s3.endpoint".to_string(),
            fixture.endpoint().to_string(),
        ),
        (
            "table.datalake.paimon.s3.region".to_string(),
            "us-east-1".to_string(),
        ),
        (
            "table.datalake.paimon.s3.path-style-access".to_string(),
            "true".to_string(),
        ),
    ]);
    if include_credentials {
        properties.insert(
            "table.datalake.paimon.s3.access-key".to_string(),
            fixture.access_key().to_string(),
        );
        properties.insert(
            "table.datalake.paimon.s3.secret-key".to_string(),
            fixture.secret_key().to_string(),
        );
    }
    properties
}

#[cfg(feature = "paimon")]
fn serialized_execution_descriptor(split: &FlussLakeReadSplit) -> Vec<u8> {
    serde_json::to_value(split)
        .expect("Failed to serialize split as JSON value")
        .get("execution_descriptor")
        .and_then(serde_json::Value::as_array)
        .expect("Serialized split must contain an execution_descriptor byte array")
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| u8::try_from(value).ok())
                .expect("Execution descriptor must contain bytes")
        })
        .collect()
}

/// Real PK UnionRead across a Paimon snapshot produced by Fluss' production
/// Java/Flink tiering pipeline and a bounded Fluss changelog tail.
#[cfg(feature = "paimon")]
#[tokio::test]
async fn paimon_pk_union_read_merges_update_delete_and_insert_end_to_end() {
    let cluster = support::get_shared_cluster();
    let connection = Arc::new(cluster.get_fluss_connection().await);
    let admin = connection.get_admin().expect("Failed to get Fluss admin");
    let table_path = TablePath::new("fluss", "test_paimon_pk_union_read_e2e");
    let table_descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .primary_key(vec!["id"])
                .build()
                .expect("Failed to build Paimon PK UnionRead schema"),
        )
        .distributed_by(Some(1), vec!["id".to_string()])
        .property("table.datalake.enabled", "true")
        .property("table.datalake.format", "paimon")
        .property("table.datalake.freshness", "1s")
        .custom_property("paimon.file.format", "parquet")
        .build()
        .expect("Failed to build Paimon PK UnionRead table descriptor");
    support::create_table(&admin, &table_path, &table_descriptor).await;

    let table = connection
        .get_table(&table_path)
        .await
        .expect("Failed to open Paimon PK UnionRead table");
    let writer = table
        .new_upsert()
        .expect("Failed to create Paimon PK upsert operation")
        .create_writer()
        .expect("Failed to create Paimon PK writer");
    for row in [
        pk_row(1, Some("lake-old")),
        pk_row(2, Some("lake-delete")),
        pk_row(3, Some("lake-keep")),
    ] {
        writer
            .upsert(&row)
            .expect("Failed to queue Fluss baseline upsert")
            .await
            .expect("Failed to acknowledge Fluss baseline upsert");
    }
    writer
        .flush()
        .await
        .expect("Failed to flush Fluss baseline");

    let table_info = admin
        .get_table_info(&table_path)
        .await
        .expect("Failed to resolve Paimon PK table metadata");
    let seam_offset = admin
        .list_offsets(&table_path, &[0], fluss::rpc::message::OffsetSpec::Latest)
        .await
        .expect("Failed to resolve Paimon PK seam offset")[&0];
    assert!(seam_offset > 0);

    support::run_java_paimon_tiering_until_offset(
        cluster.plaintext_bootstrap_servers(),
        &table_path,
        table_info.table_id,
        None,
        seam_offset,
    )
    .await;
    let readable_snapshot = admin
        .get_readable_lake_snapshot(&table_path)
        .await
        .expect("Failed to load Java-tiered readable Paimon snapshot");
    assert!(readable_snapshot.snapshot_id >= 0);
    assert_eq!(readable_snapshot.bucket_snapshots.len(), 1);
    assert_eq!(
        readable_snapshot.bucket_snapshots[0].log_offset,
        Some(seam_offset)
    );

    writer
        .upsert(&pk_row(1, Some("tail-new")))
        .expect("Failed to queue tail update")
        .await
        .expect("Failed to acknowledge tail update");
    writer
        .delete(&pk_row(2, None))
        .expect("Failed to queue tail delete")
        .await
        .expect("Failed to acknowledge tail delete");
    writer
        .upsert(&pk_row(4, Some("tail-insert")))
        .expect("Failed to queue tail insert")
        .await
        .expect("Failed to acknowledge tail insert");
    writer
        .flush()
        .await
        .expect("Failed to flush Paimon PK changelog tail");

    let lake_table = FlussLakeTable::open(connection.clone(), &table_path)
        .await
        .expect("Failed to open real Paimon PK lake table");
    let scan = lake_table.new_scan();
    let plan = scan
        .plan()
        .await
        .expect("Failed to plan real Paimon PK UnionRead");
    assert_eq!(
        plan.splits().len(),
        1,
        "one bucket must produce one hybrid split containing all Paimon splits and its log tail"
    );
    let transported_split: FlussLakeReadSplit = serde_json::from_slice(
        &serde_json::to_vec(&plan.splits()[0]).expect("Failed to encode PK split"),
    )
    .expect("Failed to decode transported PK split");
    let read = scan.new_reader();

    let execute = |split| {
        let read = read.clone();
        async move {
            let stream = read
                .read_split(&split)
                .await
                .expect("Failed to execute real Paimon PK UnionRead split");
            stream.try_collect::<Vec<_>>().await
        }
    };
    let first = tokio::time::timeout(Duration::from_secs(20), execute(transported_split.clone()))
        .await
        .expect("Timed out waiting for real Paimon PK UnionRead")
        .expect("Failed to collect real Paimon PK UnionRead");
    let retried = tokio::time::timeout(Duration::from_secs(20), execute(transported_split))
        .await
        .expect("Timed out waiting for retried Paimon PK UnionRead")
        .expect("Failed to collect retried Paimon PK UnionRead");

    let expected = vec![
        (1, "tail-new".to_string()),
        (3, "lake-keep".to_string()),
        (4, "tail-insert".to_string()),
    ];
    assert_eq!(extract_pk_rows(&first), expected);
    assert_eq!(
        extract_pk_rows(&retried),
        expected,
        "retrying an immutable transported split must reproduce the same logical rows"
    );

    drop(writer);
    admin
        .drop_table(&table_path, false)
        .await
        .expect("Failed to drop Paimon PK UnionRead table");
}

/// Real append UnionRead across a Java-tiered Paimon snapshot and a bounded
/// Fluss log tail. This also verifies lake-only semantics and that retrying a
/// frozen split after its planned lake file disappears returns DataUnavailable.
#[cfg(feature = "paimon")]
#[tokio::test]
async fn paimon_append_union_read_has_no_gap_or_overlap_end_to_end() {
    let cluster = support::get_shared_cluster();
    let connection = Arc::new(cluster.get_fluss_connection().await);
    let admin = connection.get_admin().expect("Failed to get Fluss admin");
    let table_name = "test_paimon_append_union_read_e2e";
    let table_path = TablePath::new("fluss", table_name);
    let table_descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .build()
                .expect("Failed to build Paimon append UnionRead schema"),
        )
        .distributed_by(Some(1), vec!["id".to_string()])
        .property("table.datalake.enabled", "true")
        .property("table.datalake.format", "paimon")
        .property("table.datalake.freshness", "1s")
        .custom_property("paimon.file.format", "parquet")
        .build()
        .expect("Failed to build Paimon append UnionRead table descriptor");
    support::create_table(&admin, &table_path, &table_descriptor).await;

    let table = connection
        .get_table(&table_path)
        .await
        .expect("Failed to open Paimon append UnionRead table");
    let writer = table
        .new_append()
        .expect("Failed to create Paimon append operation")
        .create_writer()
        .expect("Failed to create Paimon append writer");
    writer
        .append_arrow_batch(append_batch(
            vec![1, 2, 3],
            vec!["lake-1", "lake-2", "lake-3"],
        ))
        .expect("Failed to append Paimon baseline batch");
    writer
        .flush()
        .await
        .expect("Failed to flush Paimon append baseline");

    let table_info = admin
        .get_table_info(&table_path)
        .await
        .expect("Failed to resolve Paimon append table metadata");
    let seam_offset = admin
        .list_offsets(&table_path, &[0], fluss::rpc::message::OffsetSpec::Latest)
        .await
        .expect("Failed to resolve Paimon append seam offset")[&0];
    assert_eq!(seam_offset, 3);

    support::run_java_paimon_tiering_until_offset(
        cluster.plaintext_bootstrap_servers(),
        &table_path,
        table_info.table_id,
        None,
        seam_offset,
    )
    .await;
    let readable_snapshot = admin
        .get_readable_lake_snapshot(&table_path)
        .await
        .expect("Failed to load Paimon append readable snapshot");
    assert_eq!(readable_snapshot.bucket_snapshots.len(), 1);
    assert_eq!(
        readable_snapshot.bucket_snapshots[0].log_offset,
        Some(seam_offset)
    );

    writer
        .append_arrow_batch(append_batch(vec![4, 5], vec!["tail-4", "tail-5"]))
        .expect("Failed to append Paimon log tail");
    writer
        .flush()
        .await
        .expect("Failed to flush Paimon log tail");

    let lake_table = FlussLakeTable::open(connection.clone(), &table_path)
        .await
        .expect("Failed to open Paimon append lake table");
    let union_scan = lake_table.new_scan();
    let union_plan = union_scan
        .plan()
        .await
        .expect("Failed to plan Paimon append UnionRead");
    assert_eq!(union_plan.splits().len(), 1);
    let transported_split: FlussLakeReadSplit = serde_json::from_slice(
        &serde_json::to_vec(&union_plan.splits()[0]).expect("Failed to encode Paimon append split"),
    )
    .expect("Failed to decode transported Paimon append split");

    support::mirror_paimon_warehouse_to_s3();
    let s3_fixture = support::get_s3_fixture();
    let s3_planner_table = FlussLakeTable::open_with_properties(
        connection.clone(),
        &table_path,
        s3_catalog_overrides(s3_fixture, true),
    )
    .await
    .expect("Failed to open S3-backed Paimon table for planning");
    let s3_planner_scan = s3_planner_table.new_scan();
    let s3_plan = s3_planner_scan
        .plan()
        .await
        .expect("Failed to plan S3-backed Paimon UnionRead");
    assert_eq!(s3_plan.splits().len(), 1);
    let s3_split: FlussLakeReadSplit = serde_json::from_slice(
        &serde_json::to_vec(&s3_plan.splits()[0]).expect("Failed to encode S3-backed Paimon split"),
    )
    .expect("Failed to decode transported S3-backed Paimon split");
    let encoded_s3_split = serialized_execution_descriptor(&s3_split);
    for secret in [s3_fixture.access_key(), s3_fixture.secret_key()] {
        assert!(
            !encoded_s3_split
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "credential material must not be serialized into a read split"
        );
    }
    assert!(
        !encoded_s3_split
            .windows(b"s3://".len())
            .any(|window| window == b"s3://"),
        "distributed split descriptors must contain storage-relative paths only"
    );
    let mut s3_reader_properties = s3_catalog_overrides(s3_fixture, false);
    s3_reader_properties.insert(
        "s3.access-key".to_string(),
        s3_fixture.access_key().to_string(),
    );
    s3_reader_properties.insert(
        "s3.secret-key".to_string(),
        s3_fixture.secret_key().to_string(),
    );
    let s3_reader_table =
        FlussLakeTable::open_with_properties(connection.clone(), &table_path, s3_reader_properties)
            .await
            .expect("Failed to open S3-backed Paimon table for execution");
    let s3_reader_scan = s3_reader_table.new_scan();
    let s3_reader = s3_reader_scan.new_reader();

    writer
        .append_arrow_batch(append_batch(vec![6], vec!["after-plan-6"]))
        .expect("Failed to append row after Paimon append planning");
    writer
        .flush()
        .await
        .expect("Failed to flush row after Paimon append planning");

    let union_reader = union_scan.new_reader();
    let union_stream = union_reader
        .read_split(&transported_split)
        .await
        .expect("Failed to execute Paimon append UnionRead split");
    let union_batches = tokio::time::timeout(
        Duration::from_secs(20),
        union_stream.try_collect::<Vec<_>>(),
    )
    .await
    .expect("Timed out waiting for Paimon append UnionRead")
    .expect("Failed to collect Paimon append UnionRead");
    assert_eq!(
        extract_ids(&union_batches),
        vec![1, 2, 3, 4, 5],
        "lake [0,seam) and log [seam,stop) must have no gap or overlap, and the frozen stop must exclude id=6"
    );

    let s3_stream = s3_reader
        .read_split(&s3_split)
        .await
        .expect("Failed to execute S3-backed Paimon UnionRead split");
    let s3_batches =
        tokio::time::timeout(Duration::from_secs(20), s3_stream.try_collect::<Vec<_>>())
            .await
            .expect("Timed out waiting for S3-backed Paimon UnionRead")
            .expect("Failed to collect S3-backed Paimon UnionRead");
    assert_eq!(
        extract_ids(&s3_batches),
        vec![1, 2, 3, 4, 5],
        "runtime credentials must allow the reader to load the lake snapshot from S3"
    );

    let lake_only_scan = lake_table.new_scan().with_lake_only(true);
    let lake_only_plan = lake_only_scan
        .plan()
        .await
        .expect("Failed to plan Paimon append lake-only read");
    assert_eq!(lake_only_plan.splits().len(), 1);
    let lake_only_reader = lake_only_scan.new_reader();
    let lake_only_stream = lake_only_reader
        .read_splits(lake_only_plan.splits())
        .await
        .expect("Failed to execute Paimon append lake-only read");
    let lake_only_batches = tokio::time::timeout(
        Duration::from_secs(20),
        lake_only_stream.try_collect::<Vec<_>>(),
    )
    .await
    .expect("Timed out waiting for Paimon append lake-only read")
    .expect("Failed to collect Paimon append lake-only read");
    assert_eq!(
        extract_ids(&lake_only_batches),
        vec![1, 2, 3],
        "lake-only must exclude every row after the readable snapshot seam"
    );

    let expired_file = find_table_parquet_file(table_name);
    std::fs::remove_file(&expired_file).unwrap_or_else(|error| {
        panic!(
            "Failed to simulate Paimon file expiration by deleting {}: {error}",
            expired_file.display()
        )
    });
    let expired_stream = union_reader
        .read_split(&transported_split)
        .await
        .expect("Opening an expired Paimon split stream must remain lazy");
    let expired_result = tokio::time::timeout(
        Duration::from_secs(20),
        expired_stream.try_collect::<Vec<_>>(),
    )
    .await
    .expect("Timed out waiting for expired Paimon split to fail");
    match expired_result {
        Err(FlussLakeError::DataUnavailable(message)) => {
            assert!(
                message.contains("Paimon") || message.contains("data"),
                "unexpected data-unavailable error: {message}"
            );
        }
        Err(other) => panic!("expected DataUnavailable after file expiration, got: {other}"),
        Ok(_) => panic!("an expired Paimon split must not report successful end-of-stream"),
    }

    drop(writer);
    admin
        .drop_table(&table_path, false)
        .await
        .expect("Failed to drop Paimon append UnionRead table");
}

/// A readable Paimon snapshot remains part of the table's current view after
/// a partition expires from Fluss. Such a partition is planned as a lake-only
/// logical `(partition, bucket)` split without inventing a live partition id.
#[cfg(feature = "paimon")]
#[tokio::test]
async fn paimon_partitioned_union_read_keeps_expired_lake_partition() {
    let cluster = support::get_shared_cluster();
    let connection = Arc::new(cluster.get_fluss_connection().await);
    let admin = connection.get_admin().expect("Failed to get Fluss admin");
    let table_path = TablePath::new("fluss", "test_paimon_expired_partition_union_read");
    let table_descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .column("region", DataTypes::string())
                .build()
                .expect("Failed to build partitioned UnionRead schema"),
        )
        .partitioned_by(vec!["region"])
        .distributed_by(Some(1), vec!["id".to_string()])
        .property("table.datalake.enabled", "true")
        .property("table.datalake.format", "paimon")
        .property("table.datalake.freshness", "1s")
        .custom_property("paimon.file.format", "parquet")
        .build()
        .expect("Failed to build partitioned UnionRead table descriptor");
    support::create_table(&admin, &table_path, &table_descriptor).await;

    let us_partition_id = create_ready_partition(&admin, &table_path, "US").await;
    let eu_partition_id = create_ready_partition(&admin, &table_path, "EU").await;
    let table = connection
        .get_table(&table_path)
        .await
        .expect("Failed to open partitioned UnionRead table");
    let writer = table
        .new_append()
        .expect("Failed to create partitioned append operation")
        .create_writer()
        .expect("Failed to create partitioned append writer");
    writer
        .append_arrow_batch(partitioned_append_batch(
            vec![1, 2],
            vec!["us-1", "us-2"],
            "US",
        ))
        .expect("Failed to append US lake baseline");
    writer
        .append_arrow_batch(partitioned_append_batch(
            vec![3, 4],
            vec!["eu-3", "eu-4"],
            "EU",
        ))
        .expect("Failed to append EU lake baseline");
    writer
        .flush()
        .await
        .expect("Failed to flush partitioned lake baseline");

    let table_info = admin
        .get_table_info(&table_path)
        .await
        .expect("Failed to resolve partitioned table metadata");
    for (region, partition_id) in [("US", us_partition_id), ("EU", eu_partition_id)] {
        let seam_offset = admin
            .list_partition_offsets(
                &table_path,
                region,
                &[0],
                fluss::rpc::message::OffsetSpec::Latest,
            )
            .await
            .expect("Failed to resolve partition seam offset")[&0];
        support::run_java_paimon_tiering_until_offset(
            cluster.plaintext_bootstrap_servers(),
            &table_path,
            table_info.table_id,
            Some(partition_id),
            seam_offset,
        )
        .await;
    }

    let eu_spec = PartitionSpec::new(HashMap::from([("region", "EU")]));
    admin
        .drop_partition(&table_path, &eu_spec, false)
        .await
        .expect("Failed to expire EU partition from Fluss");
    writer
        .append_arrow_batch(partitioned_append_batch(vec![5], vec!["us-tail-5"], "US"))
        .expect("Failed to append the live US log tail");
    writer
        .flush()
        .await
        .expect("Failed to flush the live US log tail");

    let lake_table = FlussLakeTable::open(connection.clone(), &table_path)
        .await
        .expect("Failed to open partitioned lake table");
    let scan = lake_table.new_scan();
    let plan = scan
        .plan()
        .await
        .expect("Failed to plan partitioned UnionRead");
    assert_eq!(
        plan.splits().len(),
        2,
        "the live US partition and expired EU lake partition should each produce one bucket split"
    );
    let partitions: Vec<_> = plan
        .splits()
        .iter()
        .map(|split| split.partition.clone())
        .collect();
    assert!(
        partitions.contains(&FlussLakePartitionIdentity::KeyValues(vec![(
            "region".to_string(),
            "US".to_string(),
        )]))
    );
    assert!(
        partitions.contains(&FlussLakePartitionIdentity::KeyValues(vec![(
            "region".to_string(),
            "EU".to_string(),
        )]))
    );

    let stream = scan
        .new_reader()
        .read_splits(plan.splits())
        .await
        .expect("Failed to execute partitioned UnionRead");
    let batches = tokio::time::timeout(Duration::from_secs(20), stream.try_collect::<Vec<_>>())
        .await
        .expect("Timed out waiting for partitioned UnionRead")
        .expect("Failed to collect partitioned UnionRead");
    assert_eq!(extract_ids(&batches), vec![1, 2, 3, 4, 5]);

    let expired_scan = lake_table.new_scan().with_filter(col("region").eq("EU"));
    let expired_plan = expired_scan
        .plan()
        .await
        .expect("Failed to plan expired-partition predicate");
    assert_eq!(expired_plan.splits().len(), 1);
    assert_eq!(
        expired_plan.splits()[0].partition,
        FlussLakePartitionIdentity::KeyValues(vec![("region".to_string(), "EU".to_string(),)])
    );
    let expired_stream = expired_scan
        .new_reader()
        .read_splits(expired_plan.splits())
        .await
        .expect("Failed to execute expired-partition split");
    let expired_batches = tokio::time::timeout(
        Duration::from_secs(20),
        expired_stream.try_collect::<Vec<_>>(),
    )
    .await
    .expect("Timed out waiting for expired-partition UnionRead")
    .expect("Failed to collect expired-partition UnionRead");
    assert_eq!(extract_ids(&expired_batches), vec![3, 4]);

    let pruned_plan = lake_table
        .new_scan()
        .with_filter(col("region").eq("APAC"))
        .plan()
        .await
        .expect("Failed to plan fully pruned partition predicate");
    assert_eq!(pruned_plan.split_count(), 0);

    drop(writer);
    admin
        .drop_table(&table_path, false)
        .await
        .expect("Failed to drop partitioned UnionRead table");
}

#[tokio::test]
async fn append_log_plan_uses_frozen_stop_offset_after_transport() {
    let cluster = support::get_shared_cluster();
    let connection = Arc::new(cluster.get_fluss_connection().await);
    let admin = connection.get_admin().expect("Failed to get Fluss admin");
    let table_path = TablePath::new("fluss", "test_union_read_frozen_append_log");
    let table_descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .build()
                .expect("Failed to build UnionRead integration test schema"),
        )
        .property("table.datalake.enabled", "true")
        .property("table.datalake.format", "paimon")
        .build()
        .expect("Failed to build UnionRead integration test table descriptor");
    support::create_table(&admin, &table_path, &table_descriptor).await;

    let table = connection
        .get_table(&table_path)
        .await
        .expect("Failed to open UnionRead integration test table");
    let writer = table
        .new_append()
        .expect("Failed to create append operation")
        .create_writer()
        .expect("Failed to create append writer");
    writer
        .append_arrow_batch(append_batch(
            vec![1, 2, 3],
            vec!["before-1", "before-2", "before-3"],
        ))
        .expect("Failed to append pre-plan batch");
    writer
        .flush()
        .await
        .expect("Failed to flush pre-plan batch");

    let lake_table = FlussLakeTable::open(connection.clone(), &table_path)
        .await
        .expect("Failed to open append-log lake table");
    let scan = lake_table
        .new_scan()
        .with_projection(vec![1])
        .with_filter(col("id").eq(2_i32));
    let plan = scan
        .plan()
        .await
        .expect("Failed to plan append-log UnionRead");
    assert_eq!(plan.schema().fields().len(), 1);
    assert_eq!(plan.schema().field(0).name(), "name");
    assert_eq!(plan.splits().len(), 1);
    let transported_split: FlussLakeReadSplit = serde_json::from_slice(
        &serde_json::to_vec(&plan.splits()[0]).expect("Failed to encode UnionRead split"),
    )
    .expect("Failed to decode transported UnionRead split");

    writer
        .append_arrow_batch(append_batch(vec![4, 5], vec!["after-4", "after-5"]))
        .expect("Failed to append post-plan batch");
    writer
        .flush()
        .await
        .expect("Failed to flush post-plan batch");

    let read = scan.new_reader();
    let stream = read
        .read_split(&transported_split)
        .await
        .expect("Failed to execute append-log UnionRead split");
    let batches = tokio::time::timeout(Duration::from_secs(10), stream.try_collect::<Vec<_>>())
        .await
        .expect("Timed out waiting for bounded UnionRead stream to finish")
        .expect("Failed to collect bounded UnionRead output");

    assert!(!batches.is_empty());
    assert!(batches.iter().all(|batch| batch.num_columns() == 1));
    let names: Vec<String> = batches
        .iter()
        .flat_map(|batch| {
            let names = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("Projected name column should be a StringArray");
            (0..names.len()).map(|row| names.value(row).to_string())
        })
        .collect();
    assert_eq!(names, vec!["before-2"]);
    assert!(batches.iter().all(|batch| {
        batch
            .columns()
            .iter()
            .all(|column| !column.as_any().is::<Int32Array>())
    }));

    drop(writer);
    admin
        .drop_table(&table_path, false)
        .await
        .expect("Failed to drop UnionRead integration test table");
}

#[tokio::test]
async fn stale_schema_split_is_rejected_after_alter_table() {
    let cluster = support::get_shared_cluster();
    let connection = Arc::new(cluster.get_fluss_connection().await);
    let admin = connection.get_admin().expect("Failed to get Fluss admin");
    let table_path = TablePath::new("fluss", "test_union_read_stale_schema");
    let table_descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .build()
                .expect("Failed to build UnionRead integration test schema"),
        )
        .property("table.datalake.enabled", "true")
        .property("table.datalake.format", "paimon")
        .build()
        .expect("Failed to build UnionRead integration test table descriptor");
    support::create_table(&admin, &table_path, &table_descriptor).await;

    let table = connection
        .get_table(&table_path)
        .await
        .expect("Failed to open UnionRead integration test table");
    let writer = table
        .new_append()
        .expect("Failed to create append operation")
        .create_writer()
        .expect("Failed to create append writer");
    writer
        .append_arrow_batch(append_batch(vec![1, 2], vec!["a", "b"]))
        .expect("Failed to append pre-plan batch");
    writer
        .flush()
        .await
        .expect("Failed to flush pre-plan batch");

    let lake_table = FlussLakeTable::open(connection.clone(), &table_path)
        .await
        .expect("Failed to open stale-schema lake table");
    let scan = lake_table.new_scan();
    let plan = scan
        .plan()
        .await
        .expect("Failed to plan append-log UnionRead");
    assert_eq!(plan.splits().len(), 1);
    let stale_split = plan.splits()[0].clone();

    let age_type_json = serde_json::to_vec(
        &DataTypes::int()
            .serialize_json()
            .expect("Failed to serialize INT type"),
    )
    .expect("Failed to encode INT type json");
    admin
        .alter_table(
            &table_path,
            false,
            AlterTableChanges {
                add_columns: vec![AddColumn {
                    column_name: "age".to_string(),
                    data_type_json: age_type_json,
                    comment: None,
                    position: ColumnPositionType::Last,
                }],
                ..Default::default()
            },
        )
        .await
        .expect("Failed to alter UnionRead integration test table");

    // `read_split` is asynchronous and lazy: schema drift is an environment
    // failure, so it surfaces as the first item of the returned stream.
    let read = scan.new_reader();
    let stream = read
        .read_split(&stale_split)
        .await
        .expect("Opening a stale-schema split stream must not fail structurally");
    let result = tokio::time::timeout(Duration::from_secs(10), stream.try_collect::<Vec<_>>())
        .await
        .expect("Timed out waiting for the stale-schema split to fail");
    match result {
        Err(FlussLakeError::SchemaIncompatible(message)) => {
            assert!(
                message.contains("schema id"),
                "unexpected schema error: {message}"
            );
        }
        Err(other) => panic!("expected a schema-incompatible error, got: {other}"),
        Ok(_) => panic!("stale-schema split must not execute after alter table"),
    }

    drop(writer);
    admin
        .drop_table(&table_path, false)
        .await
        .expect("Failed to drop UnionRead integration test table");
}
