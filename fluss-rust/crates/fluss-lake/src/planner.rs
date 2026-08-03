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
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::planning::{
    create_append_log_task, create_pk_hybrid_task, freeze_read_boundary_for_table,
};
use crate::pruning::PartitionPruner;
use crate::{
    UnionReadError, UnionReadMode, UnionReadPlan, UnionReadPlanFuture, UnionReadPlanner,
    UnionReadRequest, UnionReadResult, UnionReadStatistics,
};
use fluss::client::FlussConnection;
use fluss::metadata::{RowType, TableInfo};
use fluss::predicate::{FieldRef, PruningPredicate};
use fluss::record::to_arrow_schema;
use std::collections::HashSet;
use std::sync::Arc;

/// Fluss table property selecting a non-default primary-key merge engine.
const FLUSS_MERGE_ENGINE_PROPERTY: &str = "table.merge-engine";

/// Default Fluss-Rust planner for bounded UnionRead requests.
///
/// Append tables plan lake-split plus append-log tasks; primary-key tables
/// plan one hybrid task per bucket that merges the lake baseline with the
/// bounded changelog tail. Configurations whose current view cannot be
/// reproduced correctly — non-deduplicate merge engines and partitioned
/// primary-key tables — are rejected explicitly at planning.
#[derive(Clone)]
pub struct FlussUnionReadPlanner {
    connection: Arc<FlussConnection>,
}

impl FlussUnionReadPlanner {
    pub fn new(connection: Arc<FlussConnection>) -> Self {
        Self { connection }
    }
}

impl UnionReadPlanner for FlussUnionReadPlanner {
    fn plan(&self, request: UnionReadRequest) -> UnionReadPlanFuture<'_> {
        let connection = self.connection.clone();
        Box::pin(async move { plan_union_read(connection, request).await })
    }
}

async fn plan_union_read(
    connection: Arc<FlussConnection>,
    request: UnionReadRequest,
) -> UnionReadResult<UnionReadPlan> {
    validate_request_shape(&request)?;
    let admin = connection.get_admin().map_err(|error| {
        UnionReadError::Planning(format!("failed to create Fluss admin client: {error}"))
    })?;
    let table_info = admin
        .get_table_info(request.table_path())
        .await
        .map_err(|error| {
            UnionReadError::Planning(format!(
                "failed to get table metadata for {}: {error}",
                request.table_path()
            ))
        })?;
    validate_request_schema(&request, table_info.row_type())?;

    let output_schema = projected_arrow_schema(&request, table_info.row_type())?;

    if table_info.has_primary_key() {
        return plan_pk_union_read(&admin, &request, &table_info, output_schema).await;
    }

    let pruner = PartitionPruner::new(
        table_info.row_type(),
        &table_info.partition_keys,
        request.predicates(),
    );
    let predicate_decisions = pruner.decisions(request.predicates());
    let partition_filter = |partition_info: &fluss::metadata::PartitionInfo| {
        pruner.partition_may_match(partition_info.get_resolved_partition_spec())
    };
    let boundary = freeze_read_boundary_for_table(
        &admin,
        request.table_path(),
        &table_info,
        Some(&partition_filter),
    )
    .await?;

    // A readable lake snapshot contributes the bulk of the data; the frozen
    // bucket ranges already start at the snapshot's log offsets, so the log
    // tail below covers exactly what the snapshot does not.
    let mut tasks = Vec::new();
    if let Some(snapshot_id) = boundary.readable_lake_snapshot_id() {
        tasks.extend(plan_lake_split_tasks(&request, &table_info, snapshot_id).await?);
    }

    if request.read_mode() == UnionReadMode::LakeOnly {
        return Ok(UnionReadPlan::new(
            output_schema,
            tasks,
            UnionReadStatistics::default(),
            predicate_decisions,
        ));
    }

    for bucket_range in boundary.bucket_ranges() {
        if bucket_range.is_empty() {
            continue;
        }
        tasks.push(create_append_log_task(
            request.table_path(),
            table_info.schema_id,
            bucket_range,
            request.output_projection(),
            UnionReadStatistics::default(),
        )?);
    }

    Ok(UnionReadPlan::new(
        output_schema,
        tasks,
        UnionReadStatistics::default(),
        predicate_decisions,
    ))
}

