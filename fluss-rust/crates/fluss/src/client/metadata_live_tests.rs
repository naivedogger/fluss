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

//! Explicitly selected tests using an existing cluster and controlled cache publication.
use super::*;
use crate::client::FlussConnection;
use crate::config::Config;
use crate::metadata::{DataTypes, PartitionSpec, Schema, TableDescriptor};
use std::collections::HashMap;
use std::sync::mpsc;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing {name}; use partition run.py"))
}

async fn connection() -> FlussConnection {
    FlussConnection::new(Config {
        bootstrap_servers: env("PART_ENDPOINT"),
        security_protocol: env("PART_SECURITY"),
        security_sasl_mechanism: env("PART_MECHANISM"),
        security_sasl_username: env("PART_USERNAME"),
        security_sasl_password: env("PART_PASSWORD"),
        writer_batch_timeout_ms: 5,
        ..Default::default()
    })
    .await
    .expect("connect to test cluster")
}

fn path(suffix: &str) -> TablePath {
    TablePath::new(
        env("PART_DATABASE"),
        format!("{}_{suffix}", env("PART_TABLE")),
    )
}

async fn create(connection: &FlussConnection, path: &TablePath) {
    println!("OWNED_TABLE {path}");
    let descriptor = TableDescriptor::builder()
        .schema(
            Schema::builder()
                .column("p", DataTypes::string())
                .column("id", DataTypes::int())
                .build()
                .unwrap(),
        )
        .partitioned_by(vec!["p"])
        .distributed_by(Some(1), vec!["id".to_string()])
        .property("table.replication.factor", env("PART_REPLICATION"))
        .build()
        .unwrap();
    let admin = connection.get_admin().unwrap();
    admin.create_table(path, &descriptor, false).await.unwrap();
    admin
        .create_partition(
            path,
            &PartitionSpec::new(HashMap::from([("p", "A")])),
            false,
        )
        .await
        .unwrap();
}

