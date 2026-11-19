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

//! Imports a coordinator's complete frozen boundary, then selects a stable bucket.

use crate::{AbiError, Client, ErrorCode, Property, child_cancel, status, stream, text, wait_for};
use arrow::ffi_stream::FFI_ArrowArrayStream;
use fluss::metadata::{TableBucket, TablePath};
use fluss::rpc::message::OffsetSpec;
use fluss_lake::{
    FlussLakeLogPartition, FlussLakeLogRange, FlussLakePartitionIdentity, FlussLakeReadContext,
    FlussLakeTable,
};
use futures::{StreamExt, TryStreamExt};
use std::{
    collections::HashMap,
    ffi::{c_char, c_int},
    ptr,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[repr(C)]
#[derive(Clone, Copy)]
struct Boundary {
    table_id: i64,
    schema_id: i32,
    modified_time: i64,
    snapshot_id: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LogRange {
    bucket_id: i32,
    start_offset: i64,
    stop_offset: i64,
    has_lake_baseline: bool,
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_bucket_open(
    client: *const Client,
    database: *const c_char,
    table: *const c_char,
    boundary: *const Boundary,
    ranges: *const LogRange,
    range_count: usize,
    bucket_id: i32,
    memory_limit: usize,
    properties: *const Property,
    property_count: usize,
    out: *mut FFI_ArrowArrayStream,
    cancel: *mut *mut CancellationToken,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        fluss_union_bucket_open_with_memory(
            client,
            database,
            table,
            boundary,
            ranges,
            range_count,
            bucket_id,
            memory_limit,
            ptr::null_mut(),
            properties,
            property_count,
            out,
            cancel,
            error,
        )
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_bucket_open_with_memory(
    client: *const Client,
    database: *const c_char,
    table: *const c_char,
    boundary: *const Boundary,
    ranges: *const LogRange,
    range_count: usize,
    bucket_id: i32,
    memory_limit: usize,
    memory: *mut crate::memory::MemoryBudget,
    properties: *const Property,
    property_count: usize,
    out: *mut FFI_ArrowArrayStream,
    cancel: *mut *mut CancellationToken,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        fluss_union_bucket_open_with_baseline(
            client,
            database,
            table,
            boundary,
            ranges,
            range_count,
            bucket_id,
            memory_limit,
            memory,
            ptr::null_mut(),
            properties,
            property_count,
            out,
            cancel,
            error,
        )
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fluss_union_bucket_open_with_baseline(
    client: *const Client,
    database: *const c_char,
    table: *const c_char,
    boundary: *const Boundary,
    ranges: *const LogRange,
    range_count: usize,
    bucket_id: i32,
    memory_limit: usize,
    memory: *mut crate::memory::MemoryBudget,
    baseline: *mut FFI_ArrowArrayStream,
    properties: *const Property,
    property_count: usize,
    out: *mut FFI_ArrowArrayStream,
    cancel: *mut *mut CancellationToken,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        fluss_union_bucket_open_with_baseline_cancellable(
            client,
            database,
            table,
            boundary,
            ranges,
            range_count,
            bucket_id,
            memory_limit,
            memory,
            baseline,
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
unsafe extern "C" fn fluss_union_bucket_open_with_baseline_cancellable(
    client: *const Client,
    database: *const c_char,
    table: *const c_char,
    boundary: *const Boundary,
    ranges: *const LogRange,
    range_count: usize,
    bucket_id: i32,
    memory_limit: usize,
    memory: *mut crate::memory::MemoryBudget,
    baseline: *mut FFI_ArrowArrayStream,
    properties: *const Property,
    property_count: usize,
    operation_cancel: *const CancellationToken,
    out: *mut FFI_ArrowArrayStream,
    cancel: *mut *mut CancellationToken,
    error: *mut *mut c_char,
) -> c_int {
    unsafe {
        status(error, || {
            let memory = crate::memory::MemoryBudget::take(memory);
            #[cfg(feature = "paimon")]
            let baseline = crate::baseline::ImportedBaseline::take(baseline);
            #[cfg(not(feature = "paimon"))]
            let baseline = {
                if baseline.is_null() {
                    Ok::<(), AbiError>(())
                } else {
                    drop(FFI_ArrowArrayStream::from_raw(baseline));
                    Err(AbiError::invalid(
                        "engine baseline requires the paimon feature",
                    ))
                }
            };
            if out.is_null() || cancel.is_null() {
                return Err(AbiError::invalid("stream output is null"));
            }
            out.write(FFI_ArrowArrayStream::empty());
            cancel.write(ptr::null_mut());
            let memory = memory?;
            #[cfg(feature = "paimon")]
            let baseline = baseline?;
            #[cfg(not(feature = "paimon"))]
            baseline?;
            let client = client
                .as_ref()
                .ok_or_else(|| AbiError::invalid("client is null"))?;
            let boundary = *boundary
                .as_ref()
                .ok_or_else(|| AbiError::invalid("boundary is null"))?;
            if ranges.is_null() || range_count == 0 {
                return Err(AbiError::invalid("complete log ranges required"));
            }
            let ranges = std::slice::from_raw_parts(ranges, range_count).to_vec();
            if !ranges.iter().any(|r| r.bucket_id == bucket_id) {
                return Err(AbiError::invalid("selected bucket is absent from boundary"));
            }
            let path = TablePath::new(text(database)?, text(table)?);
            let mut overrides = HashMap::new();
            if property_count != 0 {
                if properties.is_null() {
                    return Err(AbiError::invalid("properties are null"));
                }
                for p in std::slice::from_raw_parts(properties, property_count) {
                    overrides.insert(text(p.key)?, text(p.value)?);
                }
            }
            let token = child_cancel(operation_cancel);
            let info = wait_for(&client.runtime, &token, async {
                Ok::<_, AbiError>(
                    client
                        .connection
                        .get_table(&path)
                        .await?
                        .get_table_info()
                        .clone(),
                )
            })?;
            if info.table_id != boundary.table_id
                || info.schema_id != boundary.schema_id
                || info.modified_time != boundary.modified_time
            {
                return Err(AbiError::new(
                    ErrorCode::SchemaIncompatible,
                    "table identity or metadata changed after planning",
                ));
            }
            if !info.has_primary_key() || !info.partition_keys.is_empty() {
                return Err(AbiError::invalid(
                    "bucket import currently requires an unpartitioned PK table",
                ));
            }
            if boundary.snapshot_id < -1
                || ranges
                    .iter()
                    .any(|r| r.start_offset < 0 || r.stop_offset < r.start_offset)
            {
                return Err(AbiError::invalid("invalid frozen snapshot or offsets"));
            }
            let schema = fluss::record::to_arrow_schema(info.row_type())?;
            let connection = client.connection.clone();
            let input = futures::stream::once(async move {
                let buckets = ranges.iter().map(|r| r.bucket_id).collect::<Vec<_>>();
                // Retention is checked at execution; neither the pinned snapshot nor
                // the coordinator's stopping offsets are recaptured here.
                let earliest = connection
                    .get_admin()?
                    .list_offsets(&path, &buckets, OffsetSpec::Earliest)
                    .await?;
                let mut frozen = Vec::with_capacity(ranges.len());
                for r in ranges {
                    let retained = *earliest
                        .get(&r.bucket_id)
                        .ok_or_else(|| AbiError::invalid("missing earliest offset"))?;
                    if retained > r.start_offset || (!r.has_lake_baseline && r.start_offset != 0) {
                        return Err(AbiError::new(
                            ErrorCode::DataUnavailable,
                            "frozen PK baseline or required log history is unavailable",
                        ));
                    }
                    frozen.push(FlussLakeLogRange::new(
                        TableBucket::new(info.table_id, r.bucket_id),
                        FlussLakeLogPartition::Ordinary(FlussLakePartitionIdentity::Unpartitioned),
                        info.num_buckets,
                        r.has_lake_baseline.then_some(r.start_offset),
                        r.stop_offset,
                        retained,
                    ));
                }
                let context = FlussLakeReadContext::from_boundary(
                    &info,
                    (boundary.snapshot_id >= 0).then_some(boundary.snapshot_id),
                    frozen,
                )?;
                let scan = FlussLakeTable::new(connection, &path)
                    .with_catalog_properties(overrides.clone())
                    .new_scan();
                #[cfg(feature = "paimon")]
                let scan = if let Some(baseline) = baseline {
                    scan.with_lake_source(Arc::new(
                        fluss_lake::PaimonLakeSource::new(&info, &overrides)?
                            .with_baseline_reader(Arc::new(baseline)),
                    ))
                } else {
                    scan
                };
                let selected = context.for_bucket(&TableBucket::new(info.table_id, bucket_id))?;
                let plan = scan.plan_with_context(&selected).await?;
                let mut reader = scan.new_reader();
                if let Some(memory) = memory {
                    reader = reader.with_memory_budget(Arc::new(memory));
                } else if memory_limit != 0 {
                    reader = reader.with_memory_budget(Arc::new(
                        fluss::client::LimitedReadMemoryBudget::new(memory_limit),
                    ));
                }
                Ok::<_, AbiError>(
                    reader
                        .read_splits(plan.splits())?
                        .map(|r| r.map_err(AbiError::from)),
                )
            })
            .try_flatten();
            out.write(stream::export(
                input,
                schema,
                client.runtime.clone(),
                token.clone(),
            ));
            cancel.write(Box::into_raw(Box::new(token)));
            Ok(())
        })
    }
}

#[cfg(all(test, feature = "paimon"))]
mod tests {
    use super::*;
    use arrow::{
        array::RecordBatchReader,
        datatypes::{Schema, SchemaRef},
        record_batch::RecordBatch,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn failed_open_releases_moved_engine_baseline_once() {
        struct Baseline(Arc<AtomicUsize>);
        impl Iterator for Baseline {
            type Item = std::result::Result<RecordBatch, arrow::error::ArrowError>;
            fn next(&mut self) -> Option<Self::Item> {
                None
            }
        }
        impl RecordBatchReader for Baseline {
            fn schema(&self) -> SchemaRef {
                Arc::new(Schema::empty())
            }
        }
        impl Drop for Baseline {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let released = Arc::new(AtomicUsize::new(0));
        let mut baseline = FFI_ArrowArrayStream::new(Box::new(Baseline(released.clone())));
        let mut out = FFI_ArrowArrayStream::empty();
        let mut cancel = ptr::null_mut();
        unsafe {
            let code = fluss_union_bucket_open_with_baseline(
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                0,
                0,
                0,
                ptr::null_mut(),
                &mut baseline,
                ptr::null(),
                0,
                &mut out,
                &mut cancel,
                ptr::null_mut(),
            );
            assert_eq!(code, ErrorCode::InvalidArgument as c_int);
        }
        assert!(cancel.is_null());
        assert!(baseline.release.is_none());
        drop(baseline);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
}