/// Plans the immutable tasks covering one readable lake snapshot.
///
/// Splits are resolved and frozen here so that executors never re-plan the lake
/// snapshot and never observe a later lake commit.
#[cfg(feature = "paimon")]
async fn plan_lake_split_tasks(
    request: &UnionReadRequest,
    table_info: &fluss::metadata::TableInfo,
    snapshot_id: i64,
) -> UnionReadResult<Vec<crate::UnionReadTask>> {
    use crate::paimon::{PaimonCatalogOptions, plan_snapshot_splits, projected_field_names};
    use crate::planning::create_lake_split_task;
    use fluss::metadata::DataLakeFormat;

    let lake_format = table_info
        .table_config
        .get_datalake_format()
        .map_err(|error| {
            UnionReadError::Planning(format!(
                "failed to resolve the lake format of {}: {error}",
                request.table_path()
            ))
        })?
        .ok_or_else(|| {
            UnionReadError::Planning(format!(
                "table {} has readable lake snapshot {snapshot_id} but no configured lake format",
                request.table_path()
            ))
        })?;
    if lake_format != DataLakeFormat::Paimon {
        return Err(UnionReadError::Planning(format!(
            "lake split planning is not implemented for the {lake_format} format of {}",
            request.table_path()
        )));
    }

    let catalog_options = PaimonCatalogOptions::from_table_info(table_info)?;
    let projected_fields =
        projected_field_names(table_info.row_type(), request.output_projection())?;
    let encoded_splits = plan_snapshot_splits(
        request.table_path(),
        &catalog_options,
        snapshot_id,
        Some(&projected_fields),
    )
    .await?;

    // Only the non-sensitive options may travel inside tasks; secrets are
    // re-supplied at execution time through the execution context. The
    // BTreeMap keeps task encoding deterministic.
    let task_options: std::collections::BTreeMap<String, String> =
        catalog_options.non_sensitive().into_iter().collect();
    encoded_splits
        .into_iter()
        .enumerate()
        .map(|(split_index, encoded_split)| {
            create_lake_split_task(
                request.table_path(),
                snapshot_id,
                task_options.clone(),
                Some(projected_fields.clone()),
                encoded_split,
                split_index,
                UnionReadStatistics::default(),
            )
        })
        .collect()
}

#[cfg(not(feature = "paimon"))]
async fn plan_lake_split_tasks(
    request: &UnionReadRequest,
    _table_info: &fluss::metadata::TableInfo,
    snapshot_id: i64,
) -> UnionReadResult<Vec<crate::UnionReadTask>> {
    Err(UnionReadError::Planning(format!(
        "table {} has readable lake snapshot {snapshot_id}, but this build has no lake format feature enabled",
        request.table_path()
    )))
}

