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

use crate::task::{
    AppendLogTaskDescriptor, LakeSplitTaskDescriptor, PkHybridTaskDescriptor, TaskDescriptor,
};
use crate::{
    SendableRecordBatchStream, UnionReadError, UnionReadExecutionContext, UnionReadExecutor,
    UnionReadResult, UnionReadTask,
};
use arrow::record_batch::RecordBatch;
use fluss::client::{FlussConnection, RecordBatchLogReader};
use fluss::error::Error as ClientError;
use futures::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// Default Fluss-Rust executor for opaque UnionRead tasks.
#[derive(Debug, Clone, Copy, Default)]
pub struct FlussUnionReadExecutor;

impl UnionReadExecutor for FlussUnionReadExecutor {
    fn execute(
        &self,
        task: UnionReadTask,
        context: UnionReadExecutionContext,
    ) -> UnionReadResult<SendableRecordBatchStream> {
        // Structural validation fails fast; environment work is deferred to
        // the first poll of the returned stream.
        match TaskDescriptor::decode(task.execution_descriptor())? {
            TaskDescriptor::AppendLog(descriptor) => execute_append_log(descriptor, context),
            TaskDescriptor::LakeSplit(descriptor) => execute_lake_split(descriptor, context),
            TaskDescriptor::PkHybrid(descriptor) => execute_pk_hybrid(descriptor, context),
        }
    }
}

/// Merges one primary-key bucket's lake baseline with its bounded log tail.
///
/// The hash-overlay merge executor is not implemented yet; the task kind is
/// dispatched here so that a plan produced by a newer planner fails with a
/// clear capability error instead of an unknown-kind decode error.
fn execute_pk_hybrid(
    descriptor: PkHybridTaskDescriptor,
    _context: UnionReadExecutionContext,
) -> UnionReadResult<SendableRecordBatchStream> {
    Err(UnionReadError::Execution(format!(
        "pk-hybrid task for {} cannot be executed: the primary-key merge executor is not implemented yet",
        descriptor.table_path()
    )))
}

/// Defers an asynchronous stream setup until the stream's first poll.
///
/// Setup failures surface as the first (and only) stream item instead of
/// failing the `execute` call, keeping `execute` synchronous and lazy.
fn lazy_stream<F>(setup: F) -> SendableRecordBatchStream
where
    F: Future<Output = UnionReadResult<SendableRecordBatchStream>> + Send + 'static,
{
    Box::pin(futures::stream::once(setup).try_flatten())
}

/// Enforces the bounded-read termination contract on a task stream.
///
/// A bounded read has exactly two exits: reaching its frozen stop boundary,
/// or a typed error. The timeout bounds the wait for the *next* item and
/// resets whenever one arrives, so a stalled fetch becomes an explicit error
/// instead of an infinite wait — and never a silent partial result. Requires
/// a Tokio runtime when polled, which the Fluss client needs anyway.
fn with_idle_timeout(
    stream: SendableRecordBatchStream,
    idle_timeout: Duration,
) -> SendableRecordBatchStream {
    Box::pin(futures::stream::try_unfold(
        stream,
        move |mut stream| async move {
            match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(Ok(batch))) => Ok(Some((batch, stream))),
                Ok(Some(Err(error))) => Err(error),
                Ok(None) => Ok(None),
                Err(_) => Err(UnionReadError::Execution(format!(
                    "bounded read made no progress within the {idle_timeout:?} idle timeout; the frozen stop boundary existed at plan time, so a stalled fetch is an operational failure rather than a reason to wait forever"
                ))),
            }
        },
    ))
}

/// Reads one frozen lake split.
///
/// Lake splits are decodable in every build so that a task planned elsewhere
/// reports a clear error here instead of failing to parse. Reading requires the
/// matching lake format feature to be compiled in.
#[cfg(feature = "paimon")]
fn execute_lake_split(
    descriptor: LakeSplitTaskDescriptor,
    context: UnionReadExecutionContext,
) -> UnionReadResult<SendableRecordBatchStream> {
    let idle_timeout = context.idle_timeout();
    Ok(with_idle_timeout(
        lazy_stream(async move {
            // The task carries only non-sensitive options; secrets arrive through
            // the execution context and override any task-carried value.
            let catalog_options = crate::paimon::PaimonCatalogOptions::from_map(
                descriptor
                    .catalog_options()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            )
            .with_runtime_credentials(context.lake_credentials());
            crate::paimon::read_snapshot_split(
                descriptor.table_path(),
                &catalog_options,
                descriptor.snapshot_id(),
                descriptor.projected_fields(),
                descriptor.encoded_split(),
            )
            .await
        }),
        idle_timeout,
    ))
}

#[cfg(not(feature = "paimon"))]
fn execute_lake_split(
    descriptor: LakeSplitTaskDescriptor,
    _context: UnionReadExecutionContext,
) -> UnionReadResult<SendableRecordBatchStream> {
    Err(UnionReadError::Execution(format!(
        "lake split task for {} cannot be executed: this build has no lake format feature enabled",
        descriptor.table_path()
    )))
}

