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
use fluss::metadata::{TableDescriptor, TablePath};
use fluss::rpc::message::OffsetSpec;
use fluss_test_cluster::{FlussTestingCluster, FlussTestingClusterBuilder};
use std::collections::HashMap;
#[cfg(feature = "paimon")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "paimon")]
use std::process::Command as StdCommand;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
#[cfg(feature = "paimon")]
use tokio::process::Command;

const READINESS_TIMEOUT: Duration = Duration::from_secs(30);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(200);
#[cfg(feature = "paimon")]
const S3_ACCESS_KEY: &str = "rustfsadmin";
#[cfg(feature = "paimon")]
const S3_SECRET_KEY: &str = "rustfsadmin";
#[cfg(feature = "paimon")]
const S3_BUCKET: &str = "fluss";
#[cfg(feature = "paimon")]
const S3_SERVER_IMAGE: &str = "rustfs/rustfs:1.0.0-alpha.83";
#[cfg(feature = "paimon")]
const S3_CLIENT_IMAGE: &str =
    "minio/mc@sha256:a7fe349ef4bd8521fb8497f55c6042871b2ae640607cf99d9bede5e9bdf11727";

extern "C" fn cleanup_on_exit() {
    SHARED_CLUSTER.stop();
}

#[cfg(feature = "paimon")]
extern "C" fn cleanup_s3_on_exit() {
    let _ = StdCommand::new("docker")
        .args(["rm", "-f", &s3_container_name()])
        .status();
}

static CLUSTER_PORT: LazyLock<u16> =
    LazyLock::new(|| 20_000 + (std::process::id() % 20_000) as u16);

#[cfg(feature = "paimon")]
fn s3_container_name() -> String {
    format!("rustfs-rust-union-read-{}", std::process::id())
}

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
static PAIMON_WAREHOUSE_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| SHARED_DATA_DIR.join("paimon-warehouse"));

#[cfg(feature = "paimon")]
pub struct S3Fixture {
    endpoint: String,
}

#[cfg(feature = "paimon")]
impl S3Fixture {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn access_key(&self) -> &'static str {
        S3_ACCESS_KEY
    }

    pub fn secret_key(&self) -> &'static str {
        S3_SECRET_KEY
    }

    pub fn warehouse(&self) -> String {
        format!("s3://{S3_BUCKET}/paimon")
    }
}

#[cfg(feature = "paimon")]
static S3_FIXTURE: LazyLock<S3Fixture> = LazyLock::new(|| {
    let container_name = s3_container_name();
    let port = *CLUSTER_PORT + 1_000;
    let _ = StdCommand::new("docker")
        .args(["rm", "-f", &container_name])
        .status();
    let mapped_port = format!("127.0.0.1:{port}:9000");
    let status = StdCommand::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            &container_name,
            "-p",
            &mapped_port,
            "-e",
            &format!("RUSTFS_ACCESS_KEY={S3_ACCESS_KEY}"),
            "-e",
            &format!("RUSTFS_SECRET_KEY={S3_SECRET_KEY}"),
            S3_SERVER_IMAGE,
            "/data",
        ])
        .status()
        .expect("Failed to start the RustFS S3-compatible test container");
    assert!(
        status.success(),
        "Failed to start the RustFS test container"
    );

    let host_endpoint = format!("http://host.docker.internal:{port}");
    let initialize_script = format!(
        "until mc alias set union-read {host_endpoint} {S3_ACCESS_KEY} {S3_SECRET_KEY}; do \
         sleep 1; done; mc mb --ignore-existing union-read/{S3_BUCKET}"
    );
    let status = StdCommand::new("docker")
        .args([
            "run",
            "--rm",
            "--add-host",
            "host.docker.internal:host-gateway",
            "--entrypoint",
            "/bin/sh",
            S3_CLIENT_IMAGE,
            "-c",
            &initialize_script,
        ])
        .status()
        .expect("Failed to initialize the RustFS test bucket");
    assert!(
        status.success(),
        "Failed to initialize the RustFS test bucket"
    );

    unsafe {
        unsafe extern "C" {
            fn atexit(callback: extern "C" fn()) -> std::os::raw::c_int;
        }
        atexit(cleanup_s3_on_exit);
    }

    S3Fixture {
        endpoint: format!("http://127.0.0.1:{port}"),
    }
});

static SHARED_CLUSTER: LazyLock<FlussTestingCluster> = LazyLock::new(|| {
    std::thread::spawn(|| {
        let runtime = tokio::runtime::Runtime::new()
            .expect("Failed to create UnionRead integration test runtime");
        runtime.block_on(async {
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
            let cluster = FlussTestingClusterBuilder::new_with_cluster_conf(
                "rust-union-read-test",
                &cluster_conf,
            )
            .with_remote_data_dir(SHARED_DATA_DIR.clone())
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
pub fn get_s3_fixture() -> &'static S3Fixture {
    &S3_FIXTURE
}

#[cfg(feature = "paimon")]
pub fn mirror_paimon_warehouse_to_s3() {
    let fixture = get_s3_fixture();
    let mount = format!("{}:/warehouse:ro", paimon_warehouse_path().display());
    let container_endpoint = fixture
        .endpoint()
        .replace("127.0.0.1", "host.docker.internal");
    let mirror_script = format!(
        "mc alias set union-read {container_endpoint} {S3_ACCESS_KEY} {S3_SECRET_KEY} && \
         mc mirror --overwrite /warehouse union-read/{S3_BUCKET}/paimon"
    );
    let status = StdCommand::new("docker")
        .args([
            "run",
            "--rm",
            "--add-host",
            "host.docker.internal:host-gateway",
            "-v",
            &mount,
            "--entrypoint",
            "/bin/sh",
            S3_CLIENT_IMAGE,
            "-c",
            &mirror_script,
        ])
        .status()
        .expect("Failed to mirror the Paimon warehouse into RustFS");
    assert!(
        status.success(),
        "Failed to mirror the Paimon warehouse into RustFS"
    );
}

#[cfg(feature = "paimon")]
pub async fn run_java_paimon_tiering_until_offset(
    bootstrap_servers: &str,
    table_path: &TablePath,
    table_id: i64,
    partition_id: Option<i64>,
    target_log_end_offset: i64,
) {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("Failed to resolve Fluss repository root");
    let mut command = Command::new(repository_root.join("mvnw"));
    command
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
        );
    if let Some(partition_id) = partition_id {
        command.env(
            "FLUSS_RUST_UNION_READ_TARGET_PARTITION_ID",
            partition_id.to_string(),
        );
    }
    let status = command
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
        let readiness = if table_descriptor.is_partitioned() {
            admin
                .get_table_info(table_path)
                .await
                .map(|_| HashMap::new())
        } else {
            admin
                .list_offsets(table_path, &[0], OffsetSpec::Latest)
                .await
        };
        match readiness {
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