/// Plans a bounded primary-key read as one hybrid task per bucket.
///
/// Each task carries the bucket's complete lake baseline (all of its splits)
/// together with its frozen changelog range: the merge is per-bucket, and
/// splitting by lake file boundaries would leave the tail without a
/// consistent partner. Lake-only mode instead exposes the splits directly —
/// Paimon's current view as of the frozen snapshot, a legitimately stale
/// view by documented semantics.
async fn plan_pk_union_read(
    admin: &fluss::client::FlussAdmin,
    request: &UnionReadRequest,
    table_info: &TableInfo,
    output_schema: arrow::datatypes::SchemaRef,
) -> UnionReadResult<UnionReadPlan> {
    validate_pk_table_shape(table_info)?;

    // Without partitions there is nothing to prune, so every predicate stays
    // an engine residual; for PK tables data-column filters must be applied
    // after the merge anyway (a filtered-out UPDATE_AFTER must still
    // suppress its older lake row).
    let pruner = PartitionPruner::new(table_info.row_type(), &[], request.predicates());
    let predicate_decisions = pruner.decisions(request.predicates());

    let pk_indexes = table_info.schema.primary_key_indexes();
    if pk_indexes.is_empty() {
        return Err(UnionReadError::Planning(format!(
            "table {} reports a primary key but resolves no key field indexes",
            request.table_path()
        )));
    }

    let boundary =
        freeze_read_boundary_for_table(admin, request.table_path(), table_info, None).await?;
    let lake_side = match boundary.readable_lake_snapshot_id() {
        Some(snapshot_id) => Some(plan_pk_lake_side(request, table_info, snapshot_id).await?),
        None => None,
    };

    if request.read_mode() == UnionReadMode::LakeOnly {
        let tasks = match lake_side {
            Some(lake_side) => lake_side.into_lake_only_tasks(request)?,
            None => Vec::new(),
        };
        return Ok(UnionReadPlan::new(
            output_schema,
            tasks,
            UnionReadStatistics::default(),
            predicate_decisions,
        ));
    }

    let (snapshot_id, mut splits_by_bucket, task_options) = match lake_side {
        Some(lake_side) => (
            Some(lake_side.snapshot_id),
            lake_side.splits_by_bucket,
            lake_side.task_options,
        ),
        None => (
            None,
            std::collections::HashMap::new(),
            std::collections::BTreeMap::new(),
        ),
    };

    let mut tasks = Vec::new();
    for bucket_range in boundary.bucket_ranges() {
        let lake_splits = splits_by_bucket
            .remove(&bucket_range.table_bucket().bucket_id())
            .unwrap_or_default();
        if bucket_range.is_empty() && lake_splits.is_empty() {
            continue;
        }
        tasks.push(create_pk_hybrid_task(
            request.table_path(),
            table_info.schema_id,
            bucket_range,
            snapshot_id,
            task_options.clone(),
            lake_splits,
            pk_indexes.clone(),
            request.output_projection(),
            UnionReadStatistics::default(),
        )?);
    }
    if !splits_by_bucket.is_empty() {
        // A lake bucket outside the server's bucket set has no log partner;
        // dropping it would silently lose rows.
        let mut orphaned: Vec<i32> = splits_by_bucket.keys().copied().collect();
        orphaned.sort_unstable();
        return Err(UnionReadError::Planning(format!(
            "lake snapshot of {} contains splits for buckets {orphaned:?} that the server bucket set does not cover",
            request.table_path()
        )));
    }

    Ok(UnionReadPlan::new(
        output_schema,
        tasks,
        UnionReadStatistics::default(),
        predicate_decisions,
    ))
}

/// Rejects primary-key configurations whose current view v1 cannot rebuild.
///
/// These are correctness gates, not capability gaps to work around: planning
/// must fail explicitly rather than silently produce a wrong view.
fn validate_pk_table_shape(table_info: &TableInfo) -> UnionReadResult<()> {
    if !table_info.partition_keys.is_empty() {
        return Err(UnionReadError::Planning(format!(
            "partitioned primary-key table {} is not supported: per-partition lake split filtering is not implemented, and merging a partial lake view would silently produce an incorrect result",
            table_info.table_path
        )));
    }
    if let Some(merge_engine) = table_info.properties.get(FLUSS_MERGE_ENGINE_PROPERTY) {
        return Err(UnionReadError::Planning(format!(
            "primary-key UnionRead only supports default deduplicate semantics, but table {} sets {FLUSS_MERGE_ENGINE_PROPERTY}={merge_engine}; its changelog cannot be folded last-writer-wins without producing an incorrect view",
            table_info.table_path
        )));
    }
    Ok(())
}

/// The frozen lake half of a primary-key plan.
#[cfg(feature = "paimon")]
struct PkLakeSide {
    snapshot_id: i64,
    splits_by_bucket: std::collections::HashMap<i32, Vec<String>>,
    task_options: std::collections::BTreeMap<String, String>,
    projected_fields: Vec<String>,
}

