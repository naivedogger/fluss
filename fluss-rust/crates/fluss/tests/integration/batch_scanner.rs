/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#[cfg(test)]
mod batch_scanner_test {
    use crate::integration::utils::{
        create_table, get_shared_cluster, wait_for_table_buckets_ready, wait_for_table_ready,
    };
    use arrow::array::{Array, Int32Array, Int64Array, StringArray, record_batch};
    use fluss::error::Error;
    use fluss::metadata::{
        AddColumn, AlterTableChanges, ColumnPositionType, DataTypes, JsonSerde, LogFormat, Schema,
        TableBucket, TableDescriptor, TablePath,
    };
    use fluss::row::GenericRow;
    use futures::TryStreamExt;
    use std::collections::HashMap;

    /// End-to-end check that the scanner yields the appended rows once and then
    /// `None`, honoring the configured limit.
    #[tokio::test]
    async fn batch_scanner_returns_appended_rows_then_none() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_batch_scanner_log");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("c1", DataTypes::int())
                    .column("c2", DataTypes::string())
                    .build()
                    .expect("schema"),
            )
            // Single bucket so a single BatchScanner sees every row.
            .distributed_by(Some(1), vec!["c1".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let writer = table
            .new_append()
            .expect("append")
            .create_writer()
            .expect("writer");

        let batch = record_batch!(
            ("c1", Int32, [1, 2, 3, 4, 5]),
            ("c2", Utf8, ["a", "b", "c", "d", "e"])
        )
        .unwrap();
        writer.append_arrow_batch(batch).expect("append batch");
        writer.flush().await.expect("flush");

        let table_info = table.get_table_info();
        let bucket = TableBucket::new(table_info.table_id, 0);

        let mut scanner = table
            .new_scan()
            .limit(3)
            .expect("limit")
            .create_bucket_batch_scanner(bucket.clone())
            .expect("create batch scanner");

        let first = scanner
            .next_batch()
            .await
            .expect("poll")
            .expect("first batch should be Some");

        assert_eq!(first.bucket(), &bucket);
        // The server may return fewer rows than the limit on the first call,
        // but must never exceed it.
        assert!(
            first.num_records() > 0 && first.num_records() <= 3,
            "expected 1..=3 records, got {}",
            first.num_records()
        );

        assert!(
            scanner.next_batch().await.expect("poll").is_none(),
            "scanner must end after one batch"
        );
    }

    /// `into_stream` yields the scanner's single batch then ends, mirroring the
    /// `next_batch` -> `Some` -> `None` sequence.
    #[tokio::test]
    async fn batch_scanner_into_stream_yields_single_batch() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_batch_scanner_into_stream");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("c1", DataTypes::int())
                    .column("c2", DataTypes::string())
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["c1".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let writer = table
            .new_append()
            .expect("append")
            .create_writer()
            .expect("writer");
        writer
            .append_arrow_batch(
                record_batch!(
                    ("c1", Int32, [1, 2, 3, 4, 5]),
                    ("c2", Utf8, ["a", "b", "c", "d", "e"])
                )
                .unwrap(),
            )
            .expect("append batch");
        writer.flush().await.expect("flush");

        let table_info = table.get_table_info();
        let bucket = TableBucket::new(table_info.table_id, 0);

        let scanner = table
            .new_scan()
            .limit(3)
            .expect("limit")
            .create_bucket_batch_scanner(bucket.clone())
            .expect("create batch scanner");

        let batches = scanner
            .into_stream()
            .try_collect::<Vec<_>>()
            .await
            .expect("drain batch scanner stream");

        assert_eq!(batches.len(), 1, "limit scanner yields exactly one batch");
        assert_eq!(batches[0].bucket(), &bucket);
        assert!(
            batches[0].num_records() > 0 && batches[0].num_records() <= 3,
            "expected 1..=3 records, got {}",
            batches[0].num_records()
        );
    }

    /// End-to-end projection skipping the middle `c2` string column.
    #[tokio::test]
    async fn batch_scanner_projects_non_contiguous_columns() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_batch_scanner_projection");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("c1", DataTypes::int())
                    .column("c2", DataTypes::string())
                    .column("c3", DataTypes::bigint())
                    .build()
                    .expect("schema"),
            )
            // Single bucket so a single BatchScanner sees every row.
            .distributed_by(Some(1), vec!["c1".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let writer = table
            .new_append()
            .expect("append")
            .create_writer()
            .expect("writer");

        let batch = record_batch!(
            ("c1", Int32, [1, 2, 3]),
            ("c2", Utf8, ["a", "b", "c"]),
            ("c3", Int64, [100, 200, 300])
        )
        .unwrap();
        writer.append_arrow_batch(batch).expect("append batch");
        writer.flush().await.expect("flush");

        let table_info = table.get_table_info();
        let bucket = TableBucket::new(table_info.table_id, 0);

        let mut scanner = table
            .new_scan()
            .project(&[0, 2])
            .expect("project")
            .limit(10)
            .expect("limit")
            .create_bucket_batch_scanner(bucket.clone())
            .expect("create batch scanner");

        let first = scanner
            .next_batch()
            .await
            .expect("poll")
            .expect("first batch should be Some");

        let rows = first.batch();
        assert_eq!(rows.num_columns(), 2, "projected to c1 + c3");
        assert_eq!(rows.schema().field(0).name(), "c1");
        assert_eq!(rows.schema().field(1).name(), "c3");

        let c1 = rows
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("c1 Int32");
        let c3 = rows
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("c3 Int64");
        // Every (c1, c3) pair must match what we appended (c2 is dropped).
        let expected: HashMap<i32, i64> = [(1, 100), (2, 200), (3, 300)].into();
        for i in 0..rows.num_rows() {
            assert_eq!(
                expected.get(&c1.value(i)),
                Some(&c3.value(i)),
                "projected row ({}, {}) does not match appended data",
                c1.value(i),
                c3.value(i)
            );
        }
    }

    #[tokio::test]
    async fn batch_scanner_reads_old_arrow_batches_after_add_column() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_batch_scanner_add_column");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("id", DataTypes::int())
                    .column("name", DataTypes::string())
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["id".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;
        wait_for_table_ready(&admin, &table_path).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let writer = table
            .new_append()
            .expect("append")
            .create_writer()
            .expect("writer");
        writer
            .append_arrow_batch(
                record_batch!(("id", Int32, [1, 2]), ("name", Utf8, ["alice", "bob"]))
                    .expect("old-schema batch"),
            )
            .expect("append old-schema batch");
        writer.flush().await.expect("flush");

        let age_type_json = serde_json::to_vec(
            &DataTypes::int()
                .serialize_json()
                .expect("serialize INT type"),
        )
        .expect("serialize data type JSON");
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
            .expect("add age column");

        let current_table = connection
            .get_table(&table_path)
            .await
            .expect("updated table");
        let table_info = current_table.get_table_info();
        let mut scanner = current_table
            .new_scan()
            .limit(10)
            .expect("limit")
            .create_bucket_batch_scanner(TableBucket::new(table_info.table_id, 0))
            .expect("create batch scanner");

        let scan_batch = scanner
            .next_batch()
            .await
            .expect("scan old-schema batch")
            .expect("batch");
        let batch = scan_batch.batch();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 3);
        assert_eq!(batch.schema().field(2).name(), "age");
        assert_eq!(batch.column(2).null_count(), 2);
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("id column");
        let names = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("name column");
        let ages = batch
            .column(2)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("age column");
        assert_eq!(ids.values(), &[1, 2]);
        assert_eq!(names.value(0), "alice");
        assert_eq!(names.value(1), "bob");
        assert!(ages.is_null(0));
        assert!(ages.is_null(1));
    }

    /// Limit scan on a primary-key table: decodes the value-record batch and
    /// honors the limit. Exercises the KV wire path (distinct from the log one).
    #[tokio::test]
    async fn batch_scanner_reads_primary_key_table() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_batch_scanner_pk");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("id", DataTypes::int())
                    .column("name", DataTypes::string())
                    .primary_key(vec!["id"])
                    .unwrap()
                    .build()
                    .expect("schema"),
            )
            // Single bucket so one BatchScanner sees every row.
            .distributed_by(Some(1), vec!["id".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let writer = table
            .new_upsert()
            .expect("upsert")
            .create_writer()
            .expect("writer");

        let expected: HashMap<i32, &str> =
            [(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")].into();
        for (id, name) in &expected {
            let mut row = GenericRow::new(2);
            row.set_field(0, *id);
            row.set_field(1, *name);
            writer.upsert(&row).expect("upsert row");
        }
        writer.flush().await.expect("flush");

        let table_info = table.get_table_info();
        let bucket = TableBucket::new(table_info.table_id, 0);

        let mut scanner = table
            .new_scan()
            .limit(3)
            .expect("limit")
            .create_bucket_batch_scanner(bucket.clone())
            .expect("create batch scanner");

        let first = scanner
            .next_batch()
            .await
            .expect("poll")
            .expect("first batch should be Some");

        assert_eq!(first.bucket(), &bucket);
        let rows = first.batch();
        assert_eq!(rows.num_columns(), 2, "id + name");
        assert!(
            rows.num_rows() > 0 && rows.num_rows() <= 3,
            "expected 1..=3 records, got {}",
            rows.num_rows()
        );

        // Every returned (id, name) must match what we upserted.
        let ids = rows
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("id column Int32");
        let names = rows
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("name column Utf8");
        for i in 0..rows.num_rows() {
            let id = ids.value(i);
            let name = names.value(i);
            assert_eq!(
                expected.get(&id),
                Some(&name),
                "decoded row ({id}, {name}) does not match upserted data"
            );
        }

        assert!(
            scanner.next_batch().await.expect("poll").is_none(),
            "scanner must end after one batch"
        );
    }

    /// A bucket with the wrong table_id or an out-of-range bucket_id must be
    /// rejected before any RPC is made.
    #[tokio::test]
    async fn batch_scanner_rejects_invalid_bucket() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_batch_scanner_table_id");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("c1", DataTypes::int())
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["c1".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let table_id = table.get_table_info().table_id;

        // Wrong table_id.
        assert!(
            table
                .new_scan()
                .limit(1)
                .expect("limit")
                .create_bucket_batch_scanner(TableBucket::new(table_id + 9999, 0))
                .is_err(),
            "must reject mismatched table_id"
        );

        // Bucket id past the single bucket of this table.
        assert!(
            table
                .new_scan()
                .limit(1)
                .expect("limit")
                .create_bucket_batch_scanner(TableBucket::new(table_id, 99))
                .is_err(),
            "must reject out-of-range bucket_id"
        );
    }

    /// A limit scan over a non-ARROW log table must be rejected (the log path
    /// decodes Arrow IPC).
    #[tokio::test]
    async fn batch_scanner_rejects_non_arrow_log_format() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_batch_scanner_indexed");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("c1", DataTypes::int())
                    .build()
                    .expect("schema"),
            )
            .log_format(LogFormat::INDEXED)
            .distributed_by(Some(1), vec!["c1".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let bucket = TableBucket::new(table.get_table_info().table_id, 0);

        assert!(
            table
                .new_scan()
                .limit(1)
                .expect("limit")
                .create_bucket_batch_scanner(bucket)
                .is_err(),
            "must reject INDEXED log format"
        );
    }

    /// `.limit(n)` must reject non-positive values before any scanner is built.
    #[tokio::test]
    async fn batch_scanner_rejects_non_positive_limit() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_batch_scanner_bad_limit");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("c1", DataTypes::int())
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["c1".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        assert!(table.new_scan().limit(0).is_err());
        assert!(table.new_scan().limit(-5).is_err());
    }

    /// A configured limit must be rejected by the log scanners rather than
    /// silently ignored.
    #[tokio::test]
    async fn limit_is_rejected_by_log_scanners() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_batch_scanner_limit_logscan");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("c1", DataTypes::int())
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["c1".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        assert!(
            table
                .new_scan()
                .limit(5)
                .expect("limit")
                .create_log_scanner()
                .is_err(),
            "create_log_scanner must reject a configured limit"
        );
        assert!(
            table
                .new_scan()
                .limit(5)
                .expect("limit")
                .create_record_batch_log_scanner()
                .is_err(),
            "create_record_batch_log_scanner must reject a configured limit"
        );
    }

    // ---- full KV scan (ScanKv) ---------------------------------------------

    /// `ScanKv` (ApiKey 1061) only exists on Fluss 1.x. The 0.9.x test image
    /// negotiates it away, so the first RPC fails with `UnsupportedVersion`.
    /// Treat that as a skip: these tests are no-ops on 0.9.x and fully
    /// exercised on 1.x (mirroring the Java `TableKvScanITCase` coverage).
    fn scan_kv_unsupported(err: &Error) -> bool {
        matches!(err, Error::UnsupportedVersion { .. })
    }

    /// Collect every (id, name) pair across all batches, asserting each key is
    /// seen exactly once — a full KV scan returns merged state, not a changelog.
    fn collect_id_name(batches: &[fluss::record::ScanBatch]) -> HashMap<i32, String> {
        let mut seen: HashMap<i32, String> = HashMap::new();
        for scan_batch in batches {
            let rows = scan_batch.batch();
            let ids = rows
                .column(0)
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("id column Int32");
            let names = rows
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("name column Utf8");
            for i in 0..rows.num_rows() {
                let prev = seen.insert(ids.value(i), names.value(i).to_string());
                assert!(
                    prev.is_none(),
                    "key {} returned more than once",
                    ids.value(i)
                );
            }
        }
        seen
    }

    /// Full scan of a single primary-key bucket returns every live key once.
    #[tokio::test]
    async fn kv_bucket_scanner_reads_all_live_rows() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_kv_scan_bucket");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("id", DataTypes::int())
                    .column("name", DataTypes::string())
                    .primary_key(vec!["id"])
                    .build()
                    .expect("schema"),
            )
            // Single bucket so one scanner sees every key.
            .distributed_by(Some(1), vec!["id".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;
        wait_for_table_ready(&admin, &table_path).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let writer = table
            .new_upsert()
            .expect("upsert")
            .create_writer()
            .expect("writer");

        let expected: HashMap<i32, String> = (1..=5).map(|i| (i, format!("name-{i}"))).collect();
        for (id, name) in &expected {
            let mut row = GenericRow::new(2);
            row.set_field(0, *id);
            row.set_field(1, name.as_str());
            writer.upsert(&row).expect("upsert row");
        }
        writer.flush().await.expect("flush");

        let table_info = table.get_table_info();
        let bucket = TableBucket::new(table_info.table_id, 0);

        let mut scanner = table
            .new_scan()
            .create_bucket_kv_scanner(bucket.clone())
            .expect("create kv bucket scanner");

        let batches = match scanner.collect_all_batches().await {
            Ok(batches) => batches,
            Err(e) if scan_kv_unsupported(&e) => return,
            Err(e) => panic!("unexpected kv scan error: {e}"),
        };

        for scan_batch in &batches {
            assert_eq!(scan_batch.bucket(), &bucket);
        }
        let seen = collect_id_name(&batches);
        assert_eq!(seen, expected, "bucket scan must return every key once");
    }

    /// Whole-table scan visits every bucket and returns every live key once.
    #[tokio::test]
    async fn kv_whole_table_scanner_covers_all_buckets() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_kv_scan_whole_table");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("id", DataTypes::int())
                    .column("name", DataTypes::string())
                    .primary_key(vec!["id"])
                    .build()
                    .expect("schema"),
            )
            // Multiple buckets so the whole-table scan must visit each one.
            .distributed_by(Some(3), vec!["id".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;
        wait_for_table_buckets_ready(&admin, &table_path, &[0, 1, 2]).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let writer = table
            .new_upsert()
            .expect("upsert")
            .create_writer()
            .expect("writer");

        let expected: HashMap<i32, String> = (1..=20).map(|i| (i, format!("name-{i}"))).collect();
        for (id, name) in &expected {
            let mut row = GenericRow::new(2);
            row.set_field(0, *id);
            row.set_field(1, name.as_str());
            writer.upsert(&row).expect("upsert row");
        }
        writer.flush().await.expect("flush");

        let mut scanner = table
            .new_scan()
            .create_kv_scanner()
            .expect("create whole-table kv scanner");

        let batches = match scanner.collect_all_batches().await {
            Ok(batches) => batches,
            Err(e) if scan_kv_unsupported(&e) => return,
            Err(e) => panic!("unexpected kv scan error: {e}"),
        };

        let seen = collect_id_name(&batches);
        assert_eq!(
            seen, expected,
            "whole-table scan must return every key once"
        );
    }

    /// A primary-key table with no rows yields no rows (an empty scan).
    #[tokio::test]
    async fn kv_scanner_empty_table_returns_no_rows() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_kv_scan_empty");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("id", DataTypes::int())
                    .column("name", DataTypes::string())
                    .primary_key(vec!["id"])
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["id".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;
        wait_for_table_ready(&admin, &table_path).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let mut scanner = table
            .new_scan()
            .create_kv_scanner()
            .expect("create whole-table kv scanner");

        let batches = match scanner.collect_all_batches().await {
            Ok(batches) => batches,
            Err(e) if scan_kv_unsupported(&e) => return,
            Err(e) => panic!("unexpected kv scan error: {e}"),
        };

        let total: usize = batches.iter().map(|b| b.batch().num_rows()).sum();
        assert_eq!(total, 0, "empty table must yield no rows");
    }

    /// Projection is applied client-side, dropping the middle string column.
    #[tokio::test]
    async fn kv_bucket_scanner_applies_projection() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_kv_scan_projection");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("id", DataTypes::int())
                    .column("name", DataTypes::string())
                    .column("age", DataTypes::bigint())
                    .primary_key(vec!["id"])
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["id".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;
        wait_for_table_ready(&admin, &table_path).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let writer = table
            .new_upsert()
            .expect("upsert")
            .create_writer()
            .expect("writer");

        let expected: HashMap<i32, i64> = [(1, 30i64), (2, 40), (3, 50)].into();
        for (id, age) in &expected {
            let mut row = GenericRow::new(3);
            row.set_field(0, *id);
            row.set_field(1, format!("name-{id}").as_str());
            row.set_field(2, *age);
            writer.upsert(&row).expect("upsert row");
        }
        writer.flush().await.expect("flush");

        let bucket = TableBucket::new(table.get_table_info().table_id, 0);
        let mut scanner = table
            .new_scan()
            .project(&[0, 2])
            .expect("project")
            .create_bucket_kv_scanner(bucket)
            .expect("create kv bucket scanner");

        let batches = match scanner.collect_all_batches().await {
            Ok(batches) => batches,
            Err(e) if scan_kv_unsupported(&e) => return,
            Err(e) => panic!("unexpected kv scan error: {e}"),
        };

        let mut seen: HashMap<i32, i64> = HashMap::new();
        for scan_batch in &batches {
            let rows = scan_batch.batch();
            assert_eq!(rows.num_columns(), 2, "projected to id + age");
            assert_eq!(rows.schema().field(0).name(), "id");
            assert_eq!(rows.schema().field(1).name(), "age");
            let ids = rows
                .column(0)
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("id Int32");
            let ages = rows
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("age Int64");
            for i in 0..rows.num_rows() {
                let prev = seen.insert(ids.value(i), ages.value(i));
                assert!(prev.is_none(), "key {} returned twice", ids.value(i));
            }
        }
        assert_eq!(seen, expected, "projected (id, age) pairs must match");
    }

    /// A full KV scan on a log (non-PK) table must be rejected before any RPC.
    #[tokio::test]
    async fn kv_scanner_rejects_log_table() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_kv_scan_reject_log");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("c1", DataTypes::int())
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["c1".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let bucket = TableBucket::new(table.get_table_info().table_id, 0);

        assert!(
            table.new_scan().create_kv_scanner().is_err(),
            "whole-table kv scan must reject a log table"
        );
        assert!(
            table.new_scan().create_bucket_kv_scanner(bucket).is_err(),
            "bucket kv scan must reject a log table"
        );
    }

    /// A configured limit must be rejected by both KV scan entry points.
    #[tokio::test]
    async fn kv_scanner_rejects_limit() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_kv_scan_reject_limit");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("id", DataTypes::int())
                    .column("name", DataTypes::string())
                    .primary_key(vec!["id"])
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["id".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let bucket = TableBucket::new(table.get_table_info().table_id, 0);

        assert!(
            table
                .new_scan()
                .limit(5)
                .expect("limit")
                .create_kv_scanner()
                .is_err(),
            "whole-table kv scan must reject a configured limit"
        );
        assert!(
            table
                .new_scan()
                .limit(5)
                .expect("limit")
                .create_bucket_kv_scanner(bucket)
                .is_err(),
            "bucket kv scan must reject a configured limit"
        );
    }

    /// A bucket with the wrong table_id or an out-of-range bucket_id must be
    /// rejected before any RPC is made.
    #[tokio::test]
    async fn kv_bucket_scanner_rejects_invalid_bucket() {
        let cluster = get_shared_cluster();
        let connection = cluster.get_fluss_connection().await;
        let admin = connection.get_admin().expect("admin");

        let table_path = TablePath::new("fluss", "test_kv_scan_reject_bucket");
        let descriptor = TableDescriptor::builder()
            .schema(
                Schema::builder()
                    .column("id", DataTypes::int())
                    .column("name", DataTypes::string())
                    .primary_key(vec!["id"])
                    .build()
                    .expect("schema"),
            )
            .distributed_by(Some(1), vec!["id".to_string()])
            .build()
            .expect("descriptor");
        create_table(&admin, &table_path, &descriptor).await;

        let table = connection.get_table(&table_path).await.expect("table");
        let table_id = table.get_table_info().table_id;

        assert!(
            table
                .new_scan()
                .create_bucket_kv_scanner(TableBucket::new(table_id + 9999, 0))
                .is_err(),
            "must reject mismatched table_id"
        );
        assert!(
            table
                .new_scan()
                .create_bucket_kv_scanner(TableBucket::new(table_id, 99))
                .is_err(),
            "must reject out-of-range bucket_id"
        );
    }
}
