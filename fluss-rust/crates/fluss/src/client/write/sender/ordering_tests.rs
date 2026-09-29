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

use super::*;
use crate::client::write::write_format::WriteFormat;
use crate::client::{ResultHandle, WriteRecord};
use crate::cluster::{BucketLocation, Cluster, ServerNode, ServerType};
use crate::config::Config;
use crate::metadata::{JsonSerde, TableInfo};
use crate::proto::{
    ApiVersionsResponse, MetadataResponse, PbApiVersion, PbBucketMetadata, PbServerNode,
    PbTableMetadata, PbTablePath,
};
use crate::record::LogRecordBatch;
use crate::record::kv::KvRecordBatch;
use crate::row::{Datum, GenericRow};
use crate::test_utils::build_table_info;
use bytes::Bytes;
use futures::FutureExt;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

struct Fixture {
    sender: Arc<Sender>,
    cluster: Arc<Cluster>,
    tables: Vec<Arc<TableInfo>>,
}

impl Fixture {
    fn new(ports: &[u16], table_nodes: &[usize], max_inflight: usize) -> Self {
        Self::with_linger(ports, table_nodes, max_inflight, 0)
    }

    fn with_linger(
        ports: &[u16],
        table_nodes: &[usize],
        max_inflight: usize,
        linger_ms: i64,
    ) -> Self {
        let servers: Vec<_> = ports
            .iter()
            .enumerate()
            .map(|(i, port)| {
                ServerNode::new(
                    i as i32 + 1,
                    "127.0.0.1".into(),
                    *port as u32,
                    ServerType::TabletServer,
                )
            })
            .collect();
        let mut locations_by_path = HashMap::new();
        let mut locations_by_bucket = HashMap::new();
        let mut ids = HashMap::new();
        let mut infos = HashMap::new();
        let mut tables = Vec::new();
        for (i, &node) in table_nodes.iter().enumerate() {
            let id = i as i64 + 1;
            let path = TablePath::new("db", format!("table_{id}"));
            let physical = Arc::new(PhysicalTablePath::of(Arc::new(path.clone())));
            let bucket = TableBucket::new(id, 0);
            let location = BucketLocation::new(
                bucket.clone(),
                Some(servers[node].clone()),
                physical.clone(),
            );
            locations_by_path.insert(physical, vec![location.clone()]);
            locations_by_bucket.insert(bucket, location);
            ids.insert(path.clone(), id);
            let info = build_table_info(path.clone(), id, 1);
            tables.push(Arc::new(info.clone()));
            infos.insert(path, info);
        }
        let cluster = Arc::new(Cluster::new(
            None,
            servers.into_iter().map(|node| (node.id(), node)).collect(),
            locations_by_path,
            locations_by_bucket,
            ids,
            infos,
            HashMap::new(),
        ));
        let idempotence = Arc::new(IdempotenceManager::new(true, max_inflight));
        let config = Config {
            writer_batch_timeout_ms: linger_ms,
            writer_max_inflight_requests_per_bucket: max_inflight,
            ..Config::default()
        };
        let retry_backoff_ms = config.writer_retry_backoff_ms;
        let retry_max_backoff_ms = config.writer_retry_max_backoff_ms;
        idempotence.set_writer_id(42);
        let accumulator = Arc::new(RecordAccumulator::new(config, idempotence.clone()));
        let sender = Arc::new(Sender::new(
            Arc::new(Metadata::new_for_test(cluster.clone())),
            accumulator,
            1024 * 1024,
            10_000,
            -1,
            i32::MAX,
            retry_backoff_ms,
            retry_max_backoff_ms,
            idempotence,
            Arc::new(WriterMetrics::new()),
        ));
        Self {
            sender,
            cluster,
            tables,
        }
    }

    fn append(&self, table: usize) -> ResultHandle {
        let info = self.tables[table].clone();
        let path = Arc::new(PhysicalTablePath::of(Arc::new(info.table_path.clone())));
        let row = GenericRow {
            values: vec![Datum::Int32(7)],
        };
        let record = WriteRecord::for_append(info, path, 1, &row);
        self.append_record(&record)
    }

    fn delete(&self, table: usize) -> ResultHandle {
        let info = self.tables[table].clone();
        let path = Arc::new(PhysicalTablePath::of(Arc::new(info.table_path.clone())));
        let record = WriteRecord::for_upsert(
            info,
            path,
            1,
            Bytes::from_static(b"key"),
            None,
            WriteFormat::CompactedKv,
            None,
            None,
        );
        self.append_record(&record)
    }

