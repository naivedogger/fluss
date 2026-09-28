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
use crate::planning::freeze_read_boundary_for_table;
use crate::pruning::PartitionPruner;
use crate::split::SplitStatistics;
use crate::table::{FlussLakeScan, validate_lake_readable};
use crate::{
    FlussLakeError, FlussLakeReadContext, FlussLakeReadPlan, LakePlannerContext, LakeReadSemantics,
    LakeSource, Result,
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
    use crate::planning::create_logical_split;

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
        let include_log_tail = !scan.lake_only();
        if planned_lake_bucket.splits.is_empty() && (!include_log_tail || bucket_range.is_empty()) {
            continue;
        }
        let statistics = split_statistics(bucket_range, include_log_tail, &planned_lake_bucket);
        splits.push(create_logical_split(
            scan.table_path(),
            table_info.schema_id,
            bucket_range,
            snapshot_id,
            planned_lake_bucket.splits,
            primary_key_indexes.clone(),
            statistics,
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
        let statistics = SplitStatistics::new(
            planned_lake_bucket.estimated_rows,
            planned_lake_bucket.estimated_size,
        );
        splits.push(create_logical_split(
            scan.table_path(),
            table_info.schema_id,
            &lake_only_range,
            snapshot_id,
            planned_lake_bucket.splits,
            primary_key_indexes.clone(),
            statistics,
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

fn partition_sort_key(partition: &crate::FlussLakePartitionIdentity) -> &[(String, String)] {
    match partition {
        crate::FlussLakePartitionIdentity::Unpartitioned => &[],
        crate::FlussLakePartitionIdentity::KeyValues(key_values) => key_values,
    }
}

fn validate_pk_union_merge_engine(table_info: &TableInfo) -> Result<()> {
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

fn physical_primary_key_indexes(table_info: &TableInfo) -> Result<Vec<usize>> {
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

fn resolve_lake_source(scan: &FlussLakeScan, info: &TableInfo) -> Result<Arc<dyn LakeSource>> {
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
    use fluss::metadata::{DataTypes, Schema, TablePath};
    use std::collections::HashMap;

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
        let schema = Schema::builder()
            .column("id", DataTypes::int())
            .column("region", DataTypes::string())
            .column("amount", DataTypes::bigint())
            .primary_key(["id", "region"])
            .unwrap()
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
