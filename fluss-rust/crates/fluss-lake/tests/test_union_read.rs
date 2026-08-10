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

use arrow::array::{Array, ArrayRef, Int32Array, Int64Array, StringArray};
use arrow::record_batch::RecordBatch;
use fluss::metadata::{
    AddColumn, AlterTableChanges, ColumnPositionType, DataTypes, JsonSerde, Schema,
    TableDescriptor, TablePath,
};
#[cfg(feature = "paimon")]
use fluss::row::GenericRow;
use fluss_lake::{
    FlussLakeError, FlussLakeExecutionContext, FlussLakePredicate, FlussLakeReadSplit,
    FlussLakeTable,
};
use futures::TryStreamExt;
use std::sync::Arc;
use std::time::Duration;

fn append_batch(ids: Vec<i32>, names: Vec<&str>) -> RecordBatch {
    RecordBatch::try_from_iter(vec![
        ("id", Arc::new(Int32Array::from(ids)) as ArrayRef),
        ("name", Arc::new(StringArray::from(names)) as ArrayRef),
    ])
    .expect("Failed to build append record batch")
}

fn partitioned_batch(ids: Vec<i32>, regions: Vec<&str>, values: Vec<i64>) -> RecordBatch {
    RecordBatch::try_from_iter(vec![
        ("id", Arc::new(Int32Array::from(ids)) as ArrayRef),
        ("region", Arc::new(StringArray::from(regions)) as ArrayRef),
        ("value", Arc::new(Int64Array::from(values)) as ArrayRef),
    ])
    .expect("Failed to build partitioned record batch")
}

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
fn partitioned_pk_row<'a>(region: &'a str, id: i32, name: Option<&'a str>) -> GenericRow<'a> {
    let mut row = GenericRow::new(3);
    row.set_field(0, region);
    row.set_field(1, id);
    if let Some(name) = name {
        row.set_field(2, name);
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
fn extract_partitioned_pk_rows(batches: &[RecordBatch]) -> Vec<(String, i32, String)> {
    let mut rows = Vec::new();
    for batch in batches {
        let regions = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("region column should be a StringArray");
        let ids = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("id column should be an Int32Array");
        let names = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("name column should be a StringArray");
        for row in 0..batch.num_rows() {
            rows.push((
                regions.value(row).to_string(),
                ids.value(row),
                names.value(row).to_string(),
            ));
        }
    }
    rows.sort_unstable();
    rows
}

#[tokio::test]
async fn union_read_test_cluster_accepts_connections() {
    support::get_shared_cluster().get_fluss_connection().await;
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
    let transported_split = FlussLakeReadSplit::decode(
        &plan.splits()[0]
            .encode()
            .expect("Failed to encode PK split"),
    )
    .expect("Failed to decode transported PK split");
    let read = scan
        .new_reader(FlussLakeExecutionContext::default().with_fluss_connection(connection.clone()))
        .expect("Failed to create real Paimon PK UnionRead reader");

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

/// Real partitioned PK UnionRead across a Java-tiered Paimon snapshot and a
/// bounded Fluss changelog tail. The partition predicate must prune the other
/// physical partition while retaining the hidden primary-key columns required
/// by the overlay.
#[cfg(feature = "paimon")]
#[tokio::test]
async fn partitioned_paimon_pk_union_read_executes_matching_partition_end_to_end() {
    let cluster = support::get_shared_cluster();
    let connection = Arc::new(cluster.get_fluss_connection().await);
    let admin = connection.get_admin().expect("Failed to get Fluss admin");
    let table_path = TablePath::new("fluss", "test_partitioned_paimon_pk_union_read_e2e");
    let table_descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("region", DataTypes::string())
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .primary_key(vec!["region", "id"])
                .build()
                .expect("Failed to build partitioned Paimon PK schema"),
        )
        .partitioned_by(vec!["region"])
        .distributed_by(Some(1), vec!["id".to_string()])
        .property("table.datalake.enabled", "true")
        .property("table.datalake.format", "paimon")
        .property("table.datalake.freshness", "1s")
        .custom_property("paimon.file.format", "parquet")
        .build()
        .expect("Failed to build partitioned Paimon PK table descriptor");
    support::create_partitioned_table(
        &admin,
        &table_path,
        &table_descriptor,
        "region",
        &["US", "EU"],
    )
    .await;

    let partition_infos = admin
        .list_partition_infos(&table_path)
        .await
        .expect("Failed to list partitioned Paimon PK table partitions");
    let us_partition_id = partition_infos
        .iter()
        .find(|partition| partition.get_partition_name() == "US")
        .expect("US partition should exist")
        .get_partition_id();

    let table = connection
        .get_table(&table_path)
        .await
        .expect("Failed to open partitioned Paimon PK table");
    let writer = table
        .new_upsert()
        .expect("Failed to create partitioned Paimon PK upsert operation")
        .create_writer()
        .expect("Failed to create partitioned Paimon PK writer");
    for row in [
        partitioned_pk_row("US", 1, Some("lake-old")),
        partitioned_pk_row("US", 2, Some("lake-delete")),
        partitioned_pk_row("US", 3, Some("lake-keep")),
        partitioned_pk_row("EU", 10, Some("eu-lake")),
    ] {
        writer
            .upsert(&row)
            .expect("Failed to queue partitioned baseline upsert")
            .await
            .expect("Failed to acknowledge partitioned baseline upsert");
    }
    writer
        .flush()
        .await
        .expect("Failed to flush partitioned Paimon PK baseline");

    let table_info = admin
        .get_table_info(&table_path)
        .await
        .expect("Failed to resolve partitioned Paimon PK table metadata");
    let us_seam_offset = admin
        .list_partition_offsets(
            &table_path,
            "US",
            &[0],
            fluss::rpc::message::OffsetSpec::Latest,
        )
        .await
        .expect("Failed to resolve US Paimon PK seam offset")[&0];
    assert!(us_seam_offset > 0);

    support::run_java_paimon_tiering_until_partition_offset(
        cluster.plaintext_bootstrap_servers(),
        &table_path,
        table_info.table_id,
        Some(us_partition_id),
        us_seam_offset,
    )
    .await;
    let readable_snapshot = admin
        .get_readable_lake_snapshot(&table_path)
        .await
        .expect("Failed to load partitioned readable Paimon snapshot");
    let us_bucket_snapshot = readable_snapshot
        .bucket_snapshots
        .iter()
        .find(|snapshot| snapshot.partition_id == Some(us_partition_id) && snapshot.bucket_id == 0)
        .expect("Readable Paimon snapshot should contain the US partition bucket");
    assert_eq!(us_bucket_snapshot.log_offset, Some(us_seam_offset));

    for row in [
        partitioned_pk_row("US", 1, Some("tail-new")),
        partitioned_pk_row("US", 4, Some("tail-insert")),
        partitioned_pk_row("EU", 10, Some("eu-tail")),
    ] {
        writer
            .upsert(&row)
            .expect("Failed to queue partitioned tail upsert")
            .await
            .expect("Failed to acknowledge partitioned tail upsert");
    }
    writer
        .delete(&partitioned_pk_row("US", 2, None))
        .expect("Failed to queue partitioned tail delete")
        .await
        .expect("Failed to acknowledge partitioned tail delete");
    writer
        .flush()
        .await
        .expect("Failed to flush partitioned Paimon PK tail");

    let lake_table = FlussLakeTable::open(connection.clone(), &table_path)
        .await
        .expect("Failed to open partitioned Paimon PK lake table");
    let scan = lake_table
        .new_scan()
        .with_filter(FlussLakePredicate::eq("region", "US"));
    let plan = scan
        .plan()
        .await
        .expect("Failed to plan partitioned Paimon PK UnionRead");
    assert_eq!(
        plan.splits().len(),
        1,
        "partition pruning must leave one logical US partition/bucket split"
    );
    let transported_split = FlussLakeReadSplit::decode(
        &plan.splits()[0]
            .encode()
            .expect("Failed to encode partitioned Paimon PK split"),
    )
    .expect("Failed to decode transported partitioned Paimon PK split");
    let read = scan
        .new_reader(FlussLakeExecutionContext::default().with_fluss_connection(connection.clone()))
        .expect("Failed to create partitioned Paimon PK reader");

    let stream = read
        .read_splits(std::slice::from_ref(&transported_split))
        .await
        .expect("Failed to execute partitioned Paimon PK splits");
    let first = tokio::time::timeout(Duration::from_secs(20), stream.try_collect::<Vec<_>>())
        .await
        .expect("Timed out waiting for partitioned Paimon PK UnionRead")
        .expect("Failed to collect partitioned Paimon PK UnionRead");
    let retry_stream = read
        .read_split(&transported_split)
        .await
        .expect("Failed to retry partitioned Paimon PK split");
    let retried = tokio::time::timeout(
        Duration::from_secs(20),
        retry_stream.try_collect::<Vec<_>>(),
    )
    .await
    .expect("Timed out waiting for retried partitioned Paimon PK UnionRead")
    .expect("Failed to collect retried partitioned Paimon PK UnionRead");

    let expected = vec![
        ("US".to_string(), 1, "tail-new".to_string()),
        ("US".to_string(), 3, "lake-keep".to_string()),
        ("US".to_string(), 4, "tail-insert".to_string()),
    ];
    assert_eq!(extract_partitioned_pk_rows(&first), expected);
    assert_eq!(
        extract_partitioned_pk_rows(&retried),
        expected,
        "retrying a transported partitioned split must reproduce the same logical rows"
    );

    drop(writer);
    admin
        .drop_table(&table_path, false)
        .await
        .expect("Failed to drop partitioned Paimon PK UnionRead table");
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
        .with_filter(FlussLakePredicate::eq("id", 2_i32));
    let plan = scan
        .plan()
        .await
        .expect("Failed to plan append-log UnionRead");
    assert_eq!(plan.output_schema().fields().len(), 1);
    assert_eq!(plan.output_schema().field(0).name(), "name");
    assert_eq!(plan.splits().len(), 1);
    let transported_split = FlussLakeReadSplit::decode(
        &plan.splits()[0]
            .encode()
            .expect("Failed to encode UnionRead split"),
    )
    .expect("Failed to decode transported UnionRead split");

    writer
        .append_arrow_batch(append_batch(vec![4, 5], vec!["after-4", "after-5"]))
        .expect("Failed to append post-plan batch");
    writer
        .flush()
        .await
        .expect("Failed to flush post-plan batch");

    let read = scan
        .new_reader(FlussLakeExecutionContext::default().with_fluss_connection(connection.clone()))
        .expect("Failed to create append-log UnionRead reader");
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
async fn partitioned_plan_prunes_partitions_and_executes_matching_buckets() {
    let cluster = support::get_shared_cluster();
    let connection = Arc::new(cluster.get_fluss_connection().await);
    let admin = connection.get_admin().expect("Failed to get Fluss admin");
    let table_path = TablePath::new("fluss", "test_union_read_partition_pruning");
    let table_descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("region", DataTypes::string())
                .column("value", DataTypes::bigint())
                .build()
                .expect("Failed to build UnionRead integration test schema"),
        )
        .partitioned_by(vec!["region"])
        .build()
        .expect("Failed to build UnionRead integration test table descriptor");
    support::create_partitioned_table(
        &admin,
        &table_path,
        &table_descriptor,
        "region",
        &["US", "EU"],
    )
    .await;

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
        .append_arrow_batch(partitioned_batch(
            vec![1, 2],
            vec!["US", "US"],
            vec![100, 200],
        ))
        .expect("Failed to append US batch");
    writer
        .append_arrow_batch(partitioned_batch(
            vec![3, 4],
            vec!["EU", "EU"],
            vec![300, 400],
        ))
        .expect("Failed to append EU batch");
    writer
        .flush()
        .await
        .expect("Failed to flush partitioned batches");

    let lake_table = FlussLakeTable::open(connection.clone(), &table_path)
        .await
        .expect("Failed to open partitioned lake table");
    let scan = lake_table
        .new_scan()
        .with_filter(FlussLakePredicate::eq("region", "US"));
    let plan = scan
        .plan()
        .await
        .expect("Failed to plan partitioned UnionRead");

    // The EU partition is pruned, so only the US partition's bucket remains.
    assert_eq!(plan.splits().len(), 1);

    let read = scan
        .new_reader(FlussLakeExecutionContext::default().with_fluss_connection(connection.clone()))
        .expect("Failed to create partitioned UnionRead reader");
    let mut ids = Vec::new();
    for split in plan.splits() {
        let transported_split =
            FlussLakeReadSplit::decode(&split.encode().expect("Failed to encode UnionRead split"))
                .expect("Failed to decode transported UnionRead split");
        let stream = read
            .read_split(&transported_split)
            .await
            .expect("Failed to execute partitioned UnionRead split");
        let batches = tokio::time::timeout(Duration::from_secs(10), stream.try_collect::<Vec<_>>())
            .await
            .expect("Timed out waiting for bounded UnionRead stream to finish")
            .expect("Failed to collect bounded UnionRead output");
        ids.extend(extract_ids(&batches));
    }
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);

    drop(writer);
    admin
        .drop_table(&table_path, false)
        .await
        .expect("Failed to drop UnionRead integration test table");
}

#[tokio::test]
async fn bucket_key_pruning_and_row_filter_execute_end_to_end() {
    let cluster = support::get_shared_cluster();
    let connection = Arc::new(cluster.get_fluss_connection().await);
    let admin = connection.get_admin().expect("Failed to get Fluss admin");
    let table_path = TablePath::new("fluss", "test_union_read_bucket_pruning");
    let table_descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("id", DataTypes::int())
                .column("name", DataTypes::string())
                .build()
                .expect("Failed to build UnionRead integration test schema"),
        )
        .distributed_by(Some(4), vec!["id".to_string()])
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
        .append_arrow_batch(append_batch(vec![1, 2, 3, 4], vec!["a", "b", "c", "d"]))
        .expect("Failed to append bucket-pruning batch");
    writer
        .flush()
        .await
        .expect("Failed to flush bucket-pruning batch");

    let lake_table = FlussLakeTable::open(connection.clone(), &table_path)
        .await
        .expect("Failed to open bucket-pruning lake table");
    let scan = lake_table
        .new_scan()
        .with_filter(FlussLakePredicate::eq("id", 1_i32));
    let plan = scan
        .plan()
        .await
        .expect("Failed to plan bucket-pruning UnionRead");

    // Equality on the bucket key must prune to exactly one bucket.
    assert_eq!(plan.splits().len(), 1);

    let read = scan
        .new_reader(FlussLakeExecutionContext::default().with_fluss_connection(connection.clone()))
        .expect("Failed to create bucket-pruning UnionRead reader");
    let mut ids = Vec::new();
    for split in plan.splits() {
        let transported_split =
            FlussLakeReadSplit::decode(&split.encode().expect("Failed to encode UnionRead split"))
                .expect("Failed to decode transported UnionRead split");
        let stream = read
            .read_split(&transported_split)
            .await
            .expect("Failed to execute bucket-pruning UnionRead split");
        let batches = tokio::time::timeout(Duration::from_secs(10), stream.try_collect::<Vec<_>>())
            .await
            .expect("Timed out waiting for bounded UnionRead stream to finish")
            .expect("Failed to collect bounded UnionRead output");
        ids.extend(extract_ids(&batches));
    }
    ids.sort_unstable();
    assert_eq!(ids, vec![1]);

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
    let read = scan
        .new_reader(FlussLakeExecutionContext::default().with_fluss_connection(connection.clone()))
        .expect("Failed to create stale-schema UnionRead reader");
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