    fn append_record(&self, record: &WriteRecord<'_>) -> ResultHandle {
        let result = self
            .sender
            .accumulator
            .append(record, 0, &self.cluster, false)
            .unwrap();
        self.sender.accumulator.wakeup_sender();
        result.result_handle.unwrap()
    }

    fn start(&self) -> (mpsc::Sender<()>, JoinHandle<Result<()>>) {
        let (tx, rx) = mpsc::channel(1);
        let sender = self.sender.clone();
        (
            tx,
            tokio::spawn(async move { sender.run_with_shutdown(rx).await }),
        )
    }

    async fn finish(&self, tx: mpsc::Sender<()>, task: JoinHandle<Result<()>>) {
        drop(tx);
        task.await.unwrap().unwrap();
        assert!(!self.sender.accumulator.has_incomplete());
        assert!(self.sender.in_flight_batches.lock().is_empty());
        assert_eq!(
            self.sender.accumulator.buffer_available_bytes(),
            self.sender.accumulator.buffer_total_bytes()
        );
    }

    async fn move_leader(&self, table: usize, leader: i32) {
        let info = &self.tables[table];
        let descriptor = info
            .to_table_descriptor()
            .unwrap()
            .serialize_json()
            .unwrap();
        self.sender
            .metadata
            .update(MetadataResponse {
                tablet_servers: self
                    .cluster
                    .get_server_nodes()
                    .iter()
                    .map(|node| PbServerNode {
                        node_id: node.id(),
                        host: node.host().to_string(),
                        port: node.port() as i32,
                        ..Default::default()
                    })
                    .collect(),
                table_metadata: vec![PbTableMetadata {
                    table_path: PbTablePath {
                        database_name: info.table_path.database().to_string(),
                        table_name: info.table_path.table().to_string(),
                    },
                    table_id: info.table_id,
                    schema_id: info.schema_id,
                    table_json: serde_json::to_vec(&descriptor).unwrap(),
                    bucket_metadata: vec![PbBucketMetadata {
                        bucket_id: 0,
                        leader_id: Some(leader),
                        ..Default::default()
                    }],
                    created_time: info.created_time,
                    modified_time: info.modified_time,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            self.sender
                .metadata
                .get_cluster()
                .leader_for(&TableBucket::new(info.table_id, 0))
                .unwrap()
                .id(),
            leader
        );
    }
}

async fn bounded(future: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("sender made no progress");
}

async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    let len = stream.read_i32().await.unwrap();
    let mut frame = vec![0; len as usize];
    stream.read_exact(&mut frame).await.unwrap();
    frame
}

async fn reply(stream: &mut TcpStream, id: i32, response: impl Message) {
    let body = response.encode_to_vec();
    stream.write_i32((5 + body.len()) as i32).await.unwrap();
    stream.write_u8(0).await.unwrap();
    stream.write_i32(id).await.unwrap();
    stream.write_all(&body).await.unwrap();
    stream.flush().await.unwrap();
}

async fn handshake(stream: &mut TcpStream, hello: &[u8]) {
    assert_eq!(i16::from_be_bytes(hello[..2].try_into().unwrap()), 1000);
    let id = i32::from_be_bytes(hello[4..8].try_into().unwrap());
    reply(
        stream,
        id,
        ApiVersionsResponse {
            api_versions: vec![
                PbApiVersion {
                    api_key: 1000,
                    min_version: 0,
                    max_version: 0,
                },
                PbApiVersion {
                    api_key: 1014,
                    min_version: 0,
                    max_version: 0,
                },
                PbApiVersion {
                    api_key: 1016,
                    min_version: 0,
                    max_version: 0,
                },
            ],
            server_type: Some(ServerType::TabletServer.to_type_id()),
        },
    )
    .await;
}

async fn read_produce(stream: &mut TcpStream) -> (i32, i64, i32) {
    let frame = read_frame(stream).await;
    assert_eq!(i16::from_be_bytes(frame[..2].try_into().unwrap()), 1014);
    let id = i32::from_be_bytes(frame[4..8].try_into().unwrap());
    let request = crate::proto::ProduceLogRequest::decode(&frame[8..]).unwrap();
    assert_eq!(request.buckets_req.len(), 1);
    let batch = LogRecordBatch::new(Bytes::copy_from_slice(&request.buckets_req[0].records));
    assert_eq!(batch.writer_id(), 42);
    (id, request.table_id, batch.batch_sequence())
}

async fn ack(stream: &mut TcpStream, id: i32, error: FlussError) {
    reply(
        stream,
        id,
        ProduceLogResponse {
            buckets_resp: vec![PbProduceLogRespForBucket {
                bucket_id: 0,
                error_code: Some(error.code()),
                ..Default::default()
            }],
        },
    )
    .await;
}

#[tokio::test]
async fn cold_connection_cannot_overtake_predecessor() {
    bounded(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::with_linger(&[listener.local_addr().unwrap().port()], &[0], 5, 100);
        // Flush makes both batches immediately sendable without relying on a zero
        // linger or on the sender's idle timer while the first handshake is stalled.
        fixture.sender.accumulator.begin_flush();
        let first = fixture.append(0);
        let (tx, task) = fixture.start();
        let (mut predecessor_stream, _) = listener.accept().await.unwrap();
        let predecessor_hello = read_frame(&mut predecessor_stream).await;

        // Batch 0 has already been drained, so this append gets its own sequence.
        // Hold its handshake while giving batch 1 an opportunity to connect.
        let second = fixture.append(0);
        let mut stream = match tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
        {
            Ok(accepted) => {
                let (mut successor_stream, _) = accepted.unwrap();
                let hello = read_frame(&mut successor_stream).await;
                // Before the fix, the successor opens another connection and
                // sends sequence 1 as soon as this handshake completes.
                handshake(&mut successor_stream, &hello).await;
                successor_stream
            }
            Err(_) => {
                // With ordered dispatch, only the predecessor may connect.
                // Release it after the bounded observation window.
                handshake(&mut predecessor_stream, &predecessor_hello).await;
                predecessor_stream
            }
        };
        let (id0, table0, seq0) = read_produce(&mut stream).await;
        assert_eq!(table0, 1);
        assert_eq!(seq0, 0, "successor overtook predecessor on the wire");
        let (id1, table1, seq1) = read_produce(&mut stream).await;
        assert_eq!((table1, seq1), (1, 1));
        // Both frames must arrive before either ACK: the fix must preserve
        // pipelining, not serialize complete request/response round trips.
        assert!(first.wait().now_or_never().is_none());
        assert!(second.wait().now_or_never().is_none());
        assert!(
            listener.accept().now_or_never().is_none(),
            "only one connection should be opened"
        );
        // Even out-of-order ACK delivery must not block dispatch or lose registrations.
        ack(&mut stream, id1, FlussError::None).await;
        assert!(second.wait().await.unwrap().is_ok());
        ack(&mut stream, id0, FlussError::None).await;
        assert!(first.wait().await.unwrap().is_ok());
        fixture
            .sender
            .accumulator
            .await_flush_completion()
            .await
            .unwrap();
        fixture.finish(tx, task).await;
    })
    .await;
}

#[tokio::test]
async fn kv_requests_preserve_order_without_waiting_for_ack() {
    bounded(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(&[listener.local_addr().unwrap().port()], &[0], 5);
        let first = fixture.delete(0);
        let (tx, task) = fixture.start();
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello = read_frame(&mut stream).await;
        let second = fixture.delete(0);
        handshake(&mut stream, &hello).await;
        let mut ids = Vec::new();
        for sequence in 0..2 {
            let frame = read_frame(&mut stream).await;
            assert_eq!(i16::from_be_bytes(frame[..2].try_into().unwrap()), 1016);
            ids.push(i32::from_be_bytes(frame[4..8].try_into().unwrap()));
            let request = crate::proto::PutKvRequest::decode(&frame[8..]).unwrap();
            assert_eq!(request.table_id, 1);
            assert_eq!(request.buckets_req.len(), 1);
            let batch =
                KvRecordBatch::new(Bytes::copy_from_slice(&request.buckets_req[0].records), 0);
            assert_eq!(batch.writer_id().unwrap(), 42);
            assert_eq!(batch.batch_sequence().unwrap(), sequence);
        }
        assert!(first.wait().now_or_never().is_none());
        for id in ids {
            reply(
                &mut stream,
                id,
                PutKvResponse {
                    buckets_resp: vec![PbPutKvRespForBucket {
                        bucket_id: 0,
                        ..Default::default()
                    }],
                },
            )
            .await;
        }
        assert!(first.wait().await.unwrap().is_ok());
        assert!(second.wait().await.unwrap().is_ok());
        fixture.finish(tx, task).await;
    })
    .await;
}