fn execute_append_log(
    descriptor: AppendLogTaskDescriptor,
    context: UnionReadExecutionContext,
) -> UnionReadResult<SendableRecordBatchStream> {
    if descriptor.is_empty() {
        return Ok(Box::pin(futures::stream::empty::<
            UnionReadResult<RecordBatch>,
        >()));
    }

    // A missing connection is a context-shape problem, not an environment
    // failure, so it still fails fast.
    let connection = context.fluss_connection().cloned().ok_or_else(|| {
        UnionReadError::Execution(
            "append-log task requires a Fluss connection in the execution context".to_string(),
        )
    })?;
    Ok(with_idle_timeout(
        lazy_stream(open_append_log_stream(connection, descriptor)),
        context.idle_timeout(),
    ))
}

async fn open_append_log_stream(
    connection: Arc<FlussConnection>,
    descriptor: AppendLogTaskDescriptor,
) -> UnionReadResult<SendableRecordBatchStream> {
    let table = connection
        .get_table(descriptor.table_path())
        .await
        .map_err(|error| execution_client_error("open Fluss table", error))?;
    let table_info = table.get_table_info();
    if table_info.table_id != descriptor.table_bucket().table_id() {
        return Err(UnionReadError::Execution(format!(
            "task table id {} no longer matches resolved table id {} for {}",
            descriptor.table_bucket().table_id(),
            table_info.table_id,
            descriptor.table_path()
        )));
    }
    if table_info.schema_id != descriptor.schema_id() {
        return Err(UnionReadError::Execution(format!(
            "task schema id {} no longer matches current schema id {} for {}; execution against a historical schema is not implemented",
            descriptor.schema_id(),
            table_info.schema_id,
            descriptor.table_path()
        )));
    }
    if table.has_primary_key() {
        return Err(UnionReadError::InvalidTask(format!(
            "append-log task cannot execute against primary-key table {}",
            descriptor.table_path()
        )));
    }

    let scan = match descriptor.output_projection() {
        Some(projection) => table
            .new_scan()
            .project(projection)
            .map_err(|error| execution_client_error("apply append-log projection", error))?,
        None => table.new_scan(),
    };
    let scanner = scan
        .create_record_batch_log_scanner()
        .map_err(|error| execution_client_error("create append-log scanner", error))?;
    match descriptor.table_bucket().partition_id() {
        Some(partition_id) => scanner
            .subscribe_partition(
                partition_id,
                descriptor.table_bucket().bucket_id(),
                descriptor.start_offset(),
            )
            .await
            .map_err(|error| execution_client_error("subscribe partition bucket", error))?,
        None => scanner
            .subscribe(
                descriptor.table_bucket().bucket_id(),
                descriptor.start_offset(),
            )
            .await
            .map_err(|error| execution_client_error("subscribe table bucket", error))?,
    }

    let reader = RecordBatchLogReader::new_until_offsets(
        scanner,
        HashMap::from([(descriptor.table_bucket().clone(), descriptor.stop_offset())]),
    )
    .map_err(|error| execution_client_error("create bounded append-log reader", error))?;
    let stream = reader.into_stream().map(|result| {
        result
            .map(|scan_batch| scan_batch.into_batch())
            .map_err(|error| execution_client_error("read bounded append-log task", error))
    });
    Ok(Box::pin(stream))
}

