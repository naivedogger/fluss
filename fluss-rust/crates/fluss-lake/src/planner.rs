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
use crate::union_read::{FlussLakePlanFuture, FlussLakePlanner, FlussLakeScanSpec};
use crate::{
    FlussLakeError, FlussLakePlanStatistics, FlussLakeReadMode, FlussLakeReadPlan,
    FlussLakeReadStatistics, FlussLakeResult,
};
use fluss::client::FlussConnection;
use fluss::metadata::{RowType, TableInfo};
use fluss::record::to_arrow_schema;
use std::collections::HashSet;
use std::sync::Arc;

/// Fluss table property selecting a primary-key merge engine.
const FLUSS_MERGE_ENGINE_PROPERTY: &str = "table.merge-engine";

/// Default Fluss-Rust planner for bounded UnionRead requests.
#[derive(Clone)]
pub(crate) struct FlussUnionReadPlanner {
    connection: Arc<FlussConnection>,
    table_property_overrides: std::collections::HashMap<String, String>,
}

impl FlussUnionReadPlanner {
    pub(crate) fn new(
        connection: Arc<FlussConnection>,
        table_property_overrides: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            connection,
            table_property_overrides,
        }
    }
}

impl FlussLakePlanner for FlussUnionReadPlanner {
    fn plan(&self, request: FlussLakeScanSpec) -> FlussLakePlanFuture<'_> {
        let connection = self.connection.clone();
        let overrides = self.table_property_overrides.clone();
        Box::pin(async move { plan_union_read(connection, overrides, request).await })
    }
}

async fn plan_union_read(
    connection: Arc<FlussConnection>,
    table_property_overrides: std::collections::HashMap<String, String>,
    request: FlussLakeScanSpec,
) -> FlussLakeResult<FlussLakeReadPlan> {
    use crate::planning::create_logical_split;

    validate_request_shape(&request)?;
    let admin = connection.get_admin().map_err(|error| {
        FlussLakeError::PlanningFailed(format!("failed to create Fluss admin client: {error}"))
    })?;
    let table_info = admin
        .get_table_info(request.table_path())
        .await
        .map_err(|error| {
            FlussLakeError::PlanningFailed(format!(
                "failed to get table metadata for {}: {error}",
                request.table_path()
            ))
        })?;
    validate_request_schema(&request, table_info.row_type())?;
    let output_schema = projected_arrow_schema(&request, table_info.row_type())?;

    let data_lake_format = table_info
        .table_config
        .get_datalake_format()
        .map_err(|error| {
            FlussLakeError::PlanningFailed(format!(
                "failed to resolve the lake format of {}: {error}",
                request.table_path()
            ))
        })?;
    let partition_pruner = PartitionPruner::new(
        table_info.row_type(),
        &table_info.partition_keys,
        request.filter(),
    );
    let partition_filter = |partition_info: &fluss::metadata::PartitionInfo| {
        partition_pruner.partition_may_match(partition_info.get_resolved_partition_spec())
    };
    let boundary = freeze_read_boundary_for_table(
        &admin,
        request.table_path(),
        &table_info,
        Some(&partition_filter),
    )
    .await?;
    let bucket_pruner = BucketPruner::new(
        table_info.row_type(),
        &table_info.bucket_keys,
        table_info.num_buckets,
        data_lake_format,
        request.filter(),
    );

    let is_primary_key = table_info.has_primary_key();
    if is_primary_key && request.read_mode() == FlussLakeReadMode::Union {
        validate_pk_union_merge_engine(&table_info)?;
    }
    let primary_key_indexes = if is_primary_key {
        let indexes = table_info.schema.primary_key_indexes();
        if indexes.is_empty() {
            return Err(FlussLakeError::PlanningFailed(format!(
                "table {} reports a primary key but resolves no key field indexes",
                request.table_path()
            )));
        }
        indexes
    } else {
        Vec::new()
    };

    let lake_side = match boundary.readable_lake_snapshot_id() {
        Some(snapshot_id) => Some(
            plan_lake_side(
                &request,
                &table_info,
                &table_property_overrides,
                snapshot_id,
                is_primary_key && request.read_mode() == FlussLakeReadMode::Union,
            )
            .await?,
        ),
        None => None,
    };
    if request.read_mode() == FlussLakeReadMode::LakeOnly && lake_side.is_none() {
        return Ok(FlussLakeReadPlan::new(
            output_schema,
            Vec::new(),
            FlussLakePlanStatistics::new(0),
        ));
    }

    let (snapshot_id, split_options, mut lake_splits) = match lake_side {
        Some(side) => (Some(side.snapshot_id), side.split_options, side.splits),
        None => (
            None,
            std::collections::BTreeMap::new(),
            std::collections::HashMap::new(),
        ),
    };

    let mut splits = Vec::new();
    for bucket_range in boundary.bucket_ranges() {
        let bucket_id = bucket_range.table_bucket().bucket_id();
        if !bucket_pruner.bucket_may_match(bucket_id) {
            continue;
        }
        #[cfg(feature = "paimon")]
        let partition_path = crate::paimon::encode_partition_qualified_name(
            bucket_range.partition_qualified_name().unwrap_or_default(),
        );
        #[cfg(not(feature = "paimon"))]
        let partition_path = bucket_range
            .partition_qualified_name()
            .unwrap_or_default()
            .to_string();
        let physical_lake_splits = lake_splits
            .remove(&(partition_path, bucket_id))
            .unwrap_or_default();
        let include_log_tail = request.read_mode() == FlussLakeReadMode::Union;
        if physical_lake_splits.is_empty() && (!include_log_tail || bucket_range.is_empty()) {
            continue;
        }
        splits.push(create_logical_split(
            request.table_path(),
            table_info.schema_id,
            bucket_range,
            snapshot_id,
            split_options.clone(),
            physical_lake_splits,
            primary_key_indexes.clone(),
            FlussLakeReadStatistics::default(),
        )?);
    }
    let split_count = splits.len();
    Ok(FlussLakeReadPlan::new(
        output_schema,
        splits,
        FlussLakePlanStatistics::new(split_count),
    ))
}