#[tokio::test]
async fn response_keeps_evicted_connection_alive_until_ack() {
    bounded(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(&[listener.local_addr().unwrap().port()], &[0], 5);
        let handle = fixture.append(0);
        let mut batches = fixture
            .sender
            .drain_ready_sends(&SendQueues::default())
            .unwrap()
            .0
            .remove(&1)
            .unwrap();
        let request = Sender::build_write_request(1, -1, 10_000, &mut batches).unwrap();
        let records = batches
            .into_iter()
            .map(|batch| (batch.table_bucket.clone(), batch))
            .collect();
        let client = crate::rpc::RpcClient::new();
        let node = fixture.cluster.get_tablet_server(1).unwrap();
        let (connection, mut stream) = tokio::join!(client.get_connection(node), async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello = read_frame(&mut stream).await;
            handshake(&mut stream, &hello).await;
            stream
        });
        let connection = connection.unwrap();
        let weak = Arc::downgrade(&connection);
        let response = fixture
            .sender
            .dispatch_and_handle_response(&connection, request, 1, records)
            .await
            .unwrap()
            .unwrap();
        client.disconnect(node.uid());
        drop(connection);
        assert!(
            weak.upgrade().is_some(),
            "pending ACK must retain the read task"
        );
        let (id, _, _) = read_produce(&mut stream).await;
        ack(&mut stream, id, FlussError::None).await;
        response.await.unwrap();
        assert!(handle.wait().await.unwrap().is_ok());
        assert!(weak.upgrade().is_none());
        assert!(fixture.sender.in_flight_batches.lock().is_empty());
    })
    .await;
}

