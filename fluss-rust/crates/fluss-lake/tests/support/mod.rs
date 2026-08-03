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

use fluss::ServerType;
use fluss::client::FlussAdmin;
use fluss::error::{Error, FlussError};
use fluss::metadata::{PartitionSpec, TableDescriptor, TablePath};
use fluss::rpc::message::OffsetSpec;
use fluss_test_cluster::{FlussTestingCluster, FlussTestingClusterBuilder};
use std::collections::HashMap;
#[cfg(feature = "paimon")]
use std::path::Path;
#[cfg(feature = "paimon")]
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
#[cfg(feature = "paimon")]
use tokio::process::Command;

const READINESS_TIMEOUT: Duration = Duration::from_secs(30);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(200);

extern "C" fn cleanup_on_exit() {
    SHARED_CLUSTER.stop();
}

static CLUSTER_PORT: LazyLock<u16> =
    LazyLock::new(|| 20_000 + (std::process::id() % 20_000) as u16);

#[cfg(feature = "paimon")]
static SHARED_DATA_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    let data_dir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("target")
        .join(format!("fluss-rust-union-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir)
        .expect("Failed to create Paimon UnionRead shared data directory");
    data_dir
        .canonicalize()
        .expect("Failed to canonicalize Paimon UnionRead shared data directory")
});
#[cfg(feature = "paimon")]
static PAIMON_WAREHOUSE_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| SHARED_DATA_DIR.join("paimon-warehouse"));

static SHARED_CLUSTER: LazyLock<FlussTestingCluster> = LazyLock::new(|| {
    std::thread::spawn(|| {
        let runtime = tokio::runtime::Runtime::new()
            .expect("Failed to create UnionRead integration test runtime");
        runtime.block_on(async {
            #[cfg(feature = "paimon")]
            let cluster_conf = HashMap::from([
                ("datalake.enabled".to_string(), "true".to_string()),
                ("datalake.format".to_string(), "paimon".to_string()),
                (
                    "datalake.paimon.metastore".to_string(),
                    "filesystem".to_string(),
                ),
                (
                    "datalake.paimon.warehouse".to_string(),
                    PAIMON_WAREHOUSE_DIR.to_string_lossy().to_string(),
                ),
            ]);
            #[cfg(feature = "paimon")]
            let cluster = FlussTestingClusterBuilder::new_with_cluster_conf(
                "rust-union-read-test",
                &cluster_conf,
            )
            .with_remote_data_dir(SHARED_DATA_DIR.clone())
            .with_port(*CLUSTER_PORT)
            .build()
            .await;
            #[cfg(not(feature = "paimon"))]
            let cluster = FlussTestingClusterBuilder::new("rust-union-read-test")
                .with_port(*CLUSTER_PORT)
                .build()
                .await;
            wait_for_cluster_ready(&cluster).await;

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

async fn wait_for_cluster_ready(cluster: &FlussTestingCluster) {
    let connection = cluster.get_fluss_connection().await;
    let admin = connection
        .get_admin()
        .expect("Failed to get admin while waiting for UnionRead test cluster");
    let start = Instant::now();
    loop {
        match admin.get_server_nodes().await {
            Ok(nodes)
                if nodes
                    .iter()
                    .any(|node| *node.server_type() == ServerType::TabletServer) =>
            {
                return;
            }
            result if start.elapsed() >= READINESS_TIMEOUT => {
                panic!(
                    "Timed out waiting for a registered tablet server in the UnionRead test \
                     cluster. Last metadata result: {result:?}"
                );
            }
            _ => tokio::time::sleep(READINESS_POLL_INTERVAL).await,
        }
    }
}

#[cfg(feature = "paimon")]
pub fn paimon_warehouse_path() -> &'static Path {
    PAIMON_WAREHOUSE_DIR.as_path()
}

#[cfg(feature = "paimon")]
pub async fn run_java_paimon_tiering_until_offset(
    bootstrap_servers: &str,
    table_path: &TablePath,
    table_id: i64,
    target_log_end_offset: i64,
) {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("Failed to resolve Fluss repository root");
    let status = Command::new(repository_root.join("mvnw"))
        .current_dir(repository_root)
        .args([
            "--offline",
            "--no-transfer-progress",
            "-pl",
            "fluss-lake/fluss-lake-paimon",
            "-Dtest=RustUnionReadTieringE2EHelperTest",
            "test",
        ])
        .env("FLUSS_RUST_UNION_READ_TIERING_E2E", "true")
        .env("FLUSS_RUST_UNION_READ_BOOTSTRAP_SERVERS", bootstrap_servers)
        .env(
            "FLUSS_RUST_UNION_READ_PAIMON_WAREHOUSE",
            paimon_warehouse_path(),
        )
        .env(
            "FLUSS_RUST_UNION_READ_TABLE_DATABASE",
            table_path.database(),
        )
        .env("FLUSS_RUST_UNION_READ_TABLE_NAME", table_path.table())
        .env("FLUSS_RUST_UNION_READ_TABLE_ID", table_id.to_string())
        .env(
            "FLUSS_RUST_UNION_READ_TARGET_LOG_END_OFFSET",
            target_log_end_offset.to_string(),
        )
        .status()
        .await
        .expect("Failed to start Java/Flink Paimon tiering helper");
    assert!(
        status.success(),
        "Java/Flink Paimon tiering helper failed with status {status}; run \
         fluss-rust/scripts/run-paimon-union-read-e2e.sh to build its Maven dependencies first"
    );
}

pub async fn create_table(
    admin: &FlussAdmin,
    table_path: &TablePath,
    table_descriptor: &TableDescriptor,
) {
    let create_start = Instant::now();
    loop {
        match admin
            .create_table(table_path, table_descriptor, false)
            .await
        {
            Ok(()) => break,
            Err(Error::FlussAPIError { api_error })
                if FlussError::for_code(api_error.code) == FlussError::InvalidReplicationFactor
                    && create_start.elapsed() < READINESS_TIMEOUT =>
            {
                tokio::time::sleep(READINESS_POLL_INTERVAL).await;
            }
            Err(error) => {
                panic!("Failed to create UnionRead integration test table: {error}");
            }
        }
    }

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
