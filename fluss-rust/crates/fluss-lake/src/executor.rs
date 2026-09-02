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

use crate::split_descriptor::SplitDescriptor;
use crate::table::FlussLakeScan;
use crate::{FlussLakeError, FlussLakeReadSplit, RecordBatchStream, Result};
use arrow::compute::filter_record_batch;
use arrow::record_batch::RecordBatch;
use fluss::client::{DeduplicateCurrentView, FlussTable, RecordBatchLogReader};
use fluss::error::Error as ClientError;
use fluss::metadata::{RowType, TableBucket};
use fluss::predicate::{BoundPredicate, Predicate};
use fluss::record::ChangeType;
use futures::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

/// Poll interval used while folding a bounded changelog tail.
///
/// This is only the duration of one scanner poll. It is not a UnionRead
/// no-progress deadline; cancellation and query timeouts belong to the caller.
const TAIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Executes one opaque split with the configuration held by its scan.
pub(crate) fn execute_split(
    split: FlussLakeReadSplit,
    scan: &FlussLakeScan,
) -> Result<RecordBatchStream> {
    scan.validate_configuration()?;
    // Structural validation fails fast; environment work is deferred to the
    // first poll of the returned stream.
    let descriptor = split.decode_execution_descriptor()?;
    execute_logical_split(descriptor, scan.clone())
}

fn execute_logical_split(
    descriptor: SplitDescriptor,
    scan: FlussLakeScan,
) -> Result<RecordBatchStream> {
    if descriptor.is_empty() || (scan.lake_only() && descriptor.lake_splits().is_empty()) {
        return Ok(Box::pin(futures::stream::empty()));
    }
    Ok(lazy_stream(open_logical_stream(descriptor, scan)))
}

async fn open_logical_stream(
    descriptor: SplitDescriptor,
    scan: FlussLakeScan,
) -> Result<RecordBatchStream> {
    let table = scan
        .connection()
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
        return Err(FlussLakeError::Internal(format!(
            "logical split primary-key identity no longer matches table {}",
            descriptor.table_path()
        )));
    }

    let table_info = table.get_table_info();
    let output_projection = scan.resolve_projection(table_info.row_type())?;
    let filter = BoundPredicate::bind(scan.filter(), table_info.row_type()).map_err(|error| {
        FlussLakeError::PlanningFailed(format!("failed to bind filter predicate: {error}"))
    })?;
    let physical = PhysicalProjection::resolve(
        table_info.row_type(),
        output_projection.as_deref(),
        &filter,
        descriptor.primary_key_indexes(),
    )?;
    let physical_schema = physical.arrow_schema(table_info.row_type())?;
    let lake_stream = normalize_stream_schema(
        open_logical_lake_stream(
            &descriptor,
            table_info,
            scan.catalog_property_overrides(),
            &physical,
            &filter,
            descriptor.is_primary_key() && !scan.lake_only(),
        )
        .await?,
        physical_schema.clone(),
    );

    let output_column_count = output_projection.as_ref().map(Vec::len);
    let batch_size = scan.batch_size();
    if scan.lake_only() {
        return Ok(apply_output_processing(
            lake_stream,
            &filter,
            output_column_count,
            batch_size,
        ));
    }
    if descriptor.is_primary_key() {
        let mut current_view =
            DeduplicateCurrentView::try_new(physical_schema, physical.key_positions.clone())
                .map_err(reconciliation_error)?;
        fold_logical_changelog_tail(&table, &descriptor, &physical, &mut current_view).await?;
        return Ok(apply_output_processing(
            reconciled_stream(current_view, lake_stream),
            &filter,
            output_column_count,
            batch_size,
        ));
    }

    let log_stream = normalize_stream_schema(
        open_logical_append_log_stream(&table, &descriptor, &physical, scan.filter()).await?,
        physical_schema,
    );
    Ok(apply_output_processing(
        Box::pin(lake_stream.chain(log_stream)),
        &filter,
        output_column_count,
        batch_size,
    ))
}