#[tokio::test]
async fn exhausted_cooperative_budget_cannot_reorder_healthy_connection_writes() {
    bounded(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(&[listener.local_addr().unwrap().port()], &[0], 5);
        let node = fixture.cluster.get_tablet_server(1).unwrap().clone();
        // Warm the real RPC connection before exhausting the Sender task's budget.
        let (connection, mut stream) =
            tokio::join!(fixture.sender.metadata.get_connection(&node), async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let hello = read_frame(&mut stream).await;
                handshake(&mut stream, &hello).await;
                stream
            });
        let _connection = connection.unwrap();
        let mut queues = SendQueues::default();
        let mut handles = Vec::new();
        // Stage two numbered requests to exercise the queue head rule directly,
        // independently of the production drain gate (which is even stricter).
        for _ in 0..2 {
            handles.push(fixture.append(0));
            let mut batches = fixture
                .sender
                .accumulator
                .drain(
                    fixture.cluster.clone(),
                    &HashSet::from([node.clone()]),
                    1024 * 1024,
                )
                .unwrap();
            fixture.sender.add_to_inflight_batches(&batches);
            queues
                .nodes
                .entry(1)
                .or_default()
                .push_back(batches.remove(&1).unwrap());
        }
        let mut dispatches = FuturesUnordered::new();
        queues.dispatch(&fixture.sender, &dispatches);
        while tokio::task::coop::has_budget_remaining() {
            tokio::task::consume_budget().await;
        }
        // The head is suspended before acquiring the healthy connection's write lock.
        assert!(dispatches.next().now_or_never().is_none());
        queues.dispatch(&fixture.sender, &dispatches);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(queues.nodes[&1].len(), 1, "successor must remain queued");

        let (node_id, response0) = dispatches.next().await.unwrap();
        queues.complete_dispatch(node_id);
        let response0 = response0.unwrap().unwrap();
        let (id0, _, seq0) = read_produce(&mut stream).await;
        queues.dispatch(&fixture.sender, &dispatches);
        let (node_id, response1) = dispatches.next().await.unwrap();
        queues.complete_dispatch(node_id);
        let response1 = response1.unwrap().unwrap();
        let (id1, _, seq1) = read_produce(&mut stream).await;
        assert_eq!((seq0, seq1), (0, 1));
        // Both frames are already present while no ACK has been sent.
        ack(&mut stream, id0, FlussError::None).await;
        ack(&mut stream, id1, FlussError::None).await;
        let (a, b) = tokio::join!(response0, response1);
        a.unwrap();
        b.unwrap();
        for handle in handles {
            assert!(handle.wait().await.unwrap().is_ok());
        }
        assert!(queues.nodes.is_empty());
        assert!(fixture.sender.in_flight_batches.lock().is_empty());
    })
    .await;
}