#[cfg(feature = "paimon")]
impl PkLakeSide {
    /// Converts the lake side into standalone lake-split tasks.
    ///
    /// Only valid for lake-only mode: with no tail to merge, splits are
    /// independently correct (key-disjoint since apache/paimon-rust#374) and
    /// can be scheduled individually.
    fn into_lake_only_tasks(
        self,
        request: &UnionReadRequest,
    ) -> UnionReadResult<Vec<crate::UnionReadTask>> {
        use crate::planning::create_lake_split_task;

        let mut buckets: Vec<i32> = self.splits_by_bucket.keys().copied().collect();
        buckets.sort_unstable();
        let mut tasks = Vec::new();
        let mut split_index = 0;
        for bucket in buckets {
            for encoded_split in &self.splits_by_bucket[&bucket] {
                tasks.push(create_lake_split_task(
                    request.table_path(),
                    self.snapshot_id,
                    self.task_options.clone(),
                    Some(self.projected_fields.clone()),
                    encoded_split.clone(),
                    split_index,
                    UnionReadStatistics::default(),
                )?);
                split_index += 1;
            }
        }
        Ok(tasks)
    }
}

/// Plans and freezes the Paimon side of a primary-key read.
#[cfg(feature = "paimon")]
async fn plan_pk_lake_side(
    request: &UnionReadRequest,
    table_info: &TableInfo,
    snapshot_id: i64,
) -> UnionReadResult<PkLakeSide> {
    use crate::paimon::{PaimonCatalogOptions, plan_pk_snapshot_splits, projected_field_names};
    use fluss::metadata::DataLakeFormat;

    let lake_format = table_info
        .table_config
        .get_datalake_format()
        .map_err(|error| {
            UnionReadError::Planning(format!(
                "failed to resolve the lake format of {}: {error}",
                request.table_path()
            ))
        })?
        .ok_or_else(|| {
            UnionReadError::Planning(format!(
                "table {} has readable lake snapshot {snapshot_id} but no configured lake format",
                request.table_path()
            ))
        })?;
    if lake_format != DataLakeFormat::Paimon {
        return Err(UnionReadError::Planning(format!(
            "lake split planning is not implemented for the {lake_format} format of {}",
            request.table_path()
        )));
    }

    let catalog_options = PaimonCatalogOptions::from_table_info(table_info)?;
    let splits_by_bucket =
        plan_pk_snapshot_splits(request.table_path(), &catalog_options, snapshot_id).await?;
    let projected_fields =
        projected_field_names(table_info.row_type(), request.output_projection())?;
    Ok(PkLakeSide {
        snapshot_id,
        splits_by_bucket,
        task_options: catalog_options.non_sensitive().into_iter().collect(),
        projected_fields,
    })
}

#[cfg(not(feature = "paimon"))]
struct PkLakeSide {
    snapshot_id: i64,
    splits_by_bucket: std::collections::HashMap<i32, Vec<String>>,
    task_options: std::collections::BTreeMap<String, String>,
}

#[cfg(not(feature = "paimon"))]
impl PkLakeSide {
    fn into_lake_only_tasks(
        self,
        _request: &UnionReadRequest,
    ) -> UnionReadResult<Vec<crate::UnionReadTask>> {
        unreachable!("a PkLakeSide is never constructed without a lake format feature")
    }
}

#[cfg(not(feature = "paimon"))]
async fn plan_pk_lake_side(
    request: &UnionReadRequest,
    _table_info: &TableInfo,
    snapshot_id: i64,
) -> UnionReadResult<PkLakeSide> {
    Err(UnionReadError::Planning(format!(
        "table {} has readable lake snapshot {snapshot_id}, but this build has no lake format feature enabled",
        request.table_path()
    )))
}

fn validate_request_shape(request: &UnionReadRequest) -> UnionReadResult<()> {
    if request.table_path().database().is_empty() || request.table_path().table().is_empty() {
        return Err(UnionReadError::InvalidRequest(
            "database and table names must not be empty".to_string(),
        ));
    }
    if request.target_parallelism() == Some(0) {
        return Err(UnionReadError::InvalidRequest(
            "target parallelism must be greater than zero".to_string(),
        ));
    }
    if request.output_projection().is_some_and(<[usize]>::is_empty) {
        return Err(UnionReadError::InvalidRequest(
            "output projection must not be empty when present".to_string(),
        ));
    }

    let mut predicate_ids = HashSet::with_capacity(request.predicates().len());
    for predicate in request.predicates() {
        if !predicate_ids.insert(predicate.id()) {
            return Err(UnionReadError::InvalidRequest(format!(
                "predicate id {} is duplicated",
                predicate.id().value()
            )));
        }
    }
    Ok(())
}

