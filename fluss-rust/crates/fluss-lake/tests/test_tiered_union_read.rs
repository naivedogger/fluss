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

//! Invoked by RustUnionReadITCase after real Java/Flink tiering has stopped.
//! Precompile this test; do not build Rust while the Java cluster is running.
#![cfg(feature = "paimon")]

use arrow::array::{Int32Array, StringArray};
use arrow::record_batch::RecordBatch;
use fluss::client::FlussConnection;
use fluss::config::Config;
use fluss::metadata::TablePath;
use fluss::predicate::col;
use fluss_lake::{
    FlussLakeReadContext, FlussLakeReadSplit, FlussLakeTable, LakePlannerContext,
    LakeReaderContext, LakeSource, LakeSplit, PaimonLakeSource, RecordBatchStream,
};
use futures::TryStreamExt;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("Java fixture must set {name}"))
}

// Exercises the public extension point, not private Paimon split decoding.
struct DelegatingLakeSource {
    inner: PaimonLakeSource,
    plans: AtomicUsize,
    reads: AtomicUsize,
}

impl LakeSource for DelegatingLakeSource {
    fn format(&self) -> &str {
        self.inner.format()
    }
    fn plan<'a>(
        &'a self,
        context: LakePlannerContext<'a>,
    ) -> BoxFuture<'a, fluss_lake::Result<Vec<LakeSplit>>> {
        self.plans.fetch_add(1, Ordering::Relaxed);
        self.inner.plan(context)
    }
    fn read<'a>(
        &'a self,
        context: LakeReaderContext<'a>,
    ) -> BoxFuture<'a, fluss_lake::Result<RecordBatchStream>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read(context)
    }
}

fn rows(batches: &[RecordBatch]) -> Vec<(i32, String)> {
    let mut rows = Vec::new();
    for batch in batches {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let names = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for index in 0..batch.num_rows() {
            rows.push((ids.value(index), names.value(index).to_owned()));
        }
    }
    rows.sort_unstable();
    rows
}

#[tokio::test]
#[ignore = "requires the live Java-owned tiering fixture; run RustUnionReadITCase"]
async fn verify_tiered_union_read() {
    tokio::time::timeout(Duration::from_secs(90), verify())
        .await
        .expect("UnionRead timed out");
}

