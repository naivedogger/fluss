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

use crate::task::{AppendLogTaskDescriptor, LakeSplitTaskDescriptor, TaskDescriptor};
use crate::{
    SendableRecordBatchStream, UnionReadError, UnionReadExecutionContext, UnionReadExecutionFuture,
    UnionReadExecutor, UnionReadResult, UnionReadTask,
};
use arrow::record_batch::RecordBatch;
use fluss::client::RecordBatchLogReader;
use fluss::error::Error as ClientError;
use futures::StreamExt;
use std::collections::HashMap;

/// Default Fluss-Rust executor for opaque UnionRead tasks.
#[derive(Debug, Clone, Copy, Default)]
pub struct FlussUnionReadExecutor;

impl UnionReadExecutor for FlussUnionReadExecutor {
    fn execute(
        &self,
        task: UnionReadTask,
        context: UnionReadExecutionContext,
    ) -> UnionReadExecutionFuture<'_> {
        Box::pin(async move { execute_task(task, context).await })
    }
}

async fn execute_task(
    task: UnionReadTask,
    context: UnionReadExecutionContext,
) -> UnionReadResult<SendableRecordBatchStream> {
    match TaskDescriptor::decode(task.execution_descriptor())? {
        TaskDescriptor::AppendLog(descriptor) => execute_append_log(descriptor, context).await,
        TaskDescriptor::LakeSplit(descriptor) => execute_lake_split(descriptor).await,
    }
}

/// Reads one frozen lake split.
///
/// Lake splits are decodable in every build so that a task planned elsewhere
/// reports a clear error here instead of failing to parse. Reading requires the
/// matching lake format feature to be compiled in.
#[cfg(feature = "paimon")]
async fn execute_lake_split(
    descriptor: LakeSplitTaskDescriptor,
) -> UnionReadResult<SendableRecordBatchStream> {
    crate::paimon::read_snapshot_split(
        descriptor.table_path(),
        &crate::paimon::PaimonCatalogOptions::from_map(
            descriptor
                .catalog_options()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        descriptor.snapshot_id(),
        descriptor.projected_fields(),
        descriptor.encoded_split(),
    )
    .await
}

#[cfg(not(feature = "paimon"))]
async fn execute_lake_split(
    descriptor: LakeSplitTaskDescriptor,
) -> UnionReadResult<SendableRecordBatchStream> {
    Err(UnionReadError::Execution(format!(
        "lake split task for {} cannot be executed: this build has no lake format feature enabled",
        descriptor.table_path()
    )))
}

async fn execute_append_log(
    descriptor: AppendLogTaskDescriptor,
    context: UnionReadExecutionContext,
) -> UnionReadResult<SendableRecordBatchStream> {
    if descriptor.is_empty() {
        return Ok(Box::pin(futures::stream::empty::<
            UnionReadResult<RecordBatch>,
        >()));
    }

    let connection = context.fluss_connection().cloned().ok_or_else(|| {
        UnionReadError::Execution(
            "append-log task requires a Fluss connection in the execution context".to_string(),
        )
    })?;
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
        let mut stream = futures::executor::block_on(executor.execute(
            append_log_task(10, 10),
            UnionReadExecutionContext::default(),
        ))
        .unwrap();

        assert!(futures::executor::block_on(stream.next()).is_none());
    }

    #[test]
    fn non_empty_append_log_task_requires_runtime_connection() {
        let executor = FlussUnionReadExecutor;
        let result = futures::executor::block_on(executor.execute(
            append_log_task(10, 11),
            UnionReadExecutionContext::default(),
        ));

        assert!(matches!(result, Err(UnionReadError::Execution(_))));
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

        let result = futures::executor::block_on(
            FlussUnionReadExecutor.execute(task, UnionReadExecutionContext::default()),
        );

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