#[tokio::test]
async fn writer_reset_during_handshake_fails_unsent_batch() {
    bounded(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(&[listener.local_addr().unwrap().port()], &[0], 5);
        let handle = fixture.append(0);
        let (tx, task) = fixture.start();
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello = read_frame(&mut stream).await;
        fixture.sender.idempotence_manager.reset_writer_id();
        fixture.sender.idempotence_manager.set_writer_id(99);
        handshake(&mut stream, &hello).await;
        assert!(matches!(handle.wait().await.unwrap(),
            Err(broadcast::Error::WriteFailed { code, .. })
                if code == FlussError::UnknownWriterIdException.code()
        ));
        fixture.finish(tx, task).await;
        assert!(
            read_frame(&mut stream).now_or_never().is_none(),
            "old writer must not be sent"
        );
    })
    .await;
}

#[tokio::test]
async fn stalled_handshake_does_not_block_other_nodes() {
    bounded(async {
        let slow = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fast = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::with_linger(
            &[
                slow.local_addr().unwrap().port(),
                fast.local_addr().unwrap().port(),
            ],
            &[0, 1],
            5,
            100,
        );
        let a = fixture.append(0);
        let (tx, task) = fixture.start();
        let (mut slow_stream, _) = slow.accept().await.unwrap();
        let slow_hello = read_frame(&mut slow_stream).await;
        // B becomes ready only after its linger timer expires. The stalled A
        // dispatch must not disable that timer or require another producer wakeup.
        let b = fixture.append(1);
        let (mut fast_stream, _) = fast.accept().await.unwrap();
        let fast_hello = read_frame(&mut fast_stream).await;
        handshake(&mut fast_stream, &fast_hello).await;
        let (id, table, seq) = read_produce(&mut fast_stream).await;
        assert_eq!((table, seq), (2, 0));
        ack(&mut fast_stream, id, FlussError::None).await;
        assert!(b.wait().await.unwrap().is_ok());
        assert!(a.wait().now_or_never().is_none());
        handshake(&mut slow_stream, &slow_hello).await;
        let (id, _, seq) = read_produce(&mut slow_stream).await;
        assert_eq!(seq, 0);
        ack(&mut slow_stream, id, FlussError::None).await;
        assert!(a.wait().await.unwrap().is_ok());
        fixture.finish(tx, task).await;
    })
    .await;
}

#[tokio::test]
async fn same_node_tables_do_not_wait_for_each_others_ack() {
    bounded(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(&[listener.local_addr().unwrap().port()], &[0, 0], 5);
        let a = fixture.append(0);
        let b = fixture.append(1);
        let (tx, task) = fixture.start();
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello = read_frame(&mut stream).await;
        handshake(&mut stream, &hello).await;
        let (id0, table0, seq0) = read_produce(&mut stream).await;
        let (id1, table1, seq1) = read_produce(&mut stream).await;
        assert_ne!(table0, table1);
        assert_eq!((seq0, seq1), (0, 0));
        ack(&mut stream, id1, FlussError::None).await;
        ack(&mut stream, id0, FlussError::None).await;
        assert!(a.wait().await.unwrap().is_ok());
        assert!(b.wait().await.unwrap().is_ok());
        fixture.finish(tx, task).await;
    })
    .await;
}

#[tokio::test]
async fn shutdown_processes_responses_and_retries_before_finishing() {
    bounded(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(&[listener.local_addr().unwrap().port()], &[0], 1);
        let a = fixture.append(0);
        let (tx, task) = fixture.start();
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello = read_frame(&mut stream).await;
        handshake(&mut stream, &hello).await;
        let (id0, _, _) = read_produce(&mut stream).await;
        let b = fixture.append(0);
        // max-inflight=1 means draining B requires completing A, even during close.
        drop(tx);
        ack(&mut stream, id0, FlussError::RequestTimeOut).await;
        let (retry, _, sequence) = read_produce(&mut stream).await;
        assert_eq!(sequence, 0);
        ack(&mut stream, retry, FlussError::None).await;
        let (id1, _, sequence) = read_produce(&mut stream).await;
        assert_eq!(sequence, 1);
        ack(&mut stream, id1, FlussError::None).await;
        assert!(a.wait().await.unwrap().is_ok());
        assert!(b.wait().await.unwrap().is_ok());
        task.await.unwrap().unwrap();
        assert!(!fixture.sender.accumulator.has_incomplete());
        assert!(fixture.sender.in_flight_batches.lock().is_empty());
    })
    .await;
}