fn validate_request_schema(request: &UnionReadRequest, row_type: &RowType) -> UnionReadResult<()> {
    if let Some(projection) = request.output_projection() {
        let mut field_indexes = HashSet::with_capacity(projection.len());
        for field_index in projection {
            if *field_index >= row_type.fields().len() {
                return Err(UnionReadError::InvalidRequest(format!(
                    "output projection field index {field_index} exceeds table width {}",
                    row_type.fields().len()
                )));
            }
            if !field_indexes.insert(*field_index) {
                return Err(UnionReadError::InvalidRequest(format!(
                    "output projection contains duplicate field index {field_index}"
                )));
            }
        }
    }

    for predicate in request.predicates() {
        validate_predicate_fields(predicate.predicate(), row_type)?;
    }
    Ok(())
}

fn validate_predicate_fields(
    predicate: &PruningPredicate,
    row_type: &RowType,
) -> UnionReadResult<()> {
    match predicate {
        PruningPredicate::Comparison { field, .. }
        | PruningPredicate::NullCheck { field, .. }
        | PruningPredicate::In { field, .. } => validate_field_ref(field, row_type),
        PruningPredicate::And(children) | PruningPredicate::Or(children) => {
            for child in children {
                validate_predicate_fields(child, row_type)?;
            }
            Ok(())
        }
    }
}

fn validate_field_ref(field: &FieldRef, row_type: &RowType) -> UnionReadResult<()> {
    let table_field = row_type.fields().get(field.index()).ok_or_else(|| {
        UnionReadError::InvalidRequest(format!(
            "predicate field index {} exceeds table width {}",
            field.index(),
            row_type.fields().len()
        ))
    })?;
    if table_field.name() != field.name() || table_field.data_type() != field.data_type() {
        return Err(UnionReadError::InvalidRequest(format!(
            "predicate field {}:{} does not match resolved table field {}:{}",
            field.index(),
            field.name(),
            table_field.name(),
            table_field.data_type()
        )));
    }
    Ok(())
}