async fn verify() {
    let scenario = required_env("FLUSS_RUST_UNION_READ_SCENARIO");
    assert!(matches!(
        scenario.as_str(),
        "append" | "pk" | "append-grow" | "append-shrink" | "pk-grow" | "pk-shrink"
    ));
    let connection = Arc::new(
        FlussConnection::new(Config {
            bootstrap_servers: required_env("FLUSS_RUST_UNION_READ_BOOTSTRAP_SERVERS"),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let path = TablePath::new(
        required_env("FLUSS_RUST_UNION_READ_DATABASE"),
        required_env("FLUSS_RUST_UNION_READ_TABLE"),
    );
    // Only storage configuration crosses the process boundary. Rust must discover
    // the snapshot, seam, stop offsets and physical lake files through public APIs.
    let properties = HashMap::from([(
        "table.datalake.paimon.warehouse".to_owned(),
        required_env("FLUSS_RUST_UNION_READ_WAREHOUSE"),
    )]);
    let info = connection
        .get_admin()
        .unwrap()
        .get_table_info(&path)
        .await
        .unwrap();
    let custom_source = Arc::new(DelegatingLakeSource {
        inner: PaimonLakeSource::new(&info, &properties).unwrap(),
        plans: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
    });
    let table = FlussLakeTable::open_with_properties(connection, &path, properties)
        .await
        .unwrap();
    if scenario.contains('-') {
        verify_partition_layouts(&table, &scenario).await;
        return;
    }
    let scan = table.new_scan().with_batch_size(1);
    let plan = scan.plan().await.unwrap();
    let context = FlussLakeReadContext::from_json(&plan.read_context().to_json().unwrap()).unwrap();
    assert!(
        context.lake_snapshot_id().is_some(),
        "must read a real lake snapshot"
    );
    assert_eq!(context.log_ranges().len(), 1);
    let range = &context.log_ranges()[0];
    assert!(range.start_offset() > 0, "lake baseline must not be empty");
    assert!(
        range.stop_offset() > range.start_offset(),
        "log tail must not be empty"
    );
    range.validate_available().unwrap();
    assert_eq!(plan.splits().len(), 1);
    let split: FlussLakeReadSplit =
        serde_json::from_slice(&serde_json::to_vec(&plan.splits()[0]).unwrap()).unwrap();
    let expected = if scenario == "pk" {
        vec![(1, "tail-new"), (3, "lake-keep"), (4, "tail-insert")]
    } else {
        vec![
            (1, "lake-old"),
            (2, "lake-delete"),
            (3, "lake-keep"),
            (4, "tail-4"),
            (5, "tail-5"),
        ]
    }
    .into_iter()
    .map(|(id, name)| (id, name.to_owned()))
    .collect::<Vec<_>>();
    let reader = plan.new_reader();
    let mut foreign = split.clone();
    foreign.split_id.push_str("-foreign");
    assert!(reader.read_split(&foreign).await.is_err());
    assert!(
        reader
            .read_splits(&[split.clone(), split.clone()])
            .await
            .is_err()
    );
    assert!(
        reader
            .read_splits_with_concurrency(plan.splits(), 0)
            .await
            .is_err()
    );
    for _ in 0..2 {
        let batches = reader
            .read_split(&split)
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            rows(&batches),
            expected,
            "transport/retry must preserve the current view"
        );
        assert!(batches.iter().all(|batch| batch.num_rows() <= 1));
    }
    let lake_scan = table.new_scan().with_lake_only(true);
    let lake_plan = lake_scan.plan_with_context(&context).await.unwrap();
    let baseline = lake_plan
        .new_reader()
        .read_splits(lake_plan.splits())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(
        rows(&baseline),
        vec![
            (1, "lake-old".into()),
            (2, "lake-delete".into()),
            (3, "lake-keep".into())
        ]
    );
    // Count scans must preserve rows even though the output has no columns.
    for (lake_only, count) in [(false, expected.len()), (true, 3)] {
        let count_plan = table
            .new_scan()
            .with_lake_only(lake_only)
            .with_projection(vec![])
            .with_batch_size(2)
            .plan_with_context(&context)
            .await
            .unwrap();
        let batches = count_plan
            .new_reader()
            .read_splits(count_plan.splits())
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert!(
            batches
                .iter()
                .all(|b| b.num_columns() == 0 && b.num_rows() <= 2)
        );
        assert_eq!(
            batches.iter().map(RecordBatch::num_rows).sum::<usize>(),
            count
        );
    }
    let custom_plan = table
        .new_scan()
        .with_lake_source(custom_source.clone())
        .plan_with_context(&context)
        .await
        .unwrap();
    let batches = custom_plan
        .new_reader()
        .read_splits(custom_plan.splits())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(rows(&batches), expected);
    assert_eq!(custom_source.plans.load(Ordering::Relaxed), 1);
    assert_eq!(custom_source.reads.load(Ordering::Relaxed), 1);
    if scenario == "pk" {
        // Filtering before reconciliation would resurrect old/removed baseline rows.
        for (value, count) in [("lake-old", 0), ("lake-delete", 0), ("tail-new", 1)] {
            let filtered = table
                .new_scan()
                .with_filter(col("name").eq(value))
                .with_projection(vec![1]);
            let plan = filtered.plan_with_context(&context).await.unwrap();
            let batches = plan
                .new_reader()
                .read_splits(plan.splits())
                .await
                .unwrap()
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert_eq!(
                batches.iter().map(RecordBatch::num_rows).sum::<usize>(),
                count
            );
            assert!(batches.iter().all(|batch| batch.num_columns() == 1));
        }
    }
}

fn partition_rows(batches: &[RecordBatch]) -> Vec<(i32, String, String)> {
    let mut result = Vec::new();
    for batch in batches {
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let names = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let regions = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for i in 0..batch.num_rows() {
            result.push((ids.value(i), names.value(i).into(), regions.value(i).into()));
        }
    }
    result.sort_unstable();
    result
}

async fn verify_partition_layouts(table: &FlussLakeTable, scenario: &str) {
    let (old_count, new_count) = if scenario.ends_with("grow") {
        (2, 4)
    } else {
        (4, 2)
    };
    let primary_key = scenario.starts_with("pk");
    let plan = table.new_scan().plan().await.unwrap();
    let context = FlussLakeReadContext::from_json(&plan.read_context().to_json().unwrap()).unwrap();
    assert_eq!(context.num_buckets(), new_count);
    assert_eq!(context.log_ranges().len(), (old_count + new_count) as usize);
    assert!(context.lake_snapshot_id().is_some());
    let mut has_tail = false;
    for range in context.log_ranges() {
        let fluss_lake::FlussLakePartitionIdentity::KeyValues(values) = range.partition_identity()
        else {
            panic!("partitioned fixture")
        };
        let name = &values[0].1;
        assert!(name == "old" || name == "new");
        assert_eq!(
            range.bucket_count(),
            if name == "old" { old_count } else { new_count }
        );
        has_tail |= range.stop_offset() > range.start_offset();
        range.validate_available().unwrap();
    }
    assert!(has_tail);
    let mut baseline = Vec::new();
    let mut expected = Vec::new();
    for partition in ["old", "expired", "new"] {
        for id in 0..16 {
            baseline.push((id, format!("lake-{id}"), partition.to_string()));
            if primary_key && partition != "expired" && id == 1 {
                continue;
            }
            let value = if primary_key && partition != "expired" && id == 0 {
                "tail-update".into()
            } else {
                format!("lake-{id}")
            };
            expected.push((id, value, partition.to_string()));
        }
        if partition != "expired" {
            if primary_key {
                expected.push((16, "tail-insert".into(), partition.into()));
            } else {
                for id in [16, 17] {
                    expected.push((id, format!("tail-{id}"), partition.into()));
                }
            }
        }
    }
    baseline.sort_unstable();
    expected.sort_unstable();
    let transported: Vec<FlussLakeReadSplit> =
        serde_json::from_slice(&serde_json::to_vec(plan.splits()).unwrap()).unwrap();
    let read = |plan: fluss_lake::FlussLakeReadPlan| async move {
        let batches = plan
            .new_reader()
            .read_splits(plan.splits())
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        partition_rows(&batches)
    };
    let batches = plan
        .new_reader()
        .read_splits(&transported)
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(partition_rows(&batches), expected);
    assert_eq!(
        read(
            table
                .new_scan()
                .with_lake_only(true)
                .plan_with_context(&context)
                .await
                .unwrap()
        )
        .await,
        baseline
    );
    // Exercise hash pruning on all three layouts, including the lake-only partition.
    for id in [0, 1, 7, 13, 16, 17] {
        let filtered = table
            .new_scan()
            .with_filter(col("id").eq(id))
            .plan_with_context(&context)
            .await
            .unwrap();
        let wanted = expected
            .iter()
            .filter(|row| row.0 == id)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(read(filtered).await, wanted, "bucket pruning id={id}");
    }
    for partition in ["old", "expired", "new"] {
        let filtered = table
            .new_scan()
            .with_filter(col("region").eq(partition))
            .plan_with_context(&context)
            .await
            .unwrap();
        let wanted = expected
            .iter()
            .filter(|row| row.2 == partition)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            read(filtered).await,
            wanted,
            "partition pruning {partition}"
        );
    }
    if primary_key {
        let filtered = table
            .new_scan()
            .with_filter(col("name").eq("lake-0"))
            .plan_with_context(&context)
            .await
            .unwrap();
        assert_eq!(
            read(filtered).await,
            vec![(0, "lake-0".into(), "expired".into())]
        );
    }
}
