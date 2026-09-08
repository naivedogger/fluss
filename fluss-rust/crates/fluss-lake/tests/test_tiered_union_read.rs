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
use fluss_lake::{FlussLakeReadContext, FlussLakeReadSplit, FlussLakeTable};
use futures::TryStreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("Java fixture must set {name}"))
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
    assert_eq!(scenario, "append");
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
    let table = FlussLakeTable::open_with_properties(
        connection,
        &path,
        HashMap::from([(
            "table.datalake.paimon.warehouse".to_owned(),
            required_env("FLUSS_RUST_UNION_READ_WAREHOUSE"),
        )]),
    )
    .await
    .unwrap();
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
    let expected = vec![
        (1, "lake-old"),
        (2, "lake-delete"),
        (3, "lake-keep"),
        (4, "tail-4"),
        (5, "tail-5"),
    ]
    .into_iter()
    .map(|(id, name)| (id, name.to_owned()))
    .collect::<Vec<_>>();
    let reader = scan.new_reader();
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
    let baseline = lake_scan
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
}
