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

use crate::pk_overlay::{PkOverlay, merged_stream};
use crate::predicate::FlussLakePredicate;
use crate::split_descriptor::{
    AppendLogSplitDescriptor, LakeSplitDescriptor, LogicalSplitDescriptor, PkHybridSplitDescriptor,
    SplitDescriptor,
};
use crate::union_read::{FlussLakeExecutor, FlussLakeScanSpec};
use crate::{
    FlussLakeError, FlussLakeExecutionContext, FlussLakeReadMode, FlussLakeReadSplit,
    FlussLakeRecordBatchStream, FlussLakeResult,
};
use arrow::compute::filter_record_batch;
use arrow::record_batch::RecordBatch;
use fluss::client::{FlussConnection, FlussTable, RecordBatchLogReader};
use fluss::error::Error as ClientError;
use fluss::metadata::{RowType, TableBucket};
use fluss::record::ChangeType;
use futures::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

/// Poll timeout used while folding a bounded changelog tail.
///
/// Short polls keep the bounded read responsive; the read terminates at the
/// frozen stop boundary or with a typed error.
const TAIL_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Default Fluss-Rust executor for opaque UnionRead splits.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FlussUnionReadExecutor;

impl FlussLakeExecutor for FlussUnionReadExecutor {
    fn execute(
        &self,
        split: FlussLakeReadSplit,
        context: FlussLakeExecutionContext,
        specification: FlussLakeScanSpec,
        table_properties: HashMap<String, String>,
    ) -> FlussLakeResult<FlussLakeRecordBatchStream> {
        // Structural validation fails fast; environment work is deferred to
        // the first poll of the returned stream.
        match SplitDescriptor::decode(split.execution_descriptor())? {
            SplitDescriptor::AppendLog(descriptor) => execute_append_log(descriptor, context),
            SplitDescriptor::LakeSplit(descriptor) => execute_lake_split(descriptor, context),
            SplitDescriptor::PkHybrid(descriptor) => execute_pk_hybrid(descriptor, context),
            SplitDescriptor::Logical(descriptor) => {
                execute_logical_split(descriptor, context, specification, table_properties)
            }
        }
    }
}

fn execute_logical_split(
    descriptor: LogicalSplitDescriptor,
    context: FlussLakeExecutionContext,
    specification: FlussLakeScanSpec,
    table_properties: HashMap<String, String>,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    if descriptor.is_empty()
        || (specification.read_mode() == FlussLakeReadMode::LakeOnly
            && descriptor.lake_splits().is_empty())
    {
        return Ok(Box::pin(futures::stream::empty()));
    }
    let connection = context.fluss_connection().cloned().ok_or_else(|| {
        FlussLakeError::Execution(
            "logical UnionRead split requires a Fluss connection in the execution context"
                .to_string(),
        )
    })?;
    Ok(lazy_stream(open_logical_stream(
        connection,
        descriptor,
        context,
        specification,
        table_properties,
    )))
}

async fn open_logical_stream(
    connection: Arc<FlussConnection>,
    descriptor: LogicalSplitDescriptor,
    context: FlussLakeExecutionContext,
    specification: FlussLakeScanSpec,
    table_properties: HashMap<String, String>,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    let table = connection
        .get_table(descriptor.table_path())
        .await
        .map_err(|error| execution_client_error("open Fluss table", error))?;
    validate_frozen_identity(
        &table,
        descriptor.table_path(),
        descriptor.table_bucket(),
        descriptor.schema_id(),
    )?;
    if table.has_primary_key() != descriptor.is_primary_key() {
        return Err(FlussLakeError::InvalidSplit(format!(
            "logical split primary-key identity no longer matches table {}",
            descriptor.table_path()
        )));
    }

    let table_info = table.get_table_info();
    let physical = PhysicalProjection::resolve(
        table_info.row_type(),
        specification.output_projection(),
        specification.filter(),
        descriptor.primary_key_indexes(),
    )?;
    let lake_stream = open_logical_lake_stream(
        &descriptor,
        &context,
        table_info,
        &table_properties,
        &physical,
    )
    .await?;

    if specification.read_mode() == FlussLakeReadMode::LakeOnly {
        return Ok(lake_stream);
    }
    if descriptor.is_primary_key() {
        let mut overlay = PkOverlay::try_new(
            physical.arrow_schema(table_info.row_type())?,
            physical.key_positions.clone(),
            context.memory_limit_bytes(),
        )?;
        fold_logical_changelog_tail(&table, &descriptor, &physical, &mut overlay).await?;
        return Ok(merged_stream(overlay, lake_stream));
    }

    let log_stream = open_logical_append_log_stream(&table, &descriptor, &physical).await?;
    Ok(Box::pin(lake_stream.chain(log_stream)))
}