fn validate_pk_union_merge_engine(table_info: &TableInfo) -> FlussLakeResult<()> {
    if let Some(merge_engine) = table_info.properties.get(FLUSS_MERGE_ENGINE_PROPERTY) {
        let normalized = merge_engine.replace('_', "-").to_ascii_lowercase();
        if normalized != "deduplicate" {
            return Err(FlussLakeError::UnsupportedMergeEngine(format!(
                "primary-key UnionRead only supports deduplicate semantics, but table {} sets {FLUSS_MERGE_ENGINE_PROPERTY}={merge_engine}",
                table_info.table_path
            )));
        }
    }
    Ok(())
}

#[cfg(feature = "paimon")]
struct PlannedLakeSide {
    snapshot_id: i64,
    splits: std::collections::HashMap<(String, i32), Vec<String>>,
    split_options: std::collections::BTreeMap<String, String>,
}

#[cfg(feature = "paimon")]
async fn plan_lake_side(
    request: &FlussLakeScanSpec,
    table_info: &TableInfo,
    table_property_overrides: &std::collections::HashMap<String, String>,
    snapshot_id: i64,
    validate_merge_engine: bool,
) -> FlussLakeResult<PlannedLakeSide> {
    use crate::paimon::{PaimonCatalogOptions, plan_snapshot_splits};
    use fluss::metadata::DataLakeFormat;

    let lake_format = table_info
        .table_config
        .get_datalake_format()
        .map_err(|error| {
            FlussLakeError::PlanningFailed(format!(
                "failed to resolve the lake format of {}: {error}",
                request.table_path()
            ))
        })?
        .ok_or_else(|| {
            FlussLakeError::PlanningFailed(format!(
                "table {} has readable lake snapshot {snapshot_id} but no configured lake format",
                request.table_path()
            ))
        })?;
    if lake_format != DataLakeFormat::Paimon {
        return Err(FlussLakeError::PlanningFailed(format!(
            "lake split planning is not implemented for the {lake_format} format of {}",
            request.table_path()
        )));
    }

    let catalog_options =
        PaimonCatalogOptions::from_table_info_with_overrides(table_info, table_property_overrides)?;
    let splits = plan_snapshot_splits(
        request.table_path(),
        &catalog_options,
        snapshot_id,
        validate_merge_engine,
    )
    .await?;
    Ok(PlannedLakeSide {
        snapshot_id,
        splits,
        split_options: catalog_options.non_sensitive().into_iter().collect(),
    })
}

