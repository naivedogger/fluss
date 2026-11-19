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

//! Imports engine-owned append scan ranges without recapturing source offsets.

use crate::{AbiError, Client, ErrorCode, Property, child_cancel, status, stream, text, wait_for};
use arrow::ffi_stream::FFI_ArrowArrayStream;
use fluss::client::{BoundedLogReadRange, RecordBatchLogReader};
use fluss::metadata::{TableBucket, TablePath};
use fluss_lake::{FlussLakeReadContext, FlussLakeTable};
use futures::StreamExt;
use std::collections::HashMap;
use std::ffi::{c_char, c_int};
use std::ptr;
use tokio_util::sync::CancellationToken;

#[repr(C)]
struct AppendTask {
    table_id: i64,
    schema_id: i32,
    modified_time: i64,
    snapshot_id: i64,
    partition_id: i64,
    bucket_id: i32,
    start_offset: i64,
    stop_offset: i64,
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_append_open(
    client: *const Client,
    database: *const c_char,
    table: *const c_char,
    task: *const AppendTask,
    properties: *const Property,
    property_count: usize,
    out: *mut FFI_ArrowArrayStream,
    cancel: *mut *mut CancellationToken,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        fluss_union_append_open_cancellable(
            client,
            database,
            table,
            task,
            properties,
            property_count,
            ptr::null(),
            out,
            cancel,
            error,
        )
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_append_open_cancellable(
    client: *const Client,
    database: *const c_char,
    table: *const c_char,
    task: *const AppendTask,
    properties: *const Property,
    property_count: usize,
    operation_cancel: *const CancellationToken,
    out: *mut FFI_ArrowArrayStream,
    cancel: *mut *mut CancellationToken,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        status(error, || {
            if out.is_null() || cancel.is_null() {
                return Err(AbiError::invalid("stream output is null"));
            }
            out.write(FFI_ArrowArrayStream::empty());
            cancel.write(ptr::null_mut());
            let client = client
                .as_ref()
                .ok_or_else(|| AbiError::invalid("client is null"))?;
            let task = task
                .as_ref()
                .ok_or_else(|| AbiError::invalid("append task is null"))?;
            let path = TablePath::new(text(database)?, text(table)?);
            let mut overrides = HashMap::new();
            if property_count != 0 {
                if properties.is_null() {
                    return Err(AbiError::invalid("properties are null"));
                }
                for property in std::slice::from_raw_parts(properties, property_count) {
                    overrides.insert(text(property.key)?, text(property.value)?);
                }
            }
            let token = child_cancel(operation_cancel);
            let output = wait_for(&client.runtime, &token, async {
                let table = client.connection.get_table(&path).await?;
                let info = table.get_table_info();
                if info.table_id != task.table_id
                    || info.schema_id != task.schema_id
                    || info.modified_time != task.modified_time
                {
                    return Err(AbiError::new(
                        ErrorCode::SchemaIncompatible,
                        "table identity or metadata changed after FE planning",
                    ));
                }
                if info.has_primary_key() {
                    return Err(AbiError::invalid(
                        "append tasks cannot read primary-key tables",
                    ));
                }
                if task.snapshot_id >= 0 {
                    // One engine task owns the complete snapshot. Workers never select
                    // an ordinal from an independently regenerated file plan.
                    if !info.partition_keys.is_empty() {
                        return Err(AbiError::invalid(
                            "snapshot tasks currently require an unpartitioned table",
                        ));
                    }
                    let context = FlussLakeReadContext::from_lake_snapshot(
                        info,
                        Some(task.snapshot_id),
                        None,
                    )?;
                    let scan = FlussLakeTable::new(client.connection.clone(), &path)
                        .with_catalog_properties(overrides)
                        .new_scan()
                        .with_lake_only(true);
                    let plan = scan.plan_with_context(&context).await?;
                    let input = scan.new_reader().read_splits(plan.splits())?;
                    Ok(stream::export(
                        input,
                        plan.schema(),
                        client.runtime.clone(),
                        token.clone(),
                    ))
                } else {
                    if task.snapshot_id != -1
                        || task.partition_id < -1
                        || task.bucket_id < 0
                        || task.start_offset < 0
                        || task.stop_offset < task.start_offset
                    {
                        return Err(AbiError::invalid("invalid frozen log range"));
                    }
                    if info.partition_keys.is_empty() != (task.partition_id == -1) {
                        return Err(AbiError::invalid(
                            "log range partition does not match table layout",
                        ));
                    }
                    let schema = fluss::record::to_arrow_schema(info.row_type())?;
                    if task.start_offset == task.stop_offset {
                        return Ok(stream::export(
                            futures::stream::empty::<
                                Result<arrow::record_batch::RecordBatch, AbiError>,
                            >(),
                            schema,
                            client.runtime.clone(),
                            token.clone(),
                        ));
                    }
                    let scanner = table.new_scan().create_record_batch_log_scanner()?;
                    let bucket = TableBucket::new_with_partition(
                        task.table_id,
                        (task.partition_id >= 0).then_some(task.partition_id),
                        task.bucket_id,
                    );
                    let reader = RecordBatchLogReader::new_from_ranges(
                        scanner,
                        vec![BoundedLogReadRange {
                            bucket,
                            starting_offset: task.start_offset,
                            stopping_offset: task.stop_offset,
                        }],
                    )
                    .await?;
                    Ok(stream::export(
                        reader
                            .into_stream()
                            .map(|batch| batch.map(|batch| batch.into_batch())),
                        schema,
                        client.runtime.clone(),
                        token.clone(),
                    ))
                }
            })?;
            cancel.write(Box::into_raw(Box::new(token)));
            out.write(output);
            Ok(())
        })
    }
}
