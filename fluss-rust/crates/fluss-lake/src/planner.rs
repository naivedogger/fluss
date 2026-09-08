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

//! Source admission and boundary preparation, without a lake backend.
use crate::planning::freeze_read_boundary_for_table;
use crate::table::{FlussLakeScan, validate_lake_readable};
use crate::{FlussLakeError, FlussLakeReadContext, Result};
use fluss::error::Error as ClientError;
use fluss::metadata::TableInfo;
use fluss::predicate::BoundPredicate;
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
    if table_info.has_primary_key() && !scan.lake_only() {
        validate_pk_union_merge_engine(&table_info)?;
    }
    let boundary = freeze_read_boundary_for_table(&admin, scan.table_path(), &table_info).await?;
    FlussLakeReadContext::from_boundary(&table_info, boundary)
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

fn planning_client_error(action: &str, error: ClientError) -> FlussLakeError {
    match error {
        ClientError::RpcError { .. } => {
            FlussLakeError::ConnectionError(format!("failed to {action}: {error}"))
        }
        _ => FlussLakeError::PlanningFailed(format!("failed to {action}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluss::metadata::{DataTypes, Schema, TablePath};
    use std::collections::HashMap;
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
