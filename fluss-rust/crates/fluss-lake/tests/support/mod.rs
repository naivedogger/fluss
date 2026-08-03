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

use fluss::client::FlussAdmin;
use fluss::metadata::{PartitionSpec, TableDescriptor, TablePath};
use fluss::rpc::message::OffsetSpec;
use fluss_test_cluster::{FlussTestingCluster, FlussTestingClusterBuilder};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

const READINESS_TIMEOUT: Duration = Duration::from_secs(30);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(200);

extern "C" fn cleanup_on_exit() {
    SHARED_CLUSTER.stop();
}

static SHARED_CLUSTER: LazyLock<FlussTestingCluster> = LazyLock::new(|| {
    std::thread::spawn(|| {
        let runtime = tokio::runtime::Runtime::new()
            .expect("Failed to create UnionRead integration test runtime");
        runtime.block_on(async {
            let cluster = FlussTestingClusterBuilder::new("rust-union-read-test")
                .with_port(9323)
                .build()
                .await;
            cluster.get_fluss_connection().await;

            unsafe {
                unsafe extern "C" {
                    fn atexit(callback: extern "C" fn()) -> std::os::raw::c_int;
                }
                atexit(cleanup_on_exit);
            }

            cluster
        })
    })
    .join()
    .expect("Failed to initialize UnionRead integration test cluster")
});

pub fn get_shared_cluster() -> Arc<FlussTestingCluster> {
    Arc::new(SHARED_CLUSTER.clone())
}

pub async fn create_table(
    admin: &FlussAdmin,
    table_path: &TablePath,
    table_descriptor: &TableDescriptor,
) {
    admin
        .create_table(table_path, table_descriptor, false)
        .await
        .expect("Failed to create UnionRead integration test table");

    let start = Instant::now();
    loop {
        match admin
            .list_offsets(table_path, &[0], OffsetSpec::Latest)
            .await
        {
            Ok(_) => return,
            Err(error) if start.elapsed() < READINESS_TIMEOUT => {
                tokio::time::sleep(READINESS_POLL_INTERVAL).await;
                if start.elapsed() >= READINESS_TIMEOUT {
                    panic!(
                        "Timed out waiting for UnionRead integration test table {table_path}: {error}"
                    );
                }
            }
            Err(error) => {
                panic!(
                    "Timed out waiting for UnionRead integration test table {table_path}: {error}"
                );
            }
        }
    }
}

/// Creates a partitioned table and one partition per value, waiting until
/// every partition can serve offset requests for bucket 0.
pub async fn create_partitioned_table(
    admin: &FlussAdmin,
    table_path: &TablePath,
    table_descriptor: &TableDescriptor,
    partition_column: &str,
    partition_values: &[&str],
) {
    admin
        .create_table(table_path, table_descriptor, false)
        .await
        .expect("Failed to create UnionRead integration test table");

    for partition_value in partition_values {
        let mut partition_map = HashMap::new();
        partition_map.insert(partition_column, *partition_value);
        admin
            .create_partition(table_path, &PartitionSpec::new(partition_map), true)
            .await
            .expect("Failed to create UnionRead integration test partition");
    }

    for partition_value in partition_values {
        let start = Instant::now();
        loop {
            match admin
                .list_partition_offsets(table_path, partition_value, &[0], OffsetSpec::Latest)
                .await
            {
                Ok(_) => break,
                Err(error) => {
                    if start.elapsed() >= READINESS_TIMEOUT {
                        panic!(
                            "Timed out waiting for UnionRead integration test partition {partition_value} of {table_path}: {error}"
                        );
                    }
                    tokio::time::sleep(READINESS_POLL_INTERVAL).await;
                }
            }
        }
    }
}