#[test]
fn queued_node_is_not_drained_again_before_dispatch_completes() {
    let fixture = Fixture::new(&[9092], &[0], 5);
    fixture.append(0);
    let mut queues = SendQueues::default();
    let (batches, _, _) = fixture.sender.drain_ready_sends(&queues).unwrap();
    queues.enqueue(batches);
    fixture.append(0);
    let (later, _, _) = fixture.sender.drain_ready_sends(&queues).unwrap();
    assert!(later.is_empty());
    assert!(fixture.sender.accumulator.has_undrained());
    assert_eq!(
        fixture
            .sender
            .idempotence_manager
            .in_flight_count(&TableBucket::new(1, 0)),
        1
    );
}

#[tokio::test]
async fn leader_change_retries_out_of_order_batches_without_resetting_writer() {
    bounded(async {
        let old_leader = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let new_leader = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(
            &[
                old_leader.local_addr().unwrap().port(),
                new_leader.local_addr().unwrap().port(),
            ],
            &[0],
            5,
        );
        let first = fixture.append(0);
        let (tx, task) = fixture.start();
        let (mut old_stream, _) = old_leader.accept().await.unwrap();
        let hello = read_frame(&mut old_stream).await;
        fixture.move_leader(0, 2).await;
        handshake(&mut old_stream, &hello).await;
        // A leader change during connection setup must not synthesize a server error:
        // the already-selected attempt is sent and resolved by its actual response.
        let (first_id, _, sequence) = read_produce(&mut old_stream).await;
        assert_eq!(sequence, 0);

        let second = fixture.append(0);
        // The old attempt is unresolved, but it must not gate dispatch to the new leader.
        let (mut new_stream, _) = new_leader.accept().await.unwrap();
        let hello = read_frame(&mut new_stream).await;
        handshake(&mut new_stream, &hello).await;
        let (second_id, _, sequence) = read_produce(&mut new_stream).await;
        assert_eq!(sequence, 1);
        assert!(first.wait().now_or_never().is_none());
        ack(
            &mut new_stream,
            second_id,
            FlussError::OutOfOrderSequenceException,
        )
        .await;
        // Wait for the response handler to re-enqueue sequence 1. Its predecessor
        // is still in flight, so the normal retry gate must keep it queued.
        while !fixture.sender.accumulator.has_undrained() {
            tokio::task::yield_now().await;
        }
        assert!(second.wait().now_or_never().is_none());
        assert_eq!(fixture.sender.idempotence_manager.writer_id(), 42);

        // A timed-out old attempt is retried on the new leader. The retry queue
        // must restore 0, 1 ordering even though sequence 1's error arrived first.
        ack(&mut old_stream, first_id, FlussError::RequestTimeOut).await;
        let (retry0, _, sequence) = read_produce(&mut new_stream).await;
        assert_eq!(sequence, 0);
        ack(&mut new_stream, retry0, FlussError::None).await;
        assert!(first.wait().await.unwrap().is_ok());
        let (retry1, _, sequence) = read_produce(&mut new_stream).await;
        assert_eq!(sequence, 1);
        ack(&mut new_stream, retry1, FlussError::None).await;
        assert!(second.wait().await.unwrap().is_ok());
        assert_eq!(fixture.sender.idempotence_manager.writer_id(), 42);
        fixture.finish(tx, task).await;
    })
    .await;
}

#[test]
fn already_completed_batch_releases_physical_attempt() {
    let fixture = Fixture::new(&[9092], &[0], 5);
    fixture.append(0);
    let mut batches = fixture
        .sender
        .drain_ready_sends(&SendQueues::default())
        .unwrap()
        .0;
    let batch = batches.remove(&1).unwrap().pop().unwrap();
    assert!(batch.write_batch.complete(Ok(())));
    fixture
        .sender
        .accumulator
        .remove_incomplete_batches(batch.write_batch.batch_id());
    fixture.sender.complete_batch(batch);
    assert!(fixture.sender.in_flight_batches.lock().is_empty());
    assert!(!fixture.sender.accumulator.has_incomplete());
    assert_eq!(
        fixture
            .sender
            .idempotence_manager
            .in_flight_count(&TableBucket::new(1, 0)),
        0,
    );
}