// Request actual coordinator metadata without implicitly publishing it into the target cache.
async fn response(connection: &FlussConnection, path: &TablePath) -> MetadataResponse {
    let metadata = connection.get_metadata();
    let server = metadata
        .get_cluster()
        .get_coordinator_server()
        .unwrap()
        .clone();
    let rpc = connection
        .get_connections()
        .get_connection(&server)
        .await
        .unwrap();
    let physical = Arc::new(PhysicalTablePath::of_partitioned(
        Arc::new(path.clone()),
        Some("A".to_string()),
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let result = rpc
            .request(UpdateMetadataRequest::new(
                &HashSet::from([path]),
                &HashSet::from([&physical]),
                vec![],
            ))
            .await;
        if let Ok(response) = result {
            if response.table_metadata.len() == 1
                && response.partition_metadata.len() == 1
                && response.partition_metadata[0]
                    .bucket_metadata
                    .iter()
                    .any(|bucket| bucket.leader_id.is_some())
            {
                return response;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "table/partition route not ready"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn state(
    cluster: &Cluster,
    path: &TablePath,
) -> (Option<i64>, Option<i64>, Option<i64>, Option<i32>) {
    let physical = PhysicalTablePath::of_partitioned(Arc::new(path.clone()), Some("A".to_string()));
    let table = cluster.get_table_id(path);
    let partition = cluster.get_partition_id(&physical);
    let leader = table.and_then(|id| {
        cluster
            .leader_for(&TableBucket::new_with_partition(id, partition, 0))
            .map(ServerNode::id)
    });
    (
        table,
        cluster.opt_get_table(path).map(|info| info.table_id),
        partition,
        leader,
    )
}

async fn read_write(
    connection: &FlussConnection,
    path: &TablePath,
    value: i32,
    snapshot: &Cluster,
) -> bool {
    use crate::client::table::{EARLIEST_OFFSET, FlussTable};
    use crate::row::{DataGetters, GenericRow};
    let metadata = connection.get_metadata();
    let Some(info) = snapshot.opt_get_table(path).cloned() else {
        println!("READ_WRITE {path}: table missing from cache");
        return false;
    };
    let physical = PhysicalTablePath::of_partitioned(Arc::new(path.clone()), Some("A".to_string()));
    let Some(partition) = snapshot.get_partition_id(&physical) else {
        println!("READ_WRITE {path}: partition missing from cache");
        return false;
    };
    // Preserve the pre-injection TableInfo as a real open table handle would.
    // get_table() here would perform an extra refresh before the I/O probe.
    let table = FlussTable::new(connection, metadata, info);
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let writer = table.new_append()?.create_writer()?;
        let mut row = GenericRow::new(2);
        row.set_field(0, "A");
        row.set_field(1, value);
        writer.append(&row)?.await?;
        let scanner = table.new_scan().create_log_scanner()?;
        scanner
            .subscribe_partition(partition, 0, EARLIEST_OFFSET)
            .await?;
        loop {
            let records = scanner.poll(Duration::from_millis(200)).await?;
            for record in records {
                if record.row().get_int(1)? == value {
                    return Ok::<_, Error>(());
                }
            }
        }
    })
    .await;
    println!("READ_WRITE {path}: {result:?}");
    matches!(result, Ok(Ok(())))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly configured disposable cluster; run via partition runner"]
async fn metadata_late_response() {
    let admin_connection = connection().await;
    let target = connection().await;
    let metadata = target.get_metadata();
    let path = path("late");
    create(&admin_connection, &path).await;
    let old = response(&admin_connection, &path).await;
    metadata.update(old.clone()).await.unwrap();
    assert!(
        read_write(&target, &path, 10, &metadata.get_cluster()).await,
        "pre-fault read/write failed"
    );
    admin_connection
        .get_admin()
        .unwrap()
        .drop_table(&path, false)
        .await
        .unwrap();
    create(&admin_connection, &path).await;
    let new = response(&admin_connection, &path).await;
    assert!(new.table_metadata[0].table_id > old.table_metadata[0].table_id);
    metadata.update(new).await.unwrap();
    assert!(
        read_write(&target, &path, 20, &metadata.get_cluster()).await,
        "new-table read/write failed"
    );
    let snapshot = metadata.get_cluster();
    let expected = state(&snapshot, &path);
    println!(
        "INJECT late-old-response old={} new={}",
        old.table_metadata[0].table_id,
        expected.0.unwrap()
    );
    metadata.update(old).await.unwrap();
    let actual = state(&metadata.get_cluster(), &path);
    println!("CACHE expected={expected:?} actual={actual:?}");
    let io_ok = read_write(&target, &path, 21, &snapshot).await;
    target.close(Duration::from_secs(5)).await.ok();
    assert_eq!(
        actual, expected,
        "late old response rolled back recreated table metadata"
    );
    assert!(io_ok, "post-injection read/write failed");
    if env("PART_KEEP") != "1" {
        admin_connection
            .get_admin()
            .unwrap()
            .drop_table(&path, false)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly configured disposable cluster; run via partition runner"]
async fn metadata_concurrent_update() {
    let admin_connection = connection().await;
    let target = connection().await;
    let a = path("a");
    let b = path("b");
    create(&admin_connection, &a).await;
    create(&admin_connection, &b).await;
    let response_a = response(&admin_connection, &a).await;
    let response_b = response(&admin_connection, &b).await;
    // Baseline I/O uses another connection, preserving the target's S0 snapshot.
    admin_connection
        .get_metadata()
        .update(response_a.clone())
        .await
        .unwrap();
    admin_connection
        .get_metadata()
        .update(response_b.clone())
        .await
        .unwrap();
    assert!(
        read_write(
            &admin_connection,
            &a,
            1,
            &admin_connection.get_metadata().get_cluster()
        )
        .await
    );
    assert!(
        read_write(
            &admin_connection,
            &b,
            1,
            &admin_connection.get_metadata().get_cluster()
        )
        .await
    );
    let metadata = target.get_metadata();
    let initial = metadata.get_cluster();
    let snapshot_a = Cluster::from_metadata_response(response_a.clone(), Some(&initial)).unwrap();
    let snapshot_b = Cluster::from_metadata_response(response_b.clone(), Some(&initial)).unwrap();
    let expected_a = state(&snapshot_a, &a);
    let expected_b = state(&snapshot_b, &b);
    let spawn_update = |response| {
        let metadata = Arc::clone(&metadata);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::tests::install_update_snapshot_gate(ready_tx, resume_rx);
            futures::executor::block_on(metadata.update(response)).unwrap();
        });
        (worker, ready_rx, resume_tx)
    };
    let (worker_a, ready_a, resume_a) = spawn_update(response_a);
    let (worker_b, ready_b, resume_b) = spawn_update(response_b);
    assert!(Arc::ptr_eq(
        &initial,
        &ready_a.recv_timeout(Duration::from_secs(10)).unwrap()
    ));
    assert!(Arc::ptr_eq(
        &initial,
        &ready_b.recv_timeout(Duration::from_secs(10)).unwrap()
    ));
    println!("INJECT concurrent-update both-captured-S0=true");
    resume_a.send(()).unwrap();
    worker_a.join().unwrap();
    assert_eq!(state(&metadata.get_cluster(), &a), expected_a);
    resume_b.send(()).unwrap();
    worker_b.join().unwrap();
    let actual = (
        state(&metadata.get_cluster(), &a),
        state(&metadata.get_cluster(), &b),
    );
    println!(
        "CACHE expected={:?} actual={actual:?}",
        (expected_a, expected_b)
    );
    let io_a = read_write(&target, &a, 2, &snapshot_a).await;
    let io_b = read_write(&target, &b, 2, &snapshot_b).await;
    target.close(Duration::from_secs(5)).await.ok();
    assert_eq!(
        actual,
        (expected_a, expected_b),
        "concurrent update lost table metadata"
    );
    assert!(io_a && io_b, "post-injection read/write failed");
    if env("PART_KEEP") != "1" {
        for path in [&a, &b] {
            admin_connection
                .get_admin()
                .unwrap()
                .drop_table(path, false)
                .await
                .unwrap();
        }
    }
}
