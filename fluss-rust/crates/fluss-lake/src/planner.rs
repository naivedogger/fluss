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
use crate::{FlussLakeError, FlussLakePlanStatistics, FlussLakeReadPlan, Result};
use fluss::error::Error as ClientError;
use fluss::metadata::{RowType, TableInfo};
use fluss::predicate::BoundPredicate;
use fluss::record::to_arrow_schema;
use std::sync::Arc;

pub(crate) async fn plan_union_read(scan: &FlussLakeScan) -> Result<FlussLakeReadPlan> {
    use crate::planning::create_logical_split;

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
    let output_projection = scan.resolve_projection(table_info.row_type())?;
    let filter = BoundPredicate::bind(scan.filter(), table_info.row_type()).map_err(|error| {
        FlussLakeError::PlanningFailed(format!("failed to bind filter predicate: {error}"))
    })?;
    let output_schema = projected_arrow_schema(
        scan.table_path(),
        output_projection.as_deref(),
        table_info.row_type(),
    )?;

    let data_lake_format = table_info
        .table_config
        .get_datalake_format()
        .map_err(|error| {
            FlussLakeError::PlanningFailed(format!(
                "failed to resolve the lake format of {}: {error}",
                scan.table_path()
            ))
        })?;
    let partition_pruner =
        PartitionPruner::new(table_info.row_type(), &table_info.partition_keys, &filter);
    let partition_filter = |partition_info: &fluss::metadata::PartitionInfo| {
        partition_pruner.partition_may_match(partition_info.get_resolved_partition_spec())
    };
    let boundary = freeze_read_boundary_for_table(
        &admin,
        scan.table_path(),
        &table_info,
        Some(&partition_filter),
    )
    .await?;
    let bucket_pruner = BucketPruner::new(
        table_info.row_type(),
        &table_info.bucket_keys,
        table_info.num_buckets,
        data_lake_format,
        &filter,
    );

    let is_primary_key = table_info.has_primary_key();
    if is_primary_key && !scan.lake_only() {
        validate_pk_union_merge_engine(&table_info)?;
    }
    let primary_key_indexes = if is_primary_key {
        let indexes = physical_primary_key_indexes(&table_info)?;
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

    let lake_side = match boundary.readable_lake_snapshot_id() {
        Some(snapshot_id) => Some(
            plan_lake_side(
                scan,
                &table_info,
                scan.catalog_property_overrides(),
                snapshot_id,
                is_primary_key && !scan.lake_only(),
                &filter,
            )
            .await?,
        ),
        None => None,
    };
    if scan.lake_only() && lake_side.is_none() {
        return Ok(FlussLakeReadPlan::new(
            output_schema,
            Vec::new(),
            FlussLakePlanStatistics::from_splits(&[]),
        ));
    }

    let (snapshot_id, mut lake_splits) = match lake_side {
        Some(side) => (Some(side.snapshot_id), side.splits),
        None => (None, std::collections::HashMap::new()),
    };

    let mut splits = Vec::new();
    for bucket_range in boundary.bucket_ranges() {
        let bucket_id = bucket_range.table_bucket().bucket_id();
        if !bucket_pruner.bucket_may_match(bucket_id) {
            continue;
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
                .cmp(&partition_sort_key(right_partition))
                .then_with(|| left_bucket.cmp(right_bucket))
        },
    );
    for ((partition, bucket_id), planned_lake_bucket) in lake_only_buckets {
        if bucket_id < 0 || bucket_id >= table_info.num_buckets {
            return Err(FlussLakeError::PlanningFailed(format!(
                "lake snapshot {} of {} contains bucket {bucket_id}, outside the configured range [0, {})",
                snapshot_id.unwrap_or_default(),
                scan.table_path(),
                table_info.num_buckets
            )));
        }
        if !bucket_pruner.bucket_may_match(bucket_id)
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
    let statistics = FlussLakePlanStatistics::from_splits(&splits);
    Ok(FlussLakeReadPlan::new(output_schema, splits, statistics))
}

fn partition_sort_key(partition: &crate::FlussLakePartitionIdentity) -> String {
    match partition {
        crate::FlussLakePartitionIdentity::Unpartitioned => String::new(),
        crate::FlussLakePartitionIdentity::KeyValues(key_values) => key_values
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("/"),
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

#[cfg(feature = "paimon")]
struct PlannedLakeSide {
    snapshot_id: i64,
    splits: std::collections::HashMap<(crate::FlussLakePartitionIdentity, i32), PlannedLakeBucket>,
}

#[derive(Debug)]
struct PlannedLakeBucket {
    splits: Vec<String>,
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

#[cfg(feature = "paimon")]
async fn plan_lake_side(
    scan: &FlussLakeScan,
    table_info: &TableInfo,
    table_property_overrides: &std::collections::HashMap<String, String>,
    snapshot_id: i64,
    validate_merge_engine: bool,
    filter: &BoundPredicate,
) -> Result<PlannedLakeSide> {
    use crate::paimon::{
        ExpectedPaimonLayout, PaimonCatalogOptions, lake_pushdown_filter, plan_snapshot_splits,
    };
    use fluss::metadata::DataLakeFormat;

    let lake_format = table_info
        .table_config
        .get_datalake_format()
        .map_err(|error| {
            FlussLakeError::PlanningFailed(format!(
                "failed to resolve the lake format of {}: {error}",
                scan.table_path()
            ))
        })?
        .ok_or_else(|| {
            FlussLakeError::PlanningFailed(format!(
                "table {} has readable lake snapshot {snapshot_id} but no configured lake format",
                scan.table_path()
            ))
        })?;
    if lake_format != DataLakeFormat::Paimon {
        return Err(FlussLakeError::PlanningFailed(format!(
            "lake split planning is not implemented for the {lake_format} format of {}",
            scan.table_path()
        )));
    }

    let catalog_options =
        PaimonCatalogOptions::from_table_info_with_overrides(table_info, table_property_overrides)?;
    let lake_filter = lake_pushdown_filter(filter, table_info, validate_merge_engine);
    let splits = plan_snapshot_splits(
        scan.table_path(),
        &catalog_options,
        snapshot_id,
        ExpectedPaimonLayout {
            partition_keys: &table_info.partition_keys,
            primary_keys: &table_info.primary_keys,
            bucket_keys: &table_info.bucket_keys,
            num_buckets: table_info.num_buckets,
        },
        validate_merge_engine,
        lake_filter.as_ref(),
    )
    .await?
    .into_iter()
    .map(|(key, bucket)| {
        (
            key,
            PlannedLakeBucket {
                splits: bucket.splits,
                estimated_rows: bucket.estimated_rows,
                estimated_size: bucket.estimated_size,
            },
        )
    })
    .collect();
    Ok(PlannedLakeSide {
        snapshot_id,
        splits,
    })
}

#[cfg(not(feature = "paimon"))]
struct PlannedLakeSide {
    snapshot_id: i64,
    splits: std::collections::HashMap<(crate::FlussLakePartitionIdentity, i32), PlannedLakeBucket>,
}

#[cfg(not(feature = "paimon"))]
async fn plan_lake_side(
    scan: &FlussLakeScan,
    _table_info: &TableInfo,
    _table_property_overrides: &std::collections::HashMap<String, String>,
    snapshot_id: i64,
    _validate_merge_engine: bool,
    _filter: &BoundPredicate,
) -> Result<PlannedLakeSide> {
    Err(FlussLakeError::PlanningFailed(format!(
        "table {} has readable lake snapshot {snapshot_id}, but this build has no lake format feature enabled",
        scan.table_path()
    )))
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

    /// Catalog configuration belongs to the scan-derived reader and must not
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
            vec!["{\"snapshotId\":42}".to_string()],
            Vec::new(),
        )
        .unwrap();
        let split = crate::FlussLakeReadSplit::try_new(
            "fluss.orders:root:0".to_string(),
            0,
            crate::FlussLakePartitionIdentity::Unpartitioned,
            crate::CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor.encode().unwrap(),
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