#[cfg(not(feature = "paimon"))]
struct PlannedLakeSide {
    snapshot_id: i64,
    splits: std::collections::HashMap<(String, i32), Vec<String>>,
    split_options: std::collections::BTreeMap<String, String>,
}

#[cfg(not(feature = "paimon"))]
async fn plan_lake_side(
    request: &FlussLakeScanSpec,
    _table_info: &TableInfo,
    _table_property_overrides: &std::collections::HashMap<String, String>,
    snapshot_id: i64,
    _validate_merge_engine: bool,
) -> FlussLakeResult<PlannedLakeSide> {
    Err(FlussLakeError::PlanningFailed(format!(
        "table {} has readable lake snapshot {snapshot_id}, but this build has no lake format feature enabled",
        request.table_path()
    )))
}

fn validate_request_shape(request: &FlussLakeScanSpec) -> FlussLakeResult<()> {
    if request.table_path().database().is_empty() || request.table_path().table().is_empty() {
        return Err(FlussLakeError::InvalidRequest(
            "database and table names must not be empty".to_string(),
        ));
    }
    if request.target_parallelism() == Some(0) {
        return Err(FlussLakeError::InvalidRequest(
            "target parallelism must be greater than zero".to_string(),
        ));
    }
    if request.output_projection().is_some_and(<[usize]>::is_empty) {
        return Err(FlussLakeError::InvalidRequest(
            "output projection must not be empty when present".to_string(),
        ));
    }
    Ok(())
}

fn validate_request_schema(request: &FlussLakeScanSpec, row_type: &RowType) -> FlussLakeResult<()> {
    if let Some(projection) = request.output_projection() {
        let mut field_indexes = HashSet::with_capacity(projection.len());
        for field_index in projection {
            if *field_index >= row_type.fields().len() {
                return Err(FlussLakeError::InvalidRequest(format!(
                    "output projection field index {field_index} exceeds table width {}",
                    row_type.fields().len()
                )));
            }
            if !field_indexes.insert(*field_index) {
                return Err(FlussLakeError::InvalidRequest(format!(
                    "output projection contains duplicate field index {field_index}"
                )));
            }
        }
    }

    request.filter().validate_columns(row_type)?;
    Ok(())
}

