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

//! Default Fluss-Rust planner for bounded UnionRead requests.

use crate::bucket_pruning::BucketPruner;
use crate::planning::{FrozenBucketRange, create_logical_split, freeze_read_boundary_for_table};
use crate::pruning::PartitionPruner;
use crate::split::SplitStatistics;
use crate::table::{FlussLakeScan, validate_lake_readable};
use crate::{
    FlussLakeError, FlussLakeReadContext, FlussLakeReadPlan, FlussLakeReadSplit,
    LakePlannerContext, LakeReadSemantics, LakeSource, Result,
};
use fluss::error::Error as ClientError;
use fluss::metadata::{RowType, TableInfo};
use fluss::predicate::BoundPredicate;
use fluss::record::to_arrow_schema;
use std::sync::Arc;

/// Prepares source state without opening a Paimon catalog or generating files.
/// The context captures all live partitions so it is reusable across filters.
pub(crate) async fn prepare_read_context(scan: &FlussLakeScan) -> Result<FlussLakeReadContext> {
    scan.validate_configuration()?;
    let admin = scan
        .connection()
        .get_admin()
        .map_err(|error| planning_client_error("create Fluss admin client", error))?;
    let table_info = admin
        .get_table_info(scan.table_path())
        .await
        .map_err(|error| planning_client_error("get table metadata", error))?;
    validate_lake_readable(&table_info)?;
    scan.resolve_projection(table_info.row_type())?;
    BoundPredicate::bind(scan.filter(), table_info.row_type()).map_err(|error| {
        FlussLakeError::PlanningFailed(format!("failed to bind filter predicate: {error}"))
    })?;
    let boundary = freeze_read_boundary_for_table(&admin, scan.table_path(), &table_info).await?;
    FlussLakeReadContext::from_boundary(&table_info, boundary)
}

pub(crate) async fn plan_union_read(scan: &FlussLakeScan) -> Result<FlussLakeReadPlan> {
    let context = prepare_read_context(scan).await?;
    plan_with_context(scan, &context).await
}

/// Binds scan-specific pruning and the default lake backend to fixed inputs.
/// Metadata is checked, but neither snapshots nor offset bounds are refreshed.
pub(crate) async fn plan_with_context(
    scan: &FlussLakeScan,
    context: &FlussLakeReadContext,
) -> Result<FlussLakeReadPlan> {
    scan.validate_configuration()?;
    if scan.table_path() != context.table_path() {
        return Err(FlussLakeError::InvalidReadContext(
            "scan and read context refer to different tables".to_string(),
        ));
    }
    let admin = scan
        .connection()
        .get_admin()
        .map_err(|error| planning_client_error("create Fluss admin client", error))?;
    let table_info = admin
        .get_table_info(scan.table_path())
        .await
        .map_err(|error| {
            if error.api_error() == Some(fluss::error::FlussError::TableNotExist) {
                FlussLakeError::DataUnavailable(format!(
                    "table {} from the frozen context no longer exists",
                    context.table_path()
                ))
            } else {
                planning_client_error("get table metadata", error)
            }
        })?;
    context.validate_table(&table_info)?;
    plan_prepared(scan, context, &table_info).await
}