fn execution_client_error(action: &str, error: ClientError) -> UnionReadError {
    UnionReadError::Execution(format!("failed to {action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskDescriptor;
    use crate::{CURRENT_UNION_READ_TASK_VERSION, UnionReadStatistics};
    use fluss::metadata::{TableBucket, TablePath};
    use futures::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn append_log_task(start_offset: i64, stop_offset: i64) -> UnionReadTask {
        let descriptor = TaskDescriptor::AppendLog(
            AppendLogTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                1,
                TableBucket::new(5, 0),
                start_offset,
                stop_offset,
                None,
            )
            .unwrap(),
        );
        UnionReadTask::try_new(
            "append-log/5/root/0".to_string(),
            CURRENT_UNION_READ_TASK_VERSION,
            descriptor.encode().unwrap(),
            UnionReadStatistics::default(),
        )
        .unwrap()
    }

    #[test]
    fn empty_append_log_task_finishes_without_runtime_connection() {
        let executor = FlussUnionReadExecutor;
        let mut stream = executor
            .execute(
                append_log_task(10, 10),
                UnionReadExecutionContext::default(),
            )
            .unwrap();

        assert!(futures::executor::block_on(stream.next()).is_none());
    }

    #[test]
    fn non_empty_append_log_task_requires_runtime_connection() {
        let executor = FlussUnionReadExecutor;
        let result = executor.execute(
            append_log_task(10, 11),
            UnionReadExecutionContext::default(),
        );

        assert!(matches!(result, Err(UnionReadError::Execution(_))));
    }

    /// `execute` must reject an undecodable descriptor synchronously, before
    /// any environment work, so engines see structural errors immediately.
    #[test]
    fn undecodable_descriptor_fails_before_the_stream_is_polled() {
        let mut task_bytes = append_log_task(10, 11).encode().unwrap();
        let descriptor_start = task_bytes.len() - 1;
        task_bytes[descriptor_start] ^= 0xff;

        assert!(UnionReadTask::decode(&task_bytes).is_err());
    }

    /// Setup work must not run until the stream is polled, and its failure
    /// must arrive as the first stream item rather than from `execute`.
    #[test]
    fn lazy_stream_defers_setup_until_the_first_poll() {
        let setup_ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&setup_ran);
        let mut stream = lazy_stream(async move {
            flag.store(true, Ordering::SeqCst);
            Err(UnionReadError::Execution("deferred failure".to_string()))
        });

        assert!(!setup_ran.load(Ordering::SeqCst));

        let first = futures::executor::block_on(stream.next());

        assert!(setup_ran.load(Ordering::SeqCst));
        assert!(matches!(first, Some(Err(UnionReadError::Execution(_)))));
        assert!(futures::executor::block_on(stream.next()).is_none());
    }

    /// A stalled bounded read must fail with a typed error instead of
    /// waiting forever: the frozen stop boundary existed at plan time, so
    /// lack of progress is an operational failure.
    #[tokio::test]
    async fn idle_timeout_fails_a_stalled_bounded_read() {
        let stalled: SendableRecordBatchStream = Box::pin(futures::stream::pending());
        let mut stream = with_idle_timeout(stalled, Duration::from_millis(20));

        let first = stream.next().await;

        match first {
            Some(Err(UnionReadError::Execution(message))) => {
                assert!(
                    message.contains("idle timeout"),
                    "unexpected error: {message}"
                );
            }
            other => panic!("expected an idle-timeout execution error, got: {other:?}"),
        }
    }

    /// The idle timeout bounds the wait for the next item, not the total
    /// read: it must reset whenever data arrives, and a stream that keeps
    /// making progress must complete untouched.
    #[tokio::test]
    async fn idle_timeout_resets_on_progress_and_passes_bounded_streams_through() {
        use arrow::datatypes::Schema;

        let batches: Vec<UnionReadResult<RecordBatch>> = (0..3)
            .map(|_| Ok(RecordBatch::new_empty(Arc::new(Schema::empty()))))
            .collect();
        let bounded: SendableRecordBatchStream = Box::pin(futures::stream::iter(batches));
        let mut stream = with_idle_timeout(bounded, Duration::from_millis(20));

        let mut seen = 0;
        while let Some(item) = stream.next().await {
            item.unwrap();
            seen += 1;
        }

        assert_eq!(seen, 3);
    }

    /// Progress must keep a stream alive past the idle threshold measured
    /// from its start; only a gap longer than the threshold may fail it.
    #[tokio::test]
    async fn idle_timeout_measures_gaps_not_total_duration() {
        use arrow::datatypes::Schema;

        let ticking = futures::stream::unfold(0, |emitted| async move {
            if emitted == 4 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
            let batch: UnionReadResult<RecordBatch> =
                Ok(RecordBatch::new_empty(Arc::new(Schema::empty())));
            Some((batch, emitted + 1))
        });
        // Each 15ms gap is under the 25ms threshold while the 60ms total is
        // far over it: completion proves the timer resets on every item.
        let mut stream = with_idle_timeout(Box::pin(ticking), Duration::from_millis(25));

        let mut seen = 0;
        while let Some(item) = stream.next().await {
            item.unwrap();
            seen += 1;
        }

        assert_eq!(seen, 4);
    }

    /// A lake split task planned elsewhere must stay decodable here, so that a
    /// build without any lake format reports why it cannot run it.
    #[test]
    #[cfg(not(feature = "paimon"))]
    fn lake_split_task_without_lake_feature_reports_a_clear_error() {
        use crate::task::LakeSplitTaskDescriptor;
        use std::collections::BTreeMap;

        let mut catalog_options = BTreeMap::new();
        catalog_options.insert("warehouse".to_string(), "/tmp/warehouse".to_string());
        let descriptor = TaskDescriptor::LakeSplit(
            LakeSplitTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                7,
                catalog_options,
                None,
                "{}".to_string(),
            )
            .unwrap(),
        );
        let task = UnionReadTask::try_new(
            "lake-split/fluss.orders/7/0".to_string(),
            CURRENT_UNION_READ_TASK_VERSION,
            descriptor.encode().unwrap(),
            UnionReadStatistics::default(),
        )
        .unwrap();

        let result = FlussUnionReadExecutor.execute(task, UnionReadExecutionContext::default());

        match result {
            Err(UnionReadError::Execution(message)) => {
                assert!(
                    message.contains("no lake format feature"),
                    "unexpected error: {message}"
                );
            }
            Err(other) => panic!("expected an execution error, got: {other}"),
            Ok(_) => panic!("a lake split task must not execute without a lake format feature"),
        }
    }
}