#[cfg(feature = "paimon")]
async fn open_logical_lake_stream(
    descriptor: &SplitDescriptor,
    table_info: &fluss::metadata::TableInfo,
    table_properties: &HashMap<String, String>,
    physical: &PhysicalProjection,
    filter: &BoundPredicate,
    reconcile_primary_key: bool,
) -> Result<RecordBatchStream> {
    if descriptor.lake_splits().is_empty() {
        return Ok(Box::pin(futures::stream::empty()));
    }
    let snapshot_id = descriptor.snapshot_id().ok_or_else(|| {
        FlussLakeError::Internal(format!(
            "logical split for {} carries lake splits without a pinned snapshot id",
            descriptor.table_path()
        ))
    })?;
    let catalog_options = crate::paimon::PaimonCatalogOptions::from_table_info_with_overrides(
        table_info,
        table_properties,
    )?;
    let projected_fields =
        crate::paimon::projected_field_names(table_info.row_type(), Some(&physical.field_indexes))?;
    let lake_filter =
        crate::paimon::lake_pushdown_filter(filter, table_info, reconcile_primary_key);
    crate::paimon::read_snapshot_splits(
        descriptor.table_path(),
        &catalog_options,
        snapshot_id,
        descriptor.table_bucket().bucket_id(),
        Some(&projected_fields),
        descriptor.lake_splits(),
        lake_filter.as_ref(),
    )
    .await
}

#[cfg(not(feature = "paimon"))]
async fn open_logical_lake_stream(
    descriptor: &SplitDescriptor,
    _table_info: &fluss::metadata::TableInfo,
    _table_properties: &HashMap<String, String>,
    _physical: &PhysicalProjection,
    _filter: &BoundPredicate,
    _reconcile_primary_key: bool,
) -> Result<RecordBatchStream> {
    if descriptor.lake_splits().is_empty() {
        return Ok(Box::pin(futures::stream::empty()));
    }
    Err(FlussLakeError::Internal(format!(
        "logical split for {} carries lake splits, but this build has no lake format feature enabled",
        descriptor.table_path()
    )))
}