async fn plan_prepared(
    scan: &FlussLakeScan,
    context: &FlussLakeReadContext,
    table_info: &TableInfo,
) -> Result<FlussLakeReadPlan> {
    let output_projection = scan.resolve_projection(context.table_schema().row_type())?;
    let filter = BoundPredicate::bind(scan.filter(), context.table_schema().row_type()).map_err(
        |error| FlussLakeError::PlanningFailed(format!("failed to bind filter predicate: {error}")),
    )?;
    let output_schema = projected_arrow_schema(
        scan.table_path(),
        output_projection.as_deref(),
        context.table_schema().row_type(),
    )?;
    let partition_pruner = PartitionPruner::new(
        context.table_schema().row_type(),
        context.partition_keys(),
        &filter,
    );
    let lake_format = table_info
        .table_config
        .get_datalake_format()
        .map_err(|error| FlussLakeError::PlanningFailed(format!("invalid lake format: {error}")))?;
    // Partitions retain their own hash modulus after a table-default change.
    // Cache once per distinct layout, rather than re-encoding keys per bucket.
    let mut bucket_pruners = std::collections::HashMap::new();
    let mut bucket_may_match = |bucket_id, bucket_count| {
        bucket_pruners
            .entry(bucket_count)
            .or_insert_with(|| {
                BucketPruner::new(
                    context.table_schema().row_type(),
                    context.bucket_keys(),
                    bucket_count,
                    &lake_format,
                    &filter,
                )
            })
            .bucket_may_match(bucket_id)
    };
    let is_primary_key = table_info.has_primary_key();
    if is_primary_key && !scan.lake_only() {
        validate_pk_union_merge_engine(table_info)?;
    }
    let primary_key_indexes = if is_primary_key {
        let indexes = physical_primary_key_indexes(table_info)?;
        if indexes.is_empty() {
            return Err(FlussLakeError::PlanningFailed(format!(
                "table {} reports a primary key but resolves no physical key field indexes",
                scan.table_path()
            )));
        }
        indexes
    } else {
        Vec::new()
    };

    let lake_source = if context.lake_snapshot_id().is_some() {
        Some(resolve_lake_source(scan, table_info)?)
    } else {
        None
    };
    let snapshot_id = context.lake_snapshot_id();
    let mut lake_splits = match (&lake_source, snapshot_id) {
        (Some(source), Some(snapshot_id)) => {
            let safe_filter = crate::source::lake_pushdown_filter(
                &filter,
                table_info,
                is_primary_key && !scan.lake_only(),
            )
            .unwrap_or(BoundPredicate::AlwaysTrue);
            let tasks = source
                .plan(LakePlannerContext {
                    table_info,
                    snapshot_id,
                    semantics: if is_primary_key {
                        LakeReadSemantics::PrimaryKey
                    } else {
                        LakeReadSemantics::Append
                    },
                    filter: &safe_filter,
                })
                .await?;
            group_lake_splits(
                tasks,
                source.format(),
                snapshot_id,
                table_info,
                context.log_ranges(),
            )?
        }
        _ => std::collections::HashMap::new(),
    };
    let mut splits = Vec::new();
    for bucket_range in context.log_ranges() {
        let bucket_id = bucket_range.table_bucket().bucket_id();
        if !bucket_may_match(bucket_id, bucket_range.bucket_count())
            || !partition_pruner.partition_identity_may_match(bucket_range.partition_identity())
        {
            continue;
        }
        if !scan.lake_only() {
            bucket_range.validate_available()?;
        }
        let planned_lake_bucket = lake_splits
            .remove(&(bucket_range.partition_identity().clone(), bucket_id))
            .unwrap_or_default();
        splits.extend(plan_bucket_splits(
            table_info,
            bucket_range,
            snapshot_id,
            planned_lake_bucket,
            &primary_key_indexes,
            scan.lake_only(),
        )?);
    }
    let mut lake_only_buckets: Vec<_> = lake_splits.into_iter().collect();
    lake_only_buckets.sort_by(
        |((left_partition, left_bucket), _), ((right_partition, right_bucket), _)| {
            partition_sort_key(left_partition)
                .cmp(partition_sort_key(right_partition))
                .then_with(|| left_bucket.cmp(right_bucket))
        },
    );
    for ((partition, bucket_id), planned_lake_bucket) in lake_only_buckets {
        // Nonempty groups have a validated, uniform lake-partition layout.
        let bucket_count = planned_lake_bucket.splits[0].bucket_count;
        if !bucket_may_match(bucket_id, bucket_count)
            || !partition_pruner.partition_identity_may_match(&partition)
        {
            continue;
        }
        if table_info.partition_keys.is_empty() {
            return Err(FlussLakeError::PlanningFailed(format!(
                "lake snapshot {} of the unpartitioned table {} contains an unmatched bucket {bucket_id}",
                snapshot_id.unwrap_or_default(),
                scan.table_path()
            )));
        }
        if matches!(partition, crate::FlussLakePartitionIdentity::Unpartitioned) {
            return Err(FlussLakeError::PlanningFailed(format!(
                "lake snapshot {} of the partitioned table {} contains an unpartitioned split",
                snapshot_id.unwrap_or_default(),
                scan.table_path()
            )));
        }
        let lake_only_range = crate::planning::FrozenBucketRange::lake_only(
            table_info.table_id,
            bucket_id,
            bucket_count,
            partition,
        );
        splits.extend(plan_bucket_splits(
            table_info,
            &lake_only_range,
            snapshot_id,
            planned_lake_bucket,
            &primary_key_indexes,
            true,
        )?);
    }
    FlussLakeReadPlan::new(
        context.clone(),
        output_schema,
        scan.clone(),
        lake_source,
        splits,
    )
}

