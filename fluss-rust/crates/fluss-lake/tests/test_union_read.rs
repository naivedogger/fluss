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
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(feature = "integration_tests")]

mod support;

use arrow::array::{Array, ArrayRef, Int32Array, Int64Array, StringArray};
use arrow::record_batch::RecordBatch;
use fluss::metadata::{
    AddColumn, AlterTableChanges, ColumnPositionType, DataTypes, JsonSerde, Schema,
    TableDescriptor, TablePath,
};
use fluss::predicate::{ComparisonOperator, FieldRef, PruningPredicate};
use fluss_lake::{
    FlussUnionReadExecutor, FlussUnionReadPlanner, PredicateId, PredicateInput,
    PredicatePushdownLevel, UnionReadError, UnionReadExecutionContext, UnionReadExecutor,
    UnionReadPlanner, UnionReadRequest, UnionReadTask,
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

#[tokio::test]
async fn union_read_test_cluster_accepts_connections() {
    support::get_shared_cluster().get_fluss_connection().await;
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

    let plan = FlussUnionReadPlanner::new(connection.clone())
        .plan(UnionReadRequest::new(table_path.clone()).with_output_projection(vec![1]))
        .await
        .expect("Failed to plan append-log UnionRead");
    assert_eq!(plan.output_schema().fields().len(), 1);
    assert_eq!(plan.output_schema().field(0).name(), "name");
    assert_eq!(plan.tasks().len(), 1);
    let transported_task = UnionReadTask::decode(
        &plan.tasks()[0]
            .encode()
            .expect("Failed to encode UnionRead task"),
    )
    .expect("Failed to decode transported UnionRead task");

    writer
        .append_arrow_batch(append_batch(vec![4, 5], vec!["after-4", "after-5"]))
        .expect("Failed to append post-plan batch");
    writer
        .flush()
        .await
        .expect("Failed to flush post-plan batch");

    let stream = FlussUnionReadExecutor
        .execute(
            transported_task,
            UnionReadExecutionContext::default().with_fluss_connection(connection.clone()),
        )
        .expect("Failed to execute append-log UnionRead task");
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
    assert_eq!(names, vec!["before-1", "before-2", "before-3"]);
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

    let request =
        UnionReadRequest::new(table_path.clone()).with_predicates(vec![PredicateInput::new(
            PredicateId::new(7),
            PruningPredicate::comparison(
                ComparisonOperator::Equal,
                FieldRef::new(1, "region", DataTypes::string()),
                "US",
            ),
        )]);
    let plan = FlussUnionReadPlanner::new(connection.clone())
        .plan(request)
        .await
        .expect("Failed to plan partitioned UnionRead");

    let decisions = plan.predicate_pushdown_decisions();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].predicate_id(), PredicateId::new(7));
    assert_eq!(decisions[0].level(), PredicatePushdownLevel::PruningOnly);
    assert!(decisions[0].level().requires_residual_evaluation());
    // The EU partition is pruned, so only the US partition's bucket remains.
    assert_eq!(plan.tasks().len(), 1);

    let mut ids = Vec::new();
    for task in plan.tasks() {
        let transported_task =
            UnionReadTask::decode(&task.encode().expect("Failed to encode UnionRead task"))
                .expect("Failed to decode transported UnionRead task");
        let stream = FlussUnionReadExecutor
            .execute(
                transported_task,
                UnionReadExecutionContext::default().with_fluss_connection(connection.clone()),
            )
            .expect("Failed to execute partitioned UnionRead task");
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
async fn stale_schema_task_is_rejected_after_alter_table() {
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

    let plan = FlussUnionReadPlanner::new(connection.clone())
        .plan(UnionReadRequest::new(table_path.clone()))
        .await
        .expect("Failed to plan append-log UnionRead");
    assert_eq!(plan.tasks().len(), 1);
    let stale_task = plan.tasks()[0].clone();

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

    // `execute` is synchronous and lazy: schema drift is an environment
    // failure, so it surfaces as the first item of the returned stream.
    let stream = FlussUnionReadExecutor
        .execute(
            stale_task,
            UnionReadExecutionContext::default().with_fluss_connection(connection.clone()),
        )
        .expect("Opening a stale-schema task stream must not fail structurally");
    let result = tokio::time::timeout(Duration::from_secs(10), stream.try_collect::<Vec<_>>())
        .await
        .expect("Timed out waiting for the stale-schema task to fail");
    match result {
        Err(UnionReadError::Execution(message)) => {
            assert!(
                message.contains("schema id"),
                "unexpected execution error: {message}"
            );
        }
        Err(other) => panic!("expected an execution error, got: {other}"),
        Ok(_) => panic!("stale-schema task must not execute after alter table"),
    }

    drop(writer);
    admin
        .drop_table(&table_path, false)
        .await
        .expect("Failed to drop UnionRead integration test table");
}