async fn open_logical_append_log_stream(
    table: &FlussTable<'_>,
    descriptor: &SplitDescriptor,
    physical: &PhysicalProjection,
    filter: Option<&Predicate>,
) -> Result<RecordBatchStream> {
    if descriptor.start_offset() == descriptor.stop_offset() {
        return Ok(Box::pin(futures::stream::empty()));
    }
    let mut table_scan = table
        .new_scan()
        .project(&physical.field_indexes)
        .map_err(|error| execution_client_error("apply append-log projection", error))?;
    if let Some(filter) = filter {
        table_scan = table_scan
            .filter(filter.clone())
            .map_err(|error| execution_client_error("apply append-log filter pushdown", error))?;
    }
    let scanner = table_scan
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
    descriptor: &SplitDescriptor,
    physical: &PhysicalProjection,
    current_view: &mut DeduplicateCurrentView,
) -> Result<()> {
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
            .poll(TAIL_POLL_INTERVAL)
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
            current_view,
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
        filter: &BoundPredicate,
        primary_key_indexes: &[usize],
    ) -> Result<Self> {
        let field_count = row_type.fields().len();
        let mut field_indexes = match output_projection {
            Some(projection) => projection.to_vec(),
            None => (0..field_count).collect(),
        };
        for field_index in filter.referenced_field_indexes() {
            if !field_indexes.contains(&field_index) {
                field_indexes.push(field_index);
            }
        }
        for primary_key_index in primary_key_indexes {
            if *primary_key_index >= field_count {
                return Err(FlussLakeError::Internal(format!(
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

    fn arrow_schema(&self, row_type: &RowType) -> Result<arrow::datatypes::SchemaRef> {
        let schema = fluss::record::to_arrow_schema(row_type).map_err(|error| {
            FlussLakeError::Internal(format!(
                "failed to convert the physical read schema to Arrow: {error}"
            ))
        })?;
        schema
            .project(&self.field_indexes)
            .map(Arc::new)
            .map_err(|error| {
                FlussLakeError::Internal(format!(
                    "failed to project the physical UnionRead schema: {error}"
                ))
            })
    }
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
    current_view: &mut DeduplicateCurrentView,
) -> Result<i64> {
    let mut next_offset = next_offset;
    // Records of one poll arrive grouped by their backing Arrow batch, so
    // runs are detected by batch identity (address comparison only — the
    // records keep every batch alive for the whole loop).
    let mut run_source: Option<*const RecordBatch> = None;
    let mut run_batch: Option<RecordBatch> = None;
    let mut run_rows: Vec<usize> = Vec::new();
    let mut run_change_types: Vec<ChangeType> = Vec::new();

    for record in records {
        if record.offset() < next_offset {
            continue;
        }
        if next_offset >= stop_offset {
            break;
        }
        if record.offset() != next_offset {
            return Err(FlussLakeError::DataUnavailable(format!(
                "bounded changelog tail expected offset {next_offset}, but the next returned record was at offset {}; the frozen range [{}, {}) is no longer complete",
                record.offset(),
                next_offset,
                stop_offset
            )));
        }
        let row = record.row();
        let batch = row.get_record_batch().ok_or_else(|| {
            FlussLakeError::Internal(
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
                current_view,
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
        current_view,
    )?;
    Ok(next_offset)
}

fn flush_tail_run(
    run_batch: &mut Option<RecordBatch>,
    run_rows: &mut Vec<usize>,
    run_change_types: &mut Vec<ChangeType>,
    current_view: &mut DeduplicateCurrentView,
) -> Result<()> {
    let Some(batch) = run_batch.take() else {
        return Ok(());
    };
    if run_rows.is_empty() {
        return Ok(());
    }
    // The polled batch may cover offsets outside the frozen range, so only
    // the selected rows are folded, in offset order.
    let selected = take_rows(&batch, run_rows)?;
    current_view
        .fold_changelog_batch(selected, run_change_types)
        .map_err(reconciliation_error)?;
    run_rows.clear();
    run_change_types.clear();
    Ok(())
}

fn take_rows(batch: &RecordBatch, row_indexes: &[usize]) -> Result<RecordBatch> {
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
        FlussLakeError::Internal(format!("failed to select changelog tail rows: {error}"))
    })
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
) -> Result<()> {
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
fn lazy_stream<F>(setup: F) -> RecordBatchStream
where
    F: Future<Output = Result<RecordBatchStream>> + Send + 'static,
{
    Box::pin(futures::stream::once(setup).try_flatten())
}

/// Reconciles a source-neutral deduplicate state with a lake baseline stream.
fn reconciled_stream(
    current_view: DeduplicateCurrentView,
    baseline_stream: RecordBatchStream,
) -> RecordBatchStream {
    enum Phase {
        Baseline {
            current_view: DeduplicateCurrentView,
            stream: RecordBatchStream,
        },
        Survivors(DeduplicateCurrentView),
        Done,
    }

    Box::pin(futures::stream::try_unfold(
        Phase::Baseline {
            current_view,
            stream: baseline_stream,
        },
        |phase| async move {
            let mut phase = phase;
            loop {
                match phase {
                    Phase::Baseline {
                        current_view,
                        mut stream,
                    } => match stream.next().await {
                        Some(Ok(batch)) => {
                            match current_view
                                .reconcile_baseline_batch(&batch)
                                .map_err(reconciliation_error)?
                            {
                                Some(batch) => {
                                    return Ok(Some((
                                        batch,
                                        Phase::Baseline {
                                            current_view,
                                            stream,
                                        },
                                    )));
                                }
                                None => {
                                    phase = Phase::Baseline {
                                        current_view,
                                        stream,
                                    };
                                }
                            }
                        }
                        Some(Err(error)) => return Err(error),
                        None => phase = Phase::Survivors(current_view),
                    },
                    Phase::Survivors(current_view) => {
                        return match current_view.finish().map_err(reconciliation_error)? {
                            Some(batch) => Ok(Some((batch, Phase::Done))),
                            None => Ok(None),
                        };
                    }
                    Phase::Done => return Ok(None),
                }
            }
        },
    ))
}

fn reconciliation_error(error: ClientError) -> FlussLakeError {
    FlussLakeError::Internal(format!(
        "failed to reconcile the primary-key current view: {error}"
    ))
}

fn execution_client_error(action: &str, error: ClientError) -> FlussLakeError {
    if matches!(
        error.api_error(),
        Some(
            fluss::error::FlussError::TableNotExist
                | fluss::error::FlussError::UnknownTableOrBucketException
                | fluss::error::FlussError::PartitionNotExists
                | fluss::error::FlussError::LakeSnapshotNotExist
                | fluss::error::FlussError::KvSnapshotNotExist
        )
    ) {
        return FlussLakeError::DataUnavailable(format!("failed to {action}: {error}"));
    }
    match error {
        ClientError::LogOffsetOutOfRange { .. } => {
            FlussLakeError::DataUnavailable(format!("failed to {action}: {error}"))
        }
        ClientError::RpcError { .. } => {
            FlussLakeError::ConnectionError(format!("failed to {action}: {error}"))
        }
        _ => FlussLakeError::Internal(format!("failed to {action}: {error}")),
    }
}

/// Restates every source batch under the physical schema frozen from Fluss
/// metadata, rejecting name or type drift before any row is emitted.
///
/// Paimon and Fluss may differ in field metadata or nullability, which does not
/// change the physical values. Names, order, and Arrow data types must still
/// agree so that `plan.schema()` remains the schema of every output batch.
fn normalize_stream_schema(
    stream: RecordBatchStream,
    expected_schema: arrow::datatypes::SchemaRef,
) -> RecordBatchStream {
    Box::pin(stream.and_then(move |batch| {
        let expected_schema = expected_schema.clone();
        async move { normalize_batch_schema(batch, expected_schema) }
    }))
}

fn normalize_batch_schema(
    batch: RecordBatch,
    expected_schema: arrow::datatypes::SchemaRef,
) -> Result<RecordBatch> {
    if batch.schema_ref() == &expected_schema {
        return Ok(batch);
    }
    if batch.num_columns() != expected_schema.fields().len() {
        return Err(FlussLakeError::SchemaIncompatible(format!(
            "UnionRead source produced {} columns, but the frozen physical schema requires {}",
            batch.num_columns(),
            expected_schema.fields().len()
        )));
    }
    for (position, expected) in expected_schema.fields().iter().enumerate() {
        let actual = batch.schema_ref().field(position);
        if actual.name() != expected.name() || actual.data_type() != expected.data_type() {
            return Err(FlussLakeError::SchemaIncompatible(format!(
                "UnionRead source column {position} is {}:{}, but the frozen physical schema requires {}:{}",
                actual.name(),
                actual.data_type(),
                expected.name(),
                expected.data_type()
            )));
        }
    }
    RecordBatch::try_new(expected_schema, batch.columns().to_vec()).map_err(|error| {
        FlussLakeError::SchemaIncompatible(format!(
            "failed to restate a UnionRead source batch under the frozen physical schema: {error}"
        ))
    })
}

/// Applies exact filtering and projection before enforcing output batch size.
fn apply_output_processing(
    stream: RecordBatchStream,
    filter: &BoundPredicate,
    output_column_count: Option<usize>,
    batch_size: Option<usize>,
) -> RecordBatchStream {
    let stream = apply_filter_and_projection(stream, filter, output_column_count);
    match batch_size {
        Some(batch_size) => resize_batches(stream, batch_size),
        None => stream,
    }
}

/// Applies the exact predicate before stripping hidden physical columns.
fn apply_filter_and_projection(
    stream: RecordBatchStream,
    filter: &BoundPredicate,
    output_column_count: Option<usize>,
) -> RecordBatchStream {
    if matches!(filter, BoundPredicate::AlwaysTrue) && output_column_count.is_none() {
        return stream;
    }
    let filter = Arc::new(filter.clone());
    Box::pin(stream.try_filter_map(move |batch| {
        let filter = Arc::clone(&filter);
        async move {
            let filtered = if matches!(filter.as_ref(), BoundPredicate::AlwaysTrue) {
                batch
            } else {
                let mask = filter
                    .evaluate_batch(&batch)
                    .map_err(predicate_evaluation_error)?;
                let true_count = mask.true_count();
                if true_count == 0 {
                    return Ok(None);
                }
                if true_count == batch.num_rows() {
                    batch
                } else {
                    filter_record_batch(&batch, &mask).map_err(|error| {
                        FlussLakeError::Internal(format!(
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
                    FlussLakeError::Internal(format!(
                        "failed to strip hidden UnionRead columns after filtering: {error}"
                    ))
                });
            }
            Ok(Some(filtered))
        }
    }))
}

/// Slices oversized batches so every emitted batch contains at most
/// `batch_size` rows.
///
/// Small source batches are intentionally not coalesced. This keeps the
/// transform lazy and bounded while still honoring the scan's output limit.
fn resize_batches(stream: RecordBatchStream, batch_size: usize) -> RecordBatchStream {
    debug_assert!(batch_size > 0);
    Box::pin(futures::stream::try_unfold(
        (stream, None::<(RecordBatch, usize)>),
        move |(mut stream, mut pending)| async move {
            loop {
                if let Some((batch, offset)) = pending.take() {
                    if offset < batch.num_rows() {
                        let row_count = batch_size.min(batch.num_rows() - offset);
                        let next_offset = offset + row_count;
                        let next_pending = (next_offset < batch.num_rows())
                            .then_some((batch.clone(), next_offset));
                        return Ok(Some((
                            batch.slice(offset, row_count),
                            (stream, next_pending),
                        )));
                    }
                }

                match stream.next().await {
                    Some(Ok(batch)) if batch.num_rows() == 0 => {}
                    Some(Ok(batch)) => pending = Some((batch, 0)),
                    Some(Err(error)) => return Err(error),
                    None => return Ok(None),
                }
            }
        },
    ))
}

fn predicate_evaluation_error(error: ClientError) -> FlussLakeError {
    match error {
        ClientError::IllegalArgument { message } => FlussLakeError::SchemaIncompatible(message),
        error => {
            FlussLakeError::Internal(format!("failed to evaluate UnionRead predicate: {error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluss::metadata::{DataField, DataTypes};
    use fluss::predicate::{Predicate, col};
    use futures::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn bound(predicate: Predicate) -> BoundPredicate {
        BoundPredicate::bind(Some(&predicate), &pk_row_type()).unwrap()
    }

    /// Setup work must not run until the stream is polled, and its failure
    /// must arrive as the first stream item rather than from `execute`.
    #[test]
    fn lazy_stream_defers_setup_until_the_first_poll() {
        let setup_ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&setup_ran);
        let mut stream = lazy_stream(async move {
            flag.store(true, Ordering::SeqCst);
            Err(FlussLakeError::Internal("deferred failure".to_string()))
        });

        assert!(!setup_ran.load(Ordering::SeqCst));

        let first = futures::executor::block_on(stream.next());

        assert!(setup_ran.load(Ordering::SeqCst));
        assert!(matches!(first, Some(Err(FlussLakeError::Internal(_)))));
        assert!(futures::executor::block_on(stream.next()).is_none());
    }

    fn pk_row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("region", DataTypes::string(), None),
            DataField::new("amount", DataTypes::bigint(), None),
        ])
    }

    #[test]
    fn physical_projection_keeps_predicate_columns_hidden_from_output() {
        let physical = PhysicalProjection::resolve(
            &pk_row_type(),
            Some(&[2]),
            &bound(col("id").eq(1_i32)),
            &[0, 1],
        )
        .unwrap();

        assert_eq!(physical.field_indexes, vec![2, 0, 1]);
        assert_eq!(physical.key_positions, vec![1, 2]);
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
        let source: RecordBatchStream =
            Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(batch)]));
        let filtered = apply_filter_and_projection(source, &bound(col("id").gt(2_i32)), None);

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
        let source: RecordBatchStream =
            Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(batch)]));
        let filtered = apply_filter_and_projection(source, &bound(col("id").gt(5_i32)), None);

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
        let source: RecordBatchStream =
            Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(batch)]));
        let filtered = apply_filter_and_projection(source, &bound(col("id").eq(2_i32)), Some(1));

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
        let source: RecordBatchStream =
            Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(batch)]));
        let filtered = apply_filter_and_projection(source, &bound(col("id").eq(1_i32)), None);

        let result = futures::executor::block_on(filtered.try_collect::<Vec<RecordBatch>>());

        assert!(matches!(result, Err(FlussLakeError::SchemaIncompatible(_))));
    }

    #[test]
    fn source_batches_are_restated_under_the_frozen_physical_schema() {
        use arrow::array::Int32Array;
        use arrow::datatypes::{DataType, Field, Schema};

        let expected = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)]));
        let source = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, true)])),
            vec![Arc::new(Int32Array::from(vec![1, 2])) as arrow::array::ArrayRef],
        )
        .unwrap();

        let normalized = normalize_batch_schema(source, expected.clone()).unwrap();

        assert_eq!(normalized.schema_ref(), &expected);
    }

    #[test]
    fn source_schema_type_drift_is_rejected_before_output() {
        use arrow::array::Int64Array;
        use arrow::datatypes::{DataType, Field, Schema};

        let expected = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)]));
        let source = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)])),
            vec![Arc::new(Int64Array::from(vec![1, 2])) as arrow::array::ArrayRef],
        )
        .unwrap();

        assert!(matches!(
            normalize_batch_schema(source, expected),
            Err(FlussLakeError::SchemaIncompatible(_))
        ));
    }

    #[test]
    fn output_batch_size_slices_oversized_batches_after_filtering() {
        use arrow::array::Int32Array;
        use futures::stream;

        let batch = RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5])) as arrow::array::ArrayRef,
        )])
        .unwrap();
        let source: RecordBatchStream =
            Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(batch)]));
        let resized = apply_output_processing(source, &bound(col("id").gt(1_i32)), None, Some(3));

        let batches: Vec<RecordBatch> = futures::executor::block_on(resized.try_collect()).unwrap();

        assert_eq!(
            batches
                .iter()
                .map(RecordBatch::num_rows)
                .collect::<Vec<_>>(),
            vec![3, 1]
        );
        let first = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let second = batches[1]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(first.values(), &[2, 3, 4]);
        assert_eq!(second.values(), &[5]);
    }

    #[test]
    fn output_batch_size_does_not_coalesce_small_batches() {
        use arrow::array::Int32Array;
        use futures::stream;

        let first = RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(Int32Array::from(vec![1, 2])) as arrow::array::ArrayRef,
        )])
        .unwrap();
        let second = RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(Int32Array::from(vec![3])) as arrow::array::ArrayRef,
        )])
        .unwrap();
        let source: RecordBatchStream = Box::pin(stream::iter(vec![
            Ok::<_, FlussLakeError>(first),
            Ok::<_, FlussLakeError>(second),
        ]));
        let resized = resize_batches(source, 3);

        let batches: Vec<RecordBatch> = futures::executor::block_on(resized.try_collect()).unwrap();

        assert_eq!(
            batches
                .iter()
                .map(RecordBatch::num_rows)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
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

    #[test]
    fn missing_planned_table_or_partition_maps_to_data_unavailable() {
        for error in [
            fluss::error::FlussError::TableNotExist,
            fluss::error::FlussError::UnknownTableOrBucketException,
            fluss::error::FlussError::PartitionNotExists,
        ] {
            assert!(matches!(
                execution_client_error(
                    "open frozen split",
                    ClientError::FlussAPIError {
                        api_error: error.to_api_error(None),
                    },
                ),
                FlussLakeError::DataUnavailable(_)
            ));
        }
    }

    #[test]
    fn changelog_offset_gap_invalidates_the_frozen_plan() {
        use arrow::array::{ArrayRef, Int32Array, StringArray};
        use arrow::datatypes::{DataType, Field, Schema};
        use fluss::record::{ArrowReader, ScanRecord};

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("region", DataType::Utf8, false),
            Field::new("amount", DataType::Int64, false),
        ]));
        let batch = Arc::new(
            RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(Int32Array::from(vec![1])) as ArrayRef,
                    Arc::new(StringArray::from(vec!["US"])) as ArrayRef,
                    Arc::new(arrow::array::Int64Array::from(vec![10])) as ArrayRef,
                ],
            )
            .unwrap(),
        );
        let reader = ArrowReader::new(batch, Arc::new(pk_row_type())).unwrap();
        let records = vec![ScanRecord::new(
            reader.read(0),
            6,
            0,
            ChangeType::UpdateAfter,
        )];
        let mut current_view = DeduplicateCurrentView::try_new(schema, vec![0]).unwrap();

        assert!(matches!(
            fold_tail_records(&records, 5, 7, &mut current_view),
            Err(FlussLakeError::DataUnavailable(_))
        ));
    }
}