/// Append lake tasks and log tails can be scheduled independently. Primary-key
/// reads keep the whole baseline and tail together for bucket-local reconciliation.
fn plan_bucket_splits(
    table_info: &TableInfo,
    bucket_range: &FrozenBucketRange,
    snapshot_id: Option<i64>,
    lake_bucket: PlannedLakeBucket,
    primary_key_indexes: &[usize],
    lake_only: bool,
) -> Result<Vec<FlussLakeReadSplit>> {
    let include_log_tail = !lake_only && !bucket_range.is_empty();
    if lake_bucket.splits.is_empty() && !include_log_tail {
        return Ok(Vec::new());
    }
    if !primary_key_indexes.is_empty() {
        let statistics = split_statistics(bucket_range, include_log_tail, &lake_bucket);
        return Ok(vec![create_logical_split(
            &table_info.table_path,
            table_info.schema_id,
            bucket_range,
            snapshot_id,
            lake_bucket.splits,
            primary_key_indexes.to_vec(),
            statistics,
        )?]);
    }

    let mut splits = Vec::with_capacity(lake_bucket.splits.len() + usize::from(include_log_tail));
    // Preserve live partition identity, but never attach its tail to a lake task.
    let mut lake_range = bucket_range.clone();
    lake_range.stop_offset = lake_range.start_offset;
    for (index, task) in lake_bucket.splits.into_iter().enumerate() {
        let statistics = SplitStatistics::new(task.estimated_rows, task.estimated_size);
        let mut split = create_logical_split(
            &table_info.table_path,
            table_info.schema_id,
            &lake_range,
            snapshot_id,
            vec![task],
            Vec::new(),
            statistics,
        )?;
        // Task indexes are local to this immutable plan, not a replan contract.
        split.split_id.push_str(&format!(":lake:{index}"));
        splits.push(split);
    }
    if include_log_tail {
        let mut split = create_logical_split(
            &table_info.table_path,
            table_info.schema_id,
            bucket_range,
            snapshot_id,
            Vec::new(),
            Vec::new(),
            SplitStatistics::new(bucket_range.estimated_log_rows(), None),
        )?;
        split.split_id.push_str(":log");
        splits.push(split);
    }
    Ok(splits)
}

fn partition_sort_key(partition: &crate::FlussLakePartitionIdentity) -> &[(String, String)] {
    match partition {
        crate::FlussLakePartitionIdentity::Unpartitioned => &[],
        crate::FlussLakePartitionIdentity::KeyValues(key_values) => key_values,
    }
}

pub(crate) fn validate_pk_union_merge_engine(table_info: &TableInfo) -> Result<()> {
    let merge_engine = table_info
        .table_config
        .get_merge_engine_type()
        .map_err(|error| {
            FlussLakeError::PlanningFailed(format!(
                "failed to resolve the merge engine of {}: {error}",
                table_info.table_path
            ))
        })?;
    if let Some(merge_engine) = merge_engine {
        return Err(FlussLakeError::UnsupportedMergeEngine(format!(
            "primary-key UnionRead only supports the default deduplicate semantics, but table {} uses table.merge-engine={merge_engine}",
            table_info.table_path
        )));
    }
    Ok(())
}

