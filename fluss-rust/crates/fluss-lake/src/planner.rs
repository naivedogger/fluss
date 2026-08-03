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

use crate::planning::{create_append_log_task, freeze_read_boundary_for_table};
use crate::pruning::PartitionPruner;
use crate::{
    UnionReadError, UnionReadMode, UnionReadPlan, UnionReadPlanFuture, UnionReadPlanner,
    UnionReadRequest, UnionReadResult, UnionReadStatistics,
};
use fluss::client::FlussConnection;
use fluss::metadata::RowType;
use fluss::predicate::{FieldRef, PruningPredicate};
use fluss::record::to_arrow_schema;
use std::collections::HashSet;
use std::sync::Arc;

/// Default Fluss-Rust planner for bounded UnionRead requests.
///
/// The initial implementation plans append-table log tasks when no readable
/// lake snapshot exists. Planning a readable lake snapshot or a primary-key
/// table remains unsupported until the corresponding Paimon and merge
/// executors are available.
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

    if table_info.has_primary_key() {
        return Err(UnionReadError::Planning(format!(
            "primary-key UnionRead planning is not implemented for {}",
            request.table_path()
        )));
    }

    let output_schema = projected_arrow_schema(&request, table_info.row_type())?;
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
        projected_fields.as_deref(),
    )
    .await?;

    let ordered_options: std::collections::BTreeMap<String, String> = catalog_options
        .as_map()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    encoded_splits
        .into_iter()
        .enumerate()
        .map(|(split_index, encoded_split)| {
            create_lake_split_task(
                request.table_path(),
                snapshot_id,
                ordered_options.clone(),
                projected_fields.clone(),
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
    use fluss::metadata::{DataField, DataTypes};
    use fluss::predicate::{ComparisonOperator, FieldRef};

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
}