fn projected_arrow_schema(
    request: &FlussLakeScanSpec,
    row_type: &RowType,
) -> FlussLakeResult<arrow::datatypes::SchemaRef> {
    let schema = to_arrow_schema(row_type).map_err(|error| {
        FlussLakeError::PlanningFailed(format!(
            "failed to convert schema for {} to Arrow: {error}",
            request.table_path()
        ))
    })?;
    match request.output_projection() {
        Some(projection) => schema.project(projection).map(Arc::new).map_err(|error| {
            FlussLakeError::InvalidRequest(format!(
                "failed to project output schema for {}: {error}",
                request.table_path()
            ))
        }),
        None => Ok(schema),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlussLakePredicate;
    use fluss::metadata::{DataField, DataTypes, Schema, TablePath};
    use std::collections::HashMap;

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("name", DataTypes::string(), None),
        ])
    }

    #[test]
    fn rejects_invalid_request_shape() {
        assert!(matches!(
            validate_request_shape(
                &FlussLakeScanSpec::new(fluss::metadata::TablePath::new("fluss", "orders"))
                    .with_target_parallelism(0)
            ),
            Err(FlussLakeError::InvalidRequest(_))
        ));
        assert!(matches!(
            validate_request_shape(
                &FlussLakeScanSpec::new(fluss::metadata::TablePath::new("fluss", "orders"))
                    .with_output_projection(Vec::new())
            ),
            Err(FlussLakeError::InvalidRequest(_))
        ));
    }

    #[test]
    fn validates_projection_and_predicate_schema_identity() {
        let valid = FlussLakeScanSpec::new(fluss::metadata::TablePath::new("fluss", "orders"))
            .with_output_projection(vec![1])
            .with_filter(FlussLakePredicate::eq("id", 1_i32));
        validate_request_schema(&valid, &row_type()).unwrap();

        let stale = FlussLakeScanSpec::new(fluss::metadata::TablePath::new("fluss", "orders"))
            .with_filter(FlussLakePredicate::eq("missing", 1_i32));
        assert!(matches!(
            validate_request_schema(&stale, &row_type()),
            Err(FlussLakeError::InvalidRequest(_))
        ));
    }

    /// Splits are cached, logged and persisted by engines, so secret catalog
    /// options must not appear anywhere in the encoded split bytes.
    #[test]
    #[cfg(feature = "paimon")]
    fn secret_catalog_options_never_reach_encoded_split_bytes() {
        use crate::paimon::PaimonCatalogOptions;
        use crate::split_descriptor::{LogicalSplitDescriptor, SplitDescriptor};
        use fluss::metadata::TableBucket;

        let mut options = HashMap::new();
        options.insert("warehouse".to_string(), "s3://bucket/warehouse".to_string());
        options.insert("s3.access-key-id".to_string(), "AKID-VALUE".to_string());
        options.insert("s3.secret-key".to_string(), "TOP-SECRET-VALUE".to_string());
        let catalog_options = PaimonCatalogOptions::from_map(options);

        let split_options: std::collections::BTreeMap<String, String> =
            catalog_options.non_sensitive().into_iter().collect();
        let descriptor = SplitDescriptor::Logical(
            LogicalSplitDescriptor::try_new(
                fluss::metadata::TablePath::new("fluss", "orders"),
                1,
                TableBucket::new(7, 0),
                0,
                0,
                Some(42),
                split_options,
                vec!["{\"snapshotId\":42}".to_string()],
                Vec::new(),
            )
            .unwrap(),
        );
        let split = crate::FlussLakeReadSplit::try_new(
            "fluss.orders:root:0".to_string(),
            0,
            None,
            crate::CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor.encode().unwrap(),
            crate::FlussLakeReadStatistics::default(),
        )
        .unwrap();

        let encoded = split.encode().unwrap();
        for needle in [
            b"s3.secret-key".as_slice(),
            b"TOP-SECRET-VALUE".as_slice(),
            b"s3.access-key-id".as_slice(),
            b"AKID-VALUE".as_slice(),
        ] {
            assert!(
                !encoded.windows(needle.len()).any(|window| window == needle),
                "encoded split bytes must not contain {:?}",
                String::from_utf8_lossy(needle)
            );
        }
        assert!(
            encoded
                .windows(b"warehouse".len())
                .any(|window| window == b"warehouse"),
            "non-sensitive options must still travel in the split"
        );
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
    fn accepts_default_and_explicit_deduplicate_merge_engines() {
        validate_pk_union_merge_engine(&pk_table_info(Vec::new(), HashMap::new())).unwrap();

        let mut properties = HashMap::new();
        properties.insert("table.merge-engine".to_string(), "deduplicate".to_string());
        validate_pk_union_merge_engine(&pk_table_info(Vec::new(), properties)).unwrap();
    }

    #[test]
    fn accepts_partitioned_primary_key_tables() {
        let table_info = pk_table_info(vec!["region".to_string()], HashMap::new());

        validate_pk_union_merge_engine(&table_info).unwrap();
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
}