pub(crate) fn physical_primary_key_indexes(table_info: &TableInfo) -> Result<Vec<usize>> {
    table_info
        .get_physical_primary_keys()
        .iter()
        .map(|name| {
            table_info
                .row_type()
                .fields()
                .iter()
                .position(|field| field.name() == name)
                .ok_or_else(|| {
                    FlussLakeError::PlanningFailed(format!(
                        "physical primary-key column '{name}' is missing from table {}",
                        table_info.table_path
                    ))
                })
        })
        .collect()
}

#[derive(Debug)]
struct PlannedLakeBucket {
    splits: Vec<crate::LakeSplit>,
    estimated_rows: Option<usize>,
    estimated_size: Option<usize>,
}

impl Default for PlannedLakeBucket {
    fn default() -> Self {
        Self {
            splits: Vec::new(),
            estimated_rows: Some(0),
            estimated_size: Some(0),
        }
    }
}

pub(crate) fn resolve_lake_source(
    scan: &FlussLakeScan,
    info: &TableInfo,
) -> Result<Arc<dyn LakeSource>> {
    let format = info
        .table_config
        .get_datalake_format()
        .map_err(|e| FlussLakeError::PlanningFailed(e.to_string()))?
        .ok_or_else(|| FlussLakeError::NotLakeReadable("missing lake format".to_string()))?
        .to_string();
    if let Some(source) = &scan.lake_source {
        if source.format() != format {
            return Err(FlussLakeError::PlanningFailed(
                "LakeSource format does not match the table".to_string(),
            ));
        }
        return Ok(source.clone());
    }
    #[cfg(feature = "paimon")]
    if format == "paimon" {
        return Ok(Arc::new(crate::paimon::PaimonLakeSource::new(
            info,
            scan.catalog_property_overrides(),
        )?));
    }
    Err(FlussLakeError::PlanningFailed(format!(
        "no LakeSource for {format}; enable its feature or supply a custom source"
    )))
}

fn group_lake_splits(
    tasks: Vec<crate::LakeSplit>,
    format: &str,
    snapshot_id: i64,
    info: &TableInfo,
    log_ranges: &[crate::FlussLakeLogRange],
) -> Result<std::collections::HashMap<(crate::FlussLakePartitionIdentity, i32), PlannedLakeBucket>>
{
    let mut grouped = std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut bucket_counts: std::collections::HashMap<_, _> = log_ranges
        .iter()
        .map(|range| (range.partition_identity(), range.bucket_count()))
        .collect();
    if info.partition_keys.is_empty() {
        bucket_counts.insert(
            &crate::FlussLakePartitionIdentity::Unpartitioned,
            info.num_buckets,
        );
    }
    for task in &tasks {
        task.validate(format, snapshot_id)?;
        if let Some(count) = bucket_counts.insert(&task.partition, task.bucket_count)
            && count != task.bucket_count
        {
            return Err(FlussLakeError::PlanningFailed(
                "lake partition bucket count conflicts with its frozen Fluss layout or another lake task".to_string()
            ));
        }
        if !seen.insert((
            &task.partition,
            task.bucket_id,
            task.payload_version,
            &task.payload,
        )) {
            return Err(FlussLakeError::PlanningFailed(
                "lake planner returned a duplicate task".to_string(),
            ));
        }
        let valid_partition = match &task.partition {
            crate::FlussLakePartitionIdentity::Unpartitioned => info.partition_keys.is_empty(),
            crate::FlussLakePartitionIdentity::KeyValues(values) => {
                !values.is_empty()
                    && values.len() == info.partition_keys.len()
                    && values
                        .iter()
                        .zip(info.partition_keys.iter())
                        .all(|((name, _), expected)| name == expected)
            }
        };
        if !valid_partition {
            return Err(FlussLakeError::PlanningFailed(
                "lake split partition identity does not match the table layout".to_string(),
            ));
        }
    }
    // Validate borrowed payloads before moving them; do not duplicate every
    // file task's potentially large encoded metadata just for deduplication.
    drop(seen);
    for task in tasks {
        let bucket = grouped
            .entry((task.partition.clone(), task.bucket_id))
            .or_insert_with(PlannedLakeBucket::default);
        bucket.estimated_rows = add_estimates(bucket.estimated_rows, task.estimated_rows);
        bucket.estimated_size = add_estimates(bucket.estimated_size, task.estimated_size);
        bucket.splits.push(task);
    }
    Ok(grouped)
}