fn projected_arrow_schema(
    request: &UnionReadRequest,
    row_type: &RowType,
) -> UnionReadResult<arrow::datatypes::SchemaRef> {
    let schema = to_arrow_schema(row_type).map_err(|error| {
        UnionReadError::Planning(format!(
            "failed to convert schema for {} to Arrow: {error}",
            request.table_path()
        ))
    })?;
    match request.output_projection() {
        Some(projection) => schema.project(projection).map(Arc::new).map_err(|error| {
            UnionReadError::InvalidRequest(format!(
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
    use crate::{PredicateId, PredicateInput};
    use fluss::metadata::{DataField, DataTypes, Schema, TablePath};
    use fluss::predicate::{ComparisonOperator, FieldRef};
    use std::collections::HashMap;

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("name", DataTypes::string(), None),
        ])
    }

    fn predicate(id: u32, field: FieldRef) -> PredicateInput {
        PredicateInput::new(
            PredicateId::new(id),
            PruningPredicate::comparison(ComparisonOperator::Equal, field, 1_i32),
        )
    }

    #[test]
    fn rejects_invalid_request_shape() {
        assert!(matches!(
            validate_request_shape(
                &UnionReadRequest::new(fluss::metadata::TablePath::new("fluss", "orders"))
                    .with_target_parallelism(0)
            ),
            Err(UnionReadError::InvalidRequest(_))
        ));
        assert!(matches!(
            validate_request_shape(
                &UnionReadRequest::new(fluss::metadata::TablePath::new("fluss", "orders"))
                    .with_output_projection(Vec::new())
            ),
            Err(UnionReadError::InvalidRequest(_))
        ));

        let duplicate = predicate(7, FieldRef::new(0, "id", DataTypes::int()));
        assert!(matches!(
            validate_request_shape(
                &UnionReadRequest::new(fluss::metadata::TablePath::new("fluss", "orders"))
                    .with_predicates(vec![duplicate.clone(), duplicate])
            ),
            Err(UnionReadError::InvalidRequest(_))
        ));
    }

    #[test]
    fn validates_projection_and_predicate_schema_identity() {
        let valid = UnionReadRequest::new(fluss::metadata::TablePath::new("fluss", "orders"))
            .with_output_projection(vec![1])
            .with_predicates(vec![predicate(1, FieldRef::new(0, "id", DataTypes::int()))]);
        validate_request_schema(&valid, &row_type()).unwrap();

        let stale = UnionReadRequest::new(fluss::metadata::TablePath::new("fluss", "orders"))
            .with_predicates(vec![predicate(
                1,
                FieldRef::new(0, "old_id", DataTypes::int()),
            )]);
        assert!(matches!(
            validate_request_schema(&stale, &row_type()),
            Err(UnionReadError::InvalidRequest(_))
        ));
    }

    #[test]
    fn non_partitioned_planning_keeps_predicates_as_engine_residuals() {
        let request = UnionReadRequest::new(fluss::metadata::TablePath::new("fluss", "orders"))
            .with_predicates(vec![predicate(5, FieldRef::new(0, "id", DataTypes::int()))]);
        let pruner = PartitionPruner::new(&row_type(), &[], request.predicates());

        let decisions = pruner.decisions(request.predicates());

        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].predicate_id(), PredicateId::new(5));
        assert_eq!(
            decisions[0].level(),
            crate::PredicatePushdownLevel::Unsupported
        );
        assert!(decisions[0].level().requires_residual_evaluation());
    }

    /// Tasks are cached, logged and persisted by engines, so secret catalog
    /// options must not appear anywhere in the encoded task bytes.
    #[test]
    #[cfg(feature = "paimon")]
    fn secret_catalog_options_never_reach_encoded_task_bytes() {
        use crate::paimon::PaimonCatalogOptions;
        use crate::planning::create_lake_split_task;
        use std::collections::HashMap;

        let mut options = HashMap::new();
        options.insert("warehouse".to_string(), "s3://bucket/warehouse".to_string());
        options.insert("s3.access-key-id".to_string(), "AKID-VALUE".to_string());
        options.insert("s3.secret-key".to_string(), "TOP-SECRET-VALUE".to_string());
        let catalog_options = PaimonCatalogOptions::from_map(options);

        // Mirror the planner's task construction path.
        let task_options: std::collections::BTreeMap<String, String> =
            catalog_options.non_sensitive().into_iter().collect();
        let task = create_lake_split_task(
            &fluss::metadata::TablePath::new("fluss", "orders"),
            42,
            task_options,
            Some(vec!["id".to_string(), "name".to_string()]),
            "{\"snapshotId\":42}".to_string(),
            0,
            crate::UnionReadStatistics::default(),
        )
        .unwrap();

        let encoded = task.encode().unwrap();
        for needle in [
            b"s3.secret-key".as_slice(),
            b"TOP-SECRET-VALUE".as_slice(),
            b"s3.access-key-id".as_slice(),
            b"AKID-VALUE".as_slice(),
        ] {
            assert!(
                !encoded.windows(needle.len()).any(|window| window == needle),
                "encoded task bytes must not contain {:?}",
                String::from_utf8_lossy(needle)
            );
        }
        assert!(
            encoded
                .windows(b"warehouse".len())
                .any(|window| window == b"warehouse"),
            "non-sensitive options must still travel in the task"
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
    fn accepts_default_primary_key_table_shape() {
        validate_pk_table_shape(&pk_table_info(Vec::new(), HashMap::new())).unwrap();
    }

    /// Merging a partial lake view against a per-partition log tail would
    /// silently drop rows; partitioned PK tables must fail at planning.
    #[test]
    fn rejects_partitioned_primary_key_tables() {
        let table_info = pk_table_info(vec!["region".to_string()], HashMap::new());

        assert!(matches!(
            validate_pk_table_shape(&table_info),
            Err(UnionReadError::Planning(_))
        ));
    }

    /// A non-default Fluss merge engine changes changelog semantics, so a
    /// last-writer-wins fold would produce an incorrect current view.
    #[test]
    fn rejects_fluss_merge_engine_tables() {
        let mut properties = HashMap::new();
        properties.insert("table.merge-engine".to_string(), "first_row".to_string());
        let table_info = pk_table_info(Vec::new(), properties);

        assert!(matches!(
            validate_pk_table_shape(&table_info),
            Err(UnionReadError::Planning(_))
        ));
    }
}