#[cfg(feature = "paimon")]
async fn open_logical_lake_stream(
    descriptor: &LogicalSplitDescriptor,
    context: &FlussLakeExecutionContext,
    table_info: &fluss::metadata::TableInfo,
    table_properties: &HashMap<String, String>,
    physical: &PhysicalProjection,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    if descriptor.lake_splits().is_empty() {
        return Ok(Box::pin(futures::stream::empty()));
    }
    let snapshot_id = descriptor.snapshot_id().ok_or_else(|| {
        FlussLakeError::InvalidSplit(format!(
            "logical split for {} carries lake splits without a pinned snapshot id",
            descriptor.table_path()
        ))
    })?;
    let mut runtime_options = crate::paimon::PaimonCatalogOptions::from_table_info_with_overrides(
        table_info,
        table_properties,
    )?
    .sensitive();
    runtime_options.extend(context.lake_credentials().clone());
    let catalog_options = crate::paimon::PaimonCatalogOptions::from_map(
        descriptor
            .catalog_options()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
    .with_runtime_credentials(&runtime_options);
    let projected_fields =
        crate::paimon::projected_field_names(table_info.row_type(), Some(&physical.field_indexes))?;
    let splits = descriptor
        .lake_splits()
        .iter()
        .map(|split| crate::paimon::decode_split(split))
        .collect::<FlussLakeResult<Vec<_>>>()?;
    crate::paimon::read_snapshot_splits(
        descriptor.table_path(),
        &catalog_options,
        snapshot_id,
        Some(&projected_fields),
        splits,
    )
    .await
}

#[cfg(not(feature = "paimon"))]
async fn open_logical_lake_stream(
    descriptor: &LogicalSplitDescriptor,
    _context: &FlussLakeExecutionContext,
    _table_info: &fluss::metadata::TableInfo,
    _table_properties: &HashMap<String, String>,
    _physical: &PhysicalProjection,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    if descriptor.lake_splits().is_empty() {
        return Ok(Box::pin(futures::stream::empty()));
    }
    Err(FlussLakeError::Execution(format!(
        "logical split for {} carries lake splits, but this build has no lake format feature enabled",
        descriptor.table_path()
    )))
}

async fn open_logical_append_log_stream(
    table: &FlussTable<'_>,
    descriptor: &LogicalSplitDescriptor,
    physical: &PhysicalProjection,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    if descriptor.start_offset() == descriptor.stop_offset() {
        return Ok(Box::pin(futures::stream::empty()));
    }
    let scanner = table
        .new_scan()
        .project(&physical.field_indexes)
        .map_err(|error| execution_client_error("apply append-log projection", error))?
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
    Ok(Box::pin(reader.into_stream().map(|result| {
        result
            .map(|scan_batch| scan_batch.into_batch())
            .map_err(|error| execution_client_error("read bounded append-log split", error))
    })))
}

async fn fold_logical_changelog_tail(
    table: &FlussTable<'_>,
    descriptor: &LogicalSplitDescriptor,
    physical: &PhysicalProjection,
    overlay: &mut PkOverlay,
) -> FlussLakeResult<()> {
    if descriptor.start_offset() == descriptor.stop_offset() {
        return Ok(());
    }
    let scanner = table
        .new_scan()
        .project(&physical.field_indexes)
        .map_err(|error| execution_client_error("apply changelog projection", error))?
        .create_log_scanner()
        .map_err(|error| execution_client_error("create changelog scanner", error))?;
    let table_bucket = descriptor.table_bucket().clone();
    match table_bucket.partition_id() {
        Some(partition_id) => scanner
            .subscribe_partition(
                partition_id,
                table_bucket.bucket_id(),
                descriptor.start_offset(),
            )
            .await
            .map_err(|error| execution_client_error("subscribe partition bucket", error))?,
        None => scanner
            .subscribe(table_bucket.bucket_id(), descriptor.start_offset())
            .await
            .map_err(|error| execution_client_error("subscribe table bucket", error))?,
    }

    let mut next_offset = descriptor.start_offset();
    while next_offset < descriptor.stop_offset() {
        let records = scanner
            .poll(TAIL_POLL_TIMEOUT)
            .await
            .map_err(|error| execution_client_error("poll bounded changelog tail", error))?;
        let bucket_records = records.records(&table_bucket);
        if bucket_records.is_empty() {
            continue;
        }
        next_offset = fold_tail_records(
            bucket_records,
            next_offset,
            descriptor.stop_offset(),
            overlay,
        )?;
    }
    Ok(())
}

/// Physical columns are ordered as engine output, predicate-only columns,
/// then primary-key-only columns. The reader filters first and strips the
/// hidden suffix only after exact filtering.
struct PhysicalProjection {
    field_indexes: Vec<usize>,
    key_positions: Vec<usize>,
}

impl PhysicalProjection {
    fn resolve(
        row_type: &RowType,
        output_projection: Option<&[usize]>,
        filter: &FlussLakePredicate,
        primary_key_indexes: &[usize],
    ) -> FlussLakeResult<Self> {
        let field_count = row_type.fields().len();
        let mut field_indexes = match output_projection {
            Some(projection) => projection.to_vec(),
            None => (0..field_count).collect(),
        };
        for field_index in filter.referenced_field_indexes(row_type)? {
            if !field_indexes.contains(&field_index) {
                field_indexes.push(field_index);
            }
        }
        for primary_key_index in primary_key_indexes {
            if *primary_key_index >= field_count {
                return Err(FlussLakeError::InvalidSplit(format!(
                    "primary-key field index {primary_key_index} exceeds the resolved table width {field_count}"
                )));
            }
            if !field_indexes.contains(primary_key_index) {
                field_indexes.push(*primary_key_index);
            }
        }
        let key_positions = primary_key_indexes
            .iter()
            .map(|primary_key_index| {
                field_indexes
                    .iter()
                    .position(|field_index| field_index == primary_key_index)
                    .expect("primary-key columns were included in the physical projection")
            })
            .collect();
        Ok(Self {
            field_indexes,
            key_positions,
        })
    }

    fn arrow_schema(&self, row_type: &RowType) -> FlussLakeResult<arrow::datatypes::SchemaRef> {
        let schema = fluss::record::to_arrow_schema(row_type).map_err(|error| {
            FlussLakeError::Execution(format!(
                "failed to convert the physical read schema to Arrow: {error}"
            ))
        })?;
        schema
            .project(&self.field_indexes)
            .map(Arc::new)
            .map_err(|error| {
                FlussLakeError::Execution(format!(
                    "failed to project the physical UnionRead schema: {error}"
                ))
            })
    }
}

/// Merges one primary-key bucket's lake baseline with its bounded log tail.
///
/// The tail is folded first, in its entirety, because a lake row can only be
/// passed through once it is known that no later changelog record supersedes
/// it. The fold enforces its own idle-progress deadline, so wrapping the
/// returned stream again would charge the same budget twice.
fn execute_pk_hybrid(
    descriptor: PkHybridSplitDescriptor,
    context: FlussLakeExecutionContext,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    if descriptor.is_empty() {
        return Ok(Box::pin(futures::stream::empty::<
            FlussLakeResult<RecordBatch>,
        >()));
    }

    let connection = context.fluss_connection().cloned().ok_or_else(|| {
        FlussLakeError::Execution(
            "pk-hybrid split requires a Fluss connection in the execution context".to_string(),
        )
    })?;
    Ok(lazy_stream(open_pk_hybrid_stream(
        connection, descriptor, context,
    )))
}

async fn open_pk_hybrid_stream(
    connection: Arc<FlussConnection>,
    descriptor: PkHybridSplitDescriptor,
    context: FlussLakeExecutionContext,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    let table = connection
        .get_table(descriptor.table_path())
        .await
        .map_err(|error| execution_client_error("open Fluss table", error))?;
    validate_frozen_identity(
        &table,
        descriptor.table_path(),
        descriptor.table_bucket(),
        descriptor.schema_id(),
    )?;
    if !table.has_primary_key() {
        return Err(FlussLakeError::InvalidSplit(format!(
            "pk-hybrid split cannot execute against non-primary-key table {}",
            descriptor.table_path()
        )));
    }

    let table_info = table.get_table_info();
    let physical = PhysicalPkProjection::resolve(
        table_info.row_type(),
        descriptor.output_projection(),
        descriptor.pk_indexes(),
    )?;
    let mut overlay = PkOverlay::try_new(
        physical.arrow_schema(table_info.row_type())?,
        physical.key_positions.clone(),
        context.memory_limit_bytes(),
    )?;
    fold_changelog_tail(&table, &descriptor, &physical, &mut overlay, &context).await?;

    let lake_stream =
        open_pk_lake_stream(&descriptor, &context, table_info.row_type(), &physical).await?;
    Ok(merged_stream(overlay, lake_stream))
}

/// Opens the lake half of a primary-key split.
///
/// All of the bucket's splits go through a single Paimon reader: since
/// apache/paimon-rust#374 that is what guarantees each key appears exactly
/// once on the lake side, which the overlay presumes.
#[cfg(feature = "paimon")]
async fn open_pk_lake_stream(
    descriptor: &PkHybridSplitDescriptor,
    context: &FlussLakeExecutionContext,
    row_type: &RowType,
    physical: &PhysicalPkProjection,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    if descriptor.lake_splits().is_empty() {
        return Ok(Box::pin(futures::stream::empty()));
    }
    let snapshot_id = descriptor.snapshot_id().ok_or_else(|| {
        FlussLakeError::InvalidSplit(format!(
            "pk-hybrid split for {} carries lake splits without a pinned snapshot id",
            descriptor.table_path()
        ))
    })?;

    let catalog_options = crate::paimon::PaimonCatalogOptions::from_map(
        descriptor
            .catalog_options()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
    .with_runtime_credentials(context.lake_credentials());
    let projected_fields =
        crate::paimon::projected_field_names(row_type, Some(&physical.field_indexes))?;
    let mut splits = Vec::with_capacity(descriptor.lake_splits().len());
    for encoded_split in descriptor.lake_splits() {
        splits.push(crate::paimon::decode_split(encoded_split)?);
    }

    crate::paimon::read_snapshot_splits(
        descriptor.table_path(),
        &catalog_options,
        snapshot_id,
        Some(&projected_fields),
        splits,
    )
    .await
}

#[cfg(not(feature = "paimon"))]
async fn open_pk_lake_stream(
    descriptor: &PkHybridSplitDescriptor,
    _context: &FlussLakeExecutionContext,
    _row_type: &RowType,
    _physical: &PhysicalPkProjection,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    if descriptor.lake_splits().is_empty() {
        return Ok(Box::pin(futures::stream::empty()));
    }
    Err(FlussLakeError::Execution(format!(
        "pk-hybrid split for {} carries lake splits, but this build has no lake format feature enabled",
        descriptor.table_path()
    )))
}

/// Folds the split's bounded changelog range into the overlay.
///
/// The tail is read row-wise on purpose: the Arrow batch path drops change
/// types, and without them a delete is indistinguishable from an insert. The
/// loop exits only at the frozen stop offset or with a typed error, and a
/// stretch without progress longer than the context idle timeout is an
/// operational failure rather than a reason to wait forever.
async fn fold_changelog_tail(
    table: &FlussTable<'_>,
    descriptor: &PkHybridSplitDescriptor,
    physical: &PhysicalPkProjection,
    overlay: &mut PkOverlay,
    _context: &FlussLakeExecutionContext,
) -> FlussLakeResult<()> {
    if descriptor.start_offset() == descriptor.stop_offset() {
        return Ok(());
    }

    let scanner = table
        .new_scan()
        .project(&physical.field_indexes)
        .map_err(|error| execution_client_error("apply changelog projection", error))?
        .create_log_scanner()
        .map_err(|error| execution_client_error("create changelog scanner", error))?;
    let table_bucket = descriptor.table_bucket().clone();
    match table_bucket.partition_id() {
        Some(partition_id) => scanner
            .subscribe_partition(
                partition_id,
                table_bucket.bucket_id(),
                descriptor.start_offset(),
            )
            .await
            .map_err(|error| execution_client_error("subscribe partition bucket", error))?,
        None => scanner
            .subscribe(table_bucket.bucket_id(), descriptor.start_offset())
            .await
            .map_err(|error| execution_client_error("subscribe table bucket", error))?,
    }

    let mut next_offset = descriptor.start_offset();
    while next_offset < descriptor.stop_offset() {
        let records = scanner
            .poll(TAIL_POLL_TIMEOUT)
            .await
            .map_err(|error| execution_client_error("poll bounded changelog tail", error))?;
        let bucket_records = records.records(&table_bucket);
        if bucket_records.is_empty() {
            continue;
        }

        next_offset = fold_tail_records(
            bucket_records,
            next_offset,
            descriptor.stop_offset(),
            overlay,
        )?;
    }
    Ok(())
}

/// Folds one poll's records, returning the next unread offset.
///
/// Records are grouped into runs sharing an Arrow batch so that keys are
/// encoded per batch rather than per row. Records at or beyond the frozen
/// stop offset are discarded: the boundary belongs to the plan, and a server
/// that returns more must not widen the result.
fn fold_tail_records(
    records: &[fluss::record::ScanRecord],
    next_offset: i64,
    stop_offset: i64,
    overlay: &mut PkOverlay,
) -> FlussLakeResult<i64> {
    let mut next_offset = next_offset;
    // Records of one poll arrive grouped by their backing Arrow batch, so
    // runs are detected by batch identity (address comparison only — the
    // records keep every batch alive for the whole loop).
    let mut run_source: Option<*const RecordBatch> = None;
    let mut run_batch: Option<RecordBatch> = None;
    let mut run_rows: Vec<usize> = Vec::new();
    let mut run_change_types: Vec<ChangeType> = Vec::new();

    for record in records {
        if record.offset() < next_offset || record.offset() >= stop_offset {
            continue;
        }
        let row = record.row();
        let batch = row.get_record_batch().ok_or_else(|| {
            FlussLakeError::Execution(
                "changelog record has no backing Arrow batch; the ARROW log format is required to merge a primary-key table"
                    .to_string(),
            )
        })?;
        let batch_address = batch as *const RecordBatch;
        if run_source != Some(batch_address) {
            flush_tail_run(
                &mut run_batch,
                &mut run_rows,
                &mut run_change_types,
                overlay,
            )?;
            run_source = Some(batch_address);
            run_batch = Some(batch.clone());
        }
        run_rows.push(row.get_row_id());
        run_change_types.push(*record.change_type());
        next_offset = record.offset() + 1;
    }
    flush_tail_run(
        &mut run_batch,
        &mut run_rows,
        &mut run_change_types,
        overlay,
    )?;
    Ok(next_offset)
}

fn flush_tail_run(
    run_batch: &mut Option<RecordBatch>,
    run_rows: &mut Vec<usize>,
    run_change_types: &mut Vec<ChangeType>,
    overlay: &mut PkOverlay,
) -> FlussLakeResult<()> {
    let Some(batch) = run_batch.take() else {
        return Ok(());
    };
    if run_rows.is_empty() {
        return Ok(());
    }
    // The polled batch may cover offsets outside the frozen range, so only
    // the selected rows are folded, in offset order.
    let selected = take_rows(&batch, run_rows)?;
    overlay.fold_tail_batch(selected, run_change_types)?;
    run_rows.clear();
    run_change_types.clear();
    Ok(())
}

fn take_rows(batch: &RecordBatch, row_indexes: &[usize]) -> FlussLakeResult<RecordBatch> {
    // The common case is a run covering the whole batch in order, which needs
    // no copy at all.
    if row_indexes.len() == batch.num_rows()
        && row_indexes
            .iter()
            .enumerate()
            .all(|(position, row)| position == *row)
    {
        return Ok(batch.clone());
    }
    let indices: Vec<(usize, usize)> = row_indexes.iter().map(|row| (0, *row)).collect();
    arrow::compute::interleave_record_batch(&[batch], &indices).map_err(|error| {
        FlussLakeError::Execution(format!("failed to select changelog tail rows: {error}"))
    })
}

/// The physical read of a primary-key split, widened to carry its key columns.
///
/// The engine's requested columns come first and the missing key columns are
/// appended, so the engine-visible output is exactly the leading prefix and
/// the added columns are stripped before emission.
struct PhysicalPkProjection {
    field_indexes: Vec<usize>,
    key_positions: Vec<usize>,
    #[allow(dead_code)]
    output_column_count: usize,
}

impl PhysicalPkProjection {
    fn resolve(
        row_type: &RowType,
        output_projection: Option<&[usize]>,
        pk_indexes: &[usize],
    ) -> FlussLakeResult<Self> {
        let field_count = row_type.fields().len();
        for pk_index in pk_indexes {
            if *pk_index >= field_count {
                return Err(FlussLakeError::InvalidSplit(format!(
                    "primary-key field index {pk_index} exceeds the resolved table width {field_count}"
                )));
            }
        }

        let mut field_indexes = match output_projection {
            Some(projection) => projection.to_vec(),
            None => (0..field_count).collect(),
        };
        let output_column_count = field_indexes.len();
        for pk_index in pk_indexes {
            if !field_indexes.contains(pk_index) {
                field_indexes.push(*pk_index);
            }
        }
        let key_positions = pk_indexes
            .iter()
            .map(|pk_index| {
                field_indexes
                    .iter()
                    .position(|index| index == pk_index)
                    .expect("every key column was just ensured to be in the physical projection")
            })
            .collect();

        Ok(Self {
            field_indexes,
            key_positions,
            output_column_count,
        })
    }

    fn arrow_schema(&self, row_type: &RowType) -> FlussLakeResult<arrow::datatypes::SchemaRef> {
        let schema = fluss::record::to_arrow_schema(row_type).map_err(|error| {
            FlussLakeError::Execution(format!(
                "failed to convert the read schema to Arrow: {error}"
            ))
        })?;
        schema
            .project(&self.field_indexes)
            .map(Arc::new)
            .map_err(|error| {
                FlussLakeError::Execution(format!(
                    "failed to project the physical primary-key read schema: {error}"
                ))
            })
    }
}

/// Rejects a split whose frozen identity no longer matches the live table.
///
/// The schema is frozen at plan time, so executing against a drifted schema
/// would silently reinterpret data.
fn validate_frozen_identity(
    table: &FlussTable<'_>,
    table_path: &fluss::metadata::TablePath,
    table_bucket: &TableBucket,
    schema_id: i32,
) -> FlussLakeResult<()> {
    let table_info = table.get_table_info();
    if table_info.table_id != table_bucket.table_id() {
        return Err(FlussLakeError::SchemaIncompatible(format!(
            "split table id {} no longer matches resolved table id {} for {table_path}",
            table_bucket.table_id(),
            table_info.table_id
        )));
    }
    if table_info.schema_id != schema_id {
        return Err(FlussLakeError::SchemaIncompatible(format!(
            "split schema id {schema_id} no longer matches current schema id {} for {table_path}; execution against a historical schema is not implemented",
            table_info.schema_id
        )));
    }
    Ok(())
}

/// Defers an asynchronous stream setup until the stream's first poll.
///
/// Setup failures surface as the first (and only) stream item instead of
/// failing the `execute` call, keeping `execute` synchronous and lazy.
fn lazy_stream<F>(setup: F) -> FlussLakeRecordBatchStream
where
    F: Future<Output = FlussLakeResult<FlussLakeRecordBatchStream>> + Send + 'static,
{
    Box::pin(futures::stream::once(setup).try_flatten())
}

/// Reads one frozen lake split.
///
/// Lake splits are decodable in every build so that a split planned elsewhere
/// reports a clear error here instead of failing to parse. Reading requires the
/// matching lake format feature to be compiled in.
#[cfg(feature = "paimon")]
fn execute_lake_split(
    descriptor: LakeSplitDescriptor,
    context: FlussLakeExecutionContext,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    Ok(lazy_stream(async move {
        // The split carries only non-sensitive options; secrets arrive through
        // the execution context and override any split-carried value.
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
    }))
}

#[cfg(not(feature = "paimon"))]
fn execute_lake_split(
    descriptor: LakeSplitDescriptor,
    _context: FlussLakeExecutionContext,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    Err(FlussLakeError::Execution(format!(
        "lake split for {} cannot be executed: this build has no lake format feature enabled",
        descriptor.table_path()
    )))
}

fn execute_append_log(
    descriptor: AppendLogSplitDescriptor,
    context: FlussLakeExecutionContext,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    if descriptor.is_empty() {
        return Ok(Box::pin(futures::stream::empty::<
            FlussLakeResult<RecordBatch>,
        >()));
    }

    // A missing connection is a context-shape problem, not an environment
    // failure, so it still fails fast.
    let connection = context.fluss_connection().cloned().ok_or_else(|| {
        FlussLakeError::Execution(
            "append-log split requires a Fluss connection in the execution context".to_string(),
        )
    })?;
    Ok(lazy_stream(open_append_log_stream(connection, descriptor)))
}

async fn open_append_log_stream(
    connection: Arc<FlussConnection>,
    descriptor: AppendLogSplitDescriptor,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    let table = connection
        .get_table(descriptor.table_path())
        .await
        .map_err(|error| execution_client_error("open Fluss table", error))?;
    validate_frozen_identity(
        &table,
        descriptor.table_path(),
        descriptor.table_bucket(),
        descriptor.schema_id(),
    )?;
    if table.has_primary_key() {
        return Err(FlussLakeError::InvalidSplit(format!(
            "append-log split cannot execute against primary-key table {}",
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
            .map_err(|error| execution_client_error("read bounded append-log split", error))
    });
    Ok(Box::pin(stream))
}

fn execution_client_error(action: &str, error: ClientError) -> FlussLakeError {
    match error {
        ClientError::LogOffsetOutOfRange { .. } => {
            FlussLakeError::DataUnavailable(format!("failed to {action}: {error}"))
        }
        _ => FlussLakeError::Execution(format!("failed to {action}: {error}")),
    }
}

/// Applies the exact predicate before stripping hidden physical columns.
pub(crate) fn apply_filter_and_projection(
    stream: FlussLakeRecordBatchStream,
    filter: &FlussLakePredicate,
    output_column_count: Option<usize>,
) -> FlussLakeRecordBatchStream {
    if matches!(filter, FlussLakePredicate::AlwaysTrue) && output_column_count.is_none() {
        return stream;
    }
    let filter = Arc::new(filter.clone());
    Box::pin(stream.try_filter_map(move |batch| {
        let filter = Arc::clone(&filter);
        async move {
            let filtered = if matches!(filter.as_ref(), FlussLakePredicate::AlwaysTrue) {
                batch
            } else {
                let mask = filter.evaluate_batch(&batch)?;
                let true_count = mask.true_count();
                if true_count == 0 {
                    return Ok(None);
                }
                if true_count == batch.num_rows() {
                    batch
                } else {
                    filter_record_batch(&batch, &mask).map_err(|error| {
                        FlussLakeError::Execution(format!(
                            "failed to apply filter to record batch: {error}"
                        ))
                    })?
                }
            };
            if let Some(output_column_count) = output_column_count
                && filtered.num_columns() != output_column_count
            {
                let projection: Vec<usize> = (0..output_column_count).collect();
                return filtered.project(&projection).map(Some).map_err(|error| {
                    FlussLakeError::Execution(format!(
                        "failed to strip hidden UnionRead columns after filtering: {error}"
                    ))
                });
            }
            Ok(Some(filtered))
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split_descriptor::SplitDescriptor;
    use crate::{CURRENT_FLUSS_LAKE_SPLIT_VERSION, FlussLakeReadStatistics};
    use fluss::metadata::{TableBucket, TablePath};
    use futures::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn execution_spec(table: &str) -> FlussLakeScanSpec {
        FlussLakeScanSpec::new(TablePath::new("fluss", table))
    }

    fn append_log_split(start_offset: i64, stop_offset: i64) -> FlussLakeReadSplit {
        let descriptor = SplitDescriptor::AppendLog(
            AppendLogSplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                1,
                TableBucket::new(5, 0),
                start_offset,
                stop_offset,
                None,
            )
            .unwrap(),
        );
        FlussLakeReadSplit::try_new(
            "append-log/5/root/0".to_string(),
            0,
            None,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor.encode().unwrap(),
            FlussLakeReadStatistics::default(),
        )
        .unwrap()
    }

    #[test]
    fn empty_append_log_split_finishes_without_runtime_connection() {
        let executor = FlussUnionReadExecutor;
        let mut stream = executor
            .execute(
                append_log_split(10, 10),
                FlussLakeExecutionContext::default(),
                execution_spec("orders"),
                HashMap::new(),
            )
            .unwrap();

        assert!(futures::executor::block_on(stream.next()).is_none());
    }

    #[test]
    fn non_empty_append_log_split_requires_runtime_connection() {
        let executor = FlussUnionReadExecutor;
        let result = executor.execute(
            append_log_split(10, 11),
            FlussLakeExecutionContext::default(),
            execution_spec("orders"),
            HashMap::new(),
        );

        assert!(matches!(result, Err(FlussLakeError::Execution(_))));
    }

    /// `execute` must reject an undecodable descriptor synchronously, before
    /// any environment work, so engines see structural errors immediately.
    #[test]
    fn undecodable_descriptor_fails_before_the_stream_is_polled() {
        let mut split_bytes = append_log_split(10, 11).encode().unwrap();
        let descriptor_start = split_bytes.len() - 1;
        split_bytes[descriptor_start] ^= 0xff;

        assert!(FlussLakeReadSplit::decode(&split_bytes).is_err());
    }

    /// Setup work must not run until the stream is polled, and its failure
    /// must arrive as the first stream item rather than from `execute`.
    #[test]
    fn lazy_stream_defers_setup_until_the_first_poll() {
        let setup_ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&setup_ran);
        let mut stream = lazy_stream(async move {
            flag.store(true, Ordering::SeqCst);
            Err(FlussLakeError::Execution("deferred failure".to_string()))
        });

        assert!(!setup_ran.load(Ordering::SeqCst));

        let first = futures::executor::block_on(stream.next());

        assert!(setup_ran.load(Ordering::SeqCst));
        assert!(matches!(first, Some(Err(FlussLakeError::Execution(_)))));
        assert!(futures::executor::block_on(stream.next()).is_none());
    }

    /// A lake split planned elsewhere must stay decodable here, so that a
    /// build without any lake format reports why it cannot run it.
    #[test]
    #[cfg(not(feature = "paimon"))]
    fn lake_split_without_lake_feature_reports_a_clear_error() {
        use crate::split_descriptor::LakeSplitDescriptor;
        use std::collections::BTreeMap;

        let mut catalog_options = BTreeMap::new();
        catalog_options.insert("warehouse".to_string(), "/tmp/warehouse".to_string());
        let descriptor = SplitDescriptor::LakeSplit(
            LakeSplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                7,
                catalog_options,
                None,
                "{}".to_string(),
            )
            .unwrap(),
        );
        let split = FlussLakeReadSplit::try_new(
            "lake-split/fluss.orders/7/0".to_string(),
            0,
            None,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor.encode().unwrap(),
            FlussLakeReadStatistics::default(),
        )
        .unwrap();

        let result = FlussUnionReadExecutor.execute(
            split,
            FlussLakeExecutionContext::default(),
            execution_spec("orders"),
            HashMap::new(),
        );

        match result {
            Err(FlussLakeError::Execution(message)) => {
                assert!(
                    message.contains("no lake format feature"),
                    "unexpected error: {message}"
                );
            }
            Err(other) => panic!("expected an execution error, got: {other}"),
            Ok(_) => panic!("a lake split must not execute without a lake format feature"),
        }
    }

    fn pk_row_type() -> RowType {
        use fluss::metadata::{DataField, DataTypes};

        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("region", DataTypes::string(), None),
            DataField::new("amount", DataTypes::bigint(), None),
        ])
    }

    /// A request that omits key columns must still read them physically, with
    /// the engine-visible columns kept as the leading prefix so the widening
    /// can be stripped again before emission.
    #[test]
    fn physical_projection_appends_missing_key_columns_after_requested_ones() {
        let physical = PhysicalPkProjection::resolve(&pk_row_type(), Some(&[2]), &[0, 1]).unwrap();

        assert_eq!(physical.field_indexes, vec![2, 0, 1]);
        assert_eq!(physical.key_positions, vec![1, 2]);
        assert_eq!(physical.output_column_count, 1);
    }

    /// A request that already covers the key columns must not widen the read,
    /// and must not reorder the engine's columns.
    #[test]
    fn physical_projection_keeps_requested_projection_when_keys_are_covered() {
        let physical =
            PhysicalPkProjection::resolve(&pk_row_type(), Some(&[1, 0]), &[0, 1]).unwrap();

        assert_eq!(physical.field_indexes, vec![1, 0]);
        assert_eq!(physical.key_positions, vec![1, 0]);
        assert_eq!(physical.output_column_count, 2);
    }

    #[test]
    fn physical_projection_defaults_to_every_column() {
        let physical = PhysicalPkProjection::resolve(&pk_row_type(), None, &[0]).unwrap();

        assert_eq!(physical.field_indexes, vec![0, 1, 2]);
        assert_eq!(physical.key_positions, vec![0]);
        assert_eq!(physical.output_column_count, 3);
    }

    #[test]
    fn physical_projection_rejects_key_indexes_beyond_the_table() {
        assert!(matches!(
            PhysicalPkProjection::resolve(&pk_row_type(), None, &[3]),
            Err(FlussLakeError::InvalidSplit(_))
        ));
    }

    #[test]
    fn physical_projection_keeps_predicate_columns_hidden_from_output() {
        let physical = PhysicalProjection::resolve(
            &pk_row_type(),
            Some(&[2]),
            &FlussLakePredicate::eq("id", 1_i32),
            &[0, 1],
        )
        .unwrap();

        assert_eq!(physical.field_indexes, vec![2, 0, 1]);
        assert_eq!(physical.key_positions, vec![1, 2]);
    }

    fn pk_hybrid_split(start_offset: i64, stop_offset: i64) -> FlussLakeReadSplit {
        use crate::split_descriptor::PkHybridSplitDescriptor;
        use std::collections::BTreeMap;

        let descriptor = SplitDescriptor::PkHybrid(
            PkHybridSplitDescriptor::try_new(
                TablePath::new("fluss", "pk_orders"),
                1,
                TableBucket::new(5, 0),
                start_offset,
                stop_offset,
                None,
                BTreeMap::new(),
                Vec::new(),
                vec![0],
                None,
            )
            .unwrap(),
        );
        FlussLakeReadSplit::try_new(
            "pk-hybrid/5/root/0".to_string(),
            0,
            None,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor.encode().unwrap(),
            FlussLakeReadStatistics::default(),
        )
        .unwrap()
    }

    /// A bucket with neither lake splits nor a log tail contributes nothing,
    /// so it must finish without touching the environment.
    #[test]
    fn empty_pk_hybrid_split_finishes_without_runtime_connection() {
        let mut stream = FlussUnionReadExecutor
            .execute(
                pk_hybrid_split(10, 10),
                FlussLakeExecutionContext::default(),
                execution_spec("pk_orders"),
                HashMap::new(),
            )
            .unwrap();

        assert!(futures::executor::block_on(stream.next()).is_none());
    }

    #[test]
    fn non_empty_pk_hybrid_split_requires_runtime_connection() {
        let result = FlussUnionReadExecutor.execute(
            pk_hybrid_split(10, 11),
            FlussLakeExecutionContext::default(),
            execution_spec("pk_orders"),
            HashMap::new(),
        );

        assert!(matches!(result, Err(FlussLakeError::Execution(_))));
    }

    #[test]
    fn take_rows_avoids_copying_a_run_covering_the_whole_batch() {
        use arrow::array::Int32Array;
        use arrow::datatypes::{DataType, Field, Schema};

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)])),
            vec![Arc::new(Int32Array::from(vec![1, 2, 3])) as arrow::array::ArrayRef],
        )
        .unwrap();

        let whole = take_rows(&batch, &[0, 1, 2]).unwrap();
        let subset = take_rows(&batch, &[2, 0]).unwrap();

        assert_eq!(whole.num_rows(), 3);
        assert_eq!(subset.num_rows(), 2);
        let ids = subset
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(ids.values(), &[3, 1]);
    }

    #[test]
    fn apply_filter_removes_non_matching_rows_from_stream() {
        use arrow::array::Int32Array;
        use arrow::datatypes::{DataType, Field, Schema};
        use futures::stream;

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)])),
            vec![Arc::new(Int32Array::from(vec![1, 2, 3, 4])) as arrow::array::ArrayRef],
        )
        .unwrap();
        let source: FlussLakeRecordBatchStream =
            Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(batch)]));
        let filtered =
            apply_filter_and_projection(source, &FlussLakePredicate::gt("id", 2_i32), None);

        let batches: Vec<RecordBatch> =
            futures::executor::block_on(filtered.try_collect()).unwrap();
        assert_eq!(batches.len(), 1);
        let ids = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(ids.values(), &[3, 4]);
    }

    #[test]
    fn apply_filter_drops_batch_with_no_matching_rows() {
        use arrow::array::Int32Array;
        use arrow::datatypes::{DataType, Field, Schema};
        use futures::stream;

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)])),
            vec![Arc::new(Int32Array::from(vec![1, 2])) as arrow::array::ArrayRef],
        )
        .unwrap();
        let source: FlussLakeRecordBatchStream =
            Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(batch)]));
        let filtered =
            apply_filter_and_projection(source, &FlussLakePredicate::gt("id", 5_i32), None);

        let batches: Vec<RecordBatch> =
            futures::executor::block_on(filtered.try_collect()).unwrap();
        assert!(batches.is_empty());
    }

    #[test]
    fn exact_filter_runs_before_hidden_columns_are_stripped() {
        use arrow::array::{Int32Array, StringArray};
        use arrow::datatypes::{DataType, Field, Schema};
        use futures::stream;

        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("name", DataType::Utf8, false),
                Field::new("id", DataType::Int32, false),
            ])),
            vec![
                Arc::new(StringArray::from(vec!["one", "two"])) as arrow::array::ArrayRef,
                Arc::new(Int32Array::from(vec![1, 2])) as arrow::array::ArrayRef,
            ],
        )
        .unwrap();
        let source: FlussLakeRecordBatchStream =
            Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(batch)]));
        let filtered =
            apply_filter_and_projection(source, &FlussLakePredicate::eq("id", 2_i32), Some(1));

        let batches: Vec<RecordBatch> =
            futures::executor::block_on(filtered.try_collect()).unwrap();

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_columns(), 1);
        assert_eq!(batches[0].schema().field(0).name(), "name");
        let names = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(names.value(0), "two");
    }

    #[test]
    fn exact_filter_never_passes_through_a_missing_column() {
        use arrow::array::StringArray;
        use futures::stream;

        let batch = RecordBatch::try_from_iter(vec![(
            "name",
            Arc::new(StringArray::from(vec!["one"])) as arrow::array::ArrayRef,
        )])
        .unwrap();
        let source: FlussLakeRecordBatchStream =
            Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(batch)]));
        let filtered =
            apply_filter_and_projection(source, &FlussLakePredicate::eq("id", 1_i32), None);

        let result = futures::executor::block_on(filtered.try_collect::<Vec<RecordBatch>>());

        assert!(matches!(result, Err(FlussLakeError::SchemaIncompatible(_))));
    }

    #[test]
    fn log_offset_out_of_range_maps_to_data_unavailable() {
        let error = ClientError::LogOffsetOutOfRange {
            message: "offset 1 was removed".to_string(),
        };

        assert!(matches!(
            execution_client_error("read bounded log", error),
            FlussLakeError::DataUnavailable(_)
        ));
    }
}