fn split_statistics(
    bucket_range: &crate::planning::FrozenBucketRange,
    include_log_tail: bool,
    lake_bucket: &PlannedLakeBucket,
) -> SplitStatistics {
    let log_rows = if include_log_tail {
        bucket_range.estimated_log_rows()
    } else {
        Some(0)
    };
    let log_size = if include_log_tail && !bucket_range.is_empty() {
        None
    } else {
        Some(0)
    };
    SplitStatistics::new(
        add_estimates(lake_bucket.estimated_rows, log_rows),
        add_estimates(lake_bucket.estimated_size, log_size),
    )
}

fn add_estimates(left: Option<usize>, right: Option<usize>) -> Option<usize> {
    left?.checked_add(right?)
}

fn planning_client_error(action: &str, error: ClientError) -> FlussLakeError {
    match error {
        ClientError::RpcError { .. } => {
            FlussLakeError::ConnectionError(format!("failed to {action}: {error}"))
        }
        _ => FlussLakeError::PlanningFailed(format!("failed to {action}: {error}")),
    }
}

fn projected_arrow_schema(
    table_path: &fluss::metadata::TablePath,
    projection: Option<&[usize]>,
    row_type: &RowType,
) -> Result<arrow::datatypes::SchemaRef> {
    let schema = to_arrow_schema(row_type).map_err(|error| {
        FlussLakeError::PlanningFailed(format!(
            "failed to convert schema for {} to Arrow: {error}",
            table_path
        ))
    })?;
    match projection {
        Some(projection) => schema.project(projection).map(Arc::new).map_err(|error| {
            FlussLakeError::PlanningFailed(format!(
                "failed to project output schema for {}: {error}",
                table_path
            ))
        }),
        None => Ok(schema),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluss::metadata::{DataTypes, Schema, TableBucket, TablePath};
    use std::collections::{HashMap, HashSet};

    fn lake_bucket(range: &FrozenBucketRange) -> PlannedLakeBucket {
        let splits = (0..2)
            .map(|index| {
                let mut task = crate::source::testing_split();
                task.partition = range.partition_identity.clone();
                task.bucket_id = range.table_bucket.bucket_id();
                task.bucket_count = range.bucket_count;
                task.payload = vec![index];
                task.estimated_rows = Some(3);
                task.estimated_size = Some(100);
                task
            })
            .collect();
        PlannedLakeBucket {
            splits,
            estimated_rows: Some(6),
            estimated_size: Some(200),
        }
    }

    #[test]
    fn append_tasks_cover_lake_and_tail_independently() {
        let info = table_info(false, vec![], HashMap::new());
        let range = FrozenBucketRange {
            table_bucket: TableBucket::new(info.table_id, 0),
            partition_identity: crate::FlussLakePartitionIdentity::Unpartitioned,
            bucket_count: 4,
            start_offset: 6,
            stop_offset: 10,
            earliest_offset: 0,
        };
        let splits =
            plan_bucket_splits(&info, &range, Some(42), lake_bucket(&range), &[], false).unwrap();
        assert_eq!(splits.len(), 3);
        assert_eq!(
            splits
                .iter()
                .map(|split| &split.split_id)
                .collect::<HashSet<_>>()
                .len(),
            3
        );
        for (index, split) in splits.iter().enumerate() {
            let restored: FlussLakeReadSplit =
                serde_json::from_slice(&serde_json::to_vec(split).unwrap()).unwrap();
            assert_eq!(&restored, split);
            let descriptor = restored.decode_execution_descriptor().unwrap();
            assert_eq!(descriptor.table_bucket(), range.table_bucket());
            assert_eq!(descriptor.snapshot_id(), Some(42));
            assert!(!descriptor.is_primary_key());
            assert!(!descriptor.is_empty());
            if index < 2 {
                assert_eq!(descriptor.start_offset(), 6);
                assert_eq!(descriptor.stop_offset(), 6);
                assert_eq!(descriptor.lake_splits().len(), 1);
                assert_eq!(descriptor.lake_splits()[0].payload, vec![index as u8]);
                assert_eq!(split.estimated_rows, Some(3));
                assert_eq!(split.estimated_size, Some(100));
            } else {
                assert_eq!(descriptor.start_offset(), 6);
                assert_eq!(descriptor.stop_offset(), 10);
                assert!(descriptor.lake_splits().is_empty());
                assert_eq!(split.estimated_rows, Some(4));
                assert_eq!(split.estimated_size, None);
            }
        }
        let stats = crate::FlussLakePlanStatistics::from_splits(&splits);
        assert_eq!(stats.estimated_rows, Some(10));
        assert_eq!(stats.estimated_size, None);
    }

    #[test]
    fn append_planning_omits_empty_work_and_respects_lake_only_mode() {
        let info = table_info(false, vec![], HashMap::new());
        for has_lake in [false, true] {
            for has_tail in [false, true] {
                for lake_only in [false, true] {
                    let range = FrozenBucketRange {
                        table_bucket: TableBucket::new(info.table_id, 0),
                        partition_identity: crate::FlussLakePartitionIdentity::Unpartitioned,
                        bucket_count: 4,
                        start_offset: if has_lake { 6 } else { 0 },
                        stop_offset: (if has_lake { 6 } else { 0 })
                            + (if has_tail { 4 } else { 0 }),
                        earliest_offset: 0,
                    };
                    let lake = if has_lake {
                        lake_bucket(&range)
                    } else {
                        PlannedLakeBucket::default()
                    };
                    let snapshot = has_lake.then_some(42);
                    let splits =
                        plan_bucket_splits(&info, &range, snapshot, lake, &[], lake_only).unwrap();
                    let expected_lake_tasks = if has_lake { 2 } else { 0 };
                    let expected_log_tasks = usize::from(has_tail && !lake_only);
                    assert_eq!(splits.len(), expected_lake_tasks + expected_log_tasks);
                    let log_tasks = splits
                        .iter()
                        .map(|split| split.decode_execution_descriptor().unwrap())
                        .filter(|descriptor| descriptor.start_offset() < descriptor.stop_offset())
                        .count();
                    assert_eq!(log_tasks, expected_log_tasks);
                    if lake_only && has_lake {
                        let stats = crate::FlussLakePlanStatistics::from_splits(&splits);
                        assert_eq!(stats.estimated_rows, Some(6));
                        assert_eq!(stats.estimated_size, Some(200));
                    }
                }
            }
        }
    }

    #[test]
    fn append_lake_tasks_preserve_live_and_expired_partition_identity() {
        let info = table_info(false, vec!["region".into()], HashMap::new());
        let mut all_ids = HashSet::new();
        for (partition_id, name) in [(Some(9), "live"), (None, "expired")] {
            let range = FrozenBucketRange {
                table_bucket: TableBucket::new_with_partition(info.table_id, partition_id, 7),
                partition_identity: crate::FlussLakePartitionIdentity::KeyValues(vec![(
                    "region".into(),
                    name.into(),
                )]),
                bucket_count: 8,
                start_offset: 0,
                stop_offset: 0,
                earliest_offset: 0,
            };
            let splits =
                plan_bucket_splits(&info, &range, Some(42), lake_bucket(&range), &[], false)
                    .unwrap();
            assert_eq!(splits.len(), 2);
            for split in splits {
                assert!(all_ids.insert(split.split_id.clone()));
                assert_eq!(split.partition, range.partition_identity);
                let descriptor = split.decode_execution_descriptor().unwrap();
                assert_eq!(descriptor.table_bucket().partition_id(), partition_id);
                assert_eq!(descriptor.lake_splits()[0].bucket_count, 8);
                assert_eq!(descriptor.start_offset(), descriptor.stop_offset());
            }
        }
    }

    #[test]
    fn primary_key_planning_keeps_bucket_baseline_and_tail_together() {
        let info = pk_table_info(vec![], HashMap::new());
        let range = FrozenBucketRange {
            table_bucket: TableBucket::new(info.table_id, 0),
            partition_identity: crate::FlussLakePartitionIdentity::Unpartitioned,
            bucket_count: 4,
            start_offset: 6,
            stop_offset: 10,
            earliest_offset: 0,
        };
        for lake_only in [false, true] {
            let splits = plan_bucket_splits(
                &info,
                &range,
                Some(42),
                lake_bucket(&range),
                &[0],
                lake_only,
            )
            .unwrap();
            assert_eq!(splits.len(), 1);
            let descriptor = splits[0].decode_execution_descriptor().unwrap();
            assert!(descriptor.is_primary_key());
            assert_eq!(descriptor.lake_splits().len(), 2);
            assert_eq!(descriptor.start_offset(), 6);
            assert_eq!(descriptor.stop_offset(), 10);
            assert_eq!(
                splits[0].estimated_rows,
                Some(if lake_only { 6 } else { 10 })
            );
            assert_eq!(
                splits[0].estimated_size,
                if lake_only { Some(200) } else { None }
            );
        }
    }

    #[test]
    fn backend_tasks_are_validated_before_grouping_or_pruning() {
        let info = pk_table_info(vec![], HashMap::new());
        let task = crate::source::testing_split();
        let mut second = task.clone();
        second.payload = b"another-task".to_vec();
        let groups =
            group_lake_splits(vec![task.clone(), second], "paimon", 42, &info, &[]).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups.values().next().unwrap().splits.len(), 2);
        assert!(
            group_lake_splits(vec![task.clone(), task.clone()], "paimon", 42, &info, &[]).is_err()
        );
        assert!(group_lake_splits(vec![task.clone()], "iceberg", 42, &info, &[]).is_err());
        assert!(group_lake_splits(vec![task.clone()], "paimon", 43, &info, &[]).is_err());
        let mut invalid = task.clone();
        invalid.bucket_id = info.num_buckets;
        assert!(group_lake_splits(vec![invalid], "paimon", 42, &info, &[]).is_err());
        let mut invalid = task;
        invalid.partition =
            crate::FlussLakePartitionIdentity::KeyValues(vec![("unknown".into(), "v".into())]);
        assert!(group_lake_splits(vec![invalid], "paimon", 42, &info, &[]).is_err());
    }

    #[test]
    fn lake_layout_uses_actual_live_and_expired_partition_counts() {
        let info = pk_table_info(vec!["region".into()], HashMap::new());
        let identity = |value: &str| {
            crate::FlussLakePartitionIdentity::KeyValues(vec![("region".into(), value.into())])
        };
        // The table default is 4, but this old live partition retains 8 buckets.
        let ranges = (0..8)
            .map(|bucket| crate::FlussLakeLogRange {
                table_bucket: fluss::metadata::TableBucket::new_with_partition(7, Some(9), bucket),
                partition_identity: identity("old"),
                bucket_count: 8,
                start_offset: 0,
                stop_offset: 1,
                earliest_offset: 0,
            })
            .collect::<Vec<_>>();
        let mut live = crate::source::testing_split();
        live.partition = identity("old");
        live.bucket_count = 8;
        live.bucket_id = 7;
        let mut expired = live.clone();
        expired.partition = identity("expired");
        let groups = group_lake_splits(
            vec![live.clone(), expired.clone()],
            "paimon",
            42,
            &info,
            &ranges,
        )
        .unwrap();
        assert_eq!(groups.len(), 2);
        // Neither a valid id nor a query filter excuses a wrong live layout.
        let mut wrong = live;
        wrong.bucket_count = 4;
        wrong.bucket_id = 0;
        assert!(group_lake_splits(vec![wrong], "paimon", 42, &info, &ranges).is_err());
        let mut conflicting = expired.clone();
        conflicting.bucket_count = 4;
        conflicting.bucket_id = 0;
        assert!(
            group_lake_splits(vec![expired, conflicting], "paimon", 42, &info, &ranges).is_err()
        );
    }

    /// Catalog configuration belongs to the plan-bound reader and must not
    /// be duplicated into distributable splits.
    #[test]
    #[cfg(feature = "paimon")]
    fn catalog_configuration_never_reaches_encoded_split_bytes() {
        use crate::split_descriptor::SplitDescriptor;
        use fluss::metadata::TableBucket;

        let descriptor = SplitDescriptor::try_new(
            fluss::metadata::TablePath::new("fluss", "orders"),
            1,
            false,
            TableBucket::new(7, 0),
            0,
            0,
            Some(42),
            vec![crate::source::testing_split()],
            Vec::new(),
        )
        .unwrap();
        let split = crate::FlussLakeReadSplit::try_new(
            "fluss.orders:root:0".to_string(),
            0,
            crate::FlussLakePartitionIdentity::Unpartitioned,
            crate::CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor,
            SplitStatistics::default(),
        )
        .unwrap();

        let encoded = serde_json::to_vec(&split).unwrap();
        for needle in [
            b"s3.secret-key".as_slice(),
            b"TOP-SECRET-VALUE".as_slice(),
            b"s3.access-key-id".as_slice(),
            b"AKID-VALUE".as_slice(),
            b"warehouse".as_slice(),
            b"s3://bucket/warehouse".as_slice(),
        ] {
            assert!(
                !encoded.windows(needle.len()).any(|window| window == needle),
                "encoded split bytes must not contain {:?}",
                String::from_utf8_lossy(needle)
            );
        }
    }

    fn pk_table_info(
        partition_keys: Vec<String>,
        properties: HashMap<String, String>,
    ) -> TableInfo {
        table_info(true, partition_keys, properties)
    }

    fn table_info(
        primary_key: bool,
        partition_keys: Vec<String>,
        properties: HashMap<String, String>,
    ) -> TableInfo {
        let builder = Schema::builder()
            .column("id", DataTypes::int())
            .column("region", DataTypes::string())
            .column("amount", DataTypes::bigint());
        let schema = if primary_key {
            builder.primary_key(["id", "region"]).unwrap()
        } else {
            builder
        }
        .build()
        .unwrap();
        TableInfo::new(
            TablePath::new("fluss", "pk_orders"),
            7,
            1,
            schema,
            vec!["id".to_string()],
            partition_keys.into(),
            4,
            properties,
            HashMap::new(),
            None,
            0,
            0,
        )
    }

    #[test]
    fn accepts_default_deduplicate_merge_semantics() {
        validate_pk_union_merge_engine(&pk_table_info(Vec::new(), HashMap::new())).unwrap();
    }

    #[test]
    fn accepts_partitioned_primary_key_tables() {
        let table_info = pk_table_info(vec!["region".to_string()], HashMap::new());

        validate_pk_union_merge_engine(&table_info).unwrap();
        assert_eq!(physical_primary_key_indexes(&table_info).unwrap(), vec![0]);
    }

    #[test]
    fn rejects_unsupported_fluss_merge_engine_tables_with_typed_error() {
        let mut properties = HashMap::new();
        properties.insert("table.merge-engine".to_string(), "first_row".to_string());
        let table_info = pk_table_info(Vec::new(), properties);

        assert!(matches!(
            validate_pk_union_merge_engine(&table_info),
            Err(FlussLakeError::UnsupportedMergeEngine(_))
        ));
    }

    #[test]
    fn malformed_fluss_merge_engine_is_a_planning_error() {
        let mut properties = HashMap::new();
        properties.insert("table.merge-engine".to_string(), "deduplicate".to_string());
        let table_info = pk_table_info(Vec::new(), properties);

        assert!(matches!(
            validate_pk_union_merge_engine(&table_info),
            Err(FlussLakeError::PlanningFailed(_))
        ));
    }
}
