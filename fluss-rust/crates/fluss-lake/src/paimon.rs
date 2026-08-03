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

//! Paimon lake snapshot planning and reading.
//!
//! A Fluss table tiered to Paimon is mirrored one-to-one: the Paimon
//! identifier is the Fluss `database.table`, and the catalog is configured by
//! the table's `table.datalake.paimon.*` properties. Planning resolves a
//! readable lake snapshot into immutable Paimon splits; execution reads one
//! split back as a finite Arrow batch stream.
//!
//! Only Parquet data files are supported: the pinned paimon-rust build has ORC
//! reads disabled, so an ORC table fails at read time with an explicit
//! unsupported-format error.

use crate::{SendableRecordBatchStream, UnionReadError, UnionReadResult};
use fluss::metadata::{RowType, TableInfo, TablePath};
use futures::StreamExt;
use paimon::catalog::Identifier;
use paimon::table::Table;
use paimon::{CatalogFactory, DataSplit, Options};
use std::collections::HashMap;

/// Property prefix carrying the Paimon catalog configuration of a table.
const PAIMON_PROPERTY_PREFIX: &str = "table.datalake.paimon.";

/// Paimon table option that pins a scan to one snapshot id.
const PAIMON_SCAN_VERSION_OPTION: &str = "scan.version";

/// Catalog configuration needed to reopen one Paimon table.
///
/// This is planner output that travels inside a task descriptor, so it must
/// stay a plain serializable map rather than a live catalog handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaimonCatalogOptions {
    options: HashMap<String, String>,
}

impl PaimonCatalogOptions {
    /// Extracts the Paimon catalog options from a resolved Fluss table.
    ///
    /// `table.datalake.paimon.warehouse` becomes `warehouse`, and so on for
    /// every other prefixed property.
    pub(crate) fn from_table_info(table_info: &TableInfo) -> UnionReadResult<Self> {
        let options: HashMap<String, String> = table_info
            .properties
            .iter()
            .chain(table_info.custom_properties.iter())
            .filter_map(|(key, value)| {
                key.strip_prefix(PAIMON_PROPERTY_PREFIX)
                    .map(|suffix| (suffix.to_string(), value.clone()))
            })
            .collect();
        if !options.contains_key("warehouse") {
            return Err(UnionReadError::Planning(format!(
                "table {} has a readable lake snapshot but no {PAIMON_PROPERTY_PREFIX}warehouse property",
                table_info.table_path
            )));
        }
        Ok(Self { options })
    }

    pub(crate) fn as_map(&self) -> &HashMap<String, String> {
        &self.options
    }

    pub(crate) fn from_map(options: HashMap<String, String>) -> Self {
        Self { options }
    }
}

/// Opens one Paimon table pinned to a single lake snapshot.
///
/// Pinning uses the Paimon `scan.version` option so that both planning and
/// execution observe exactly the snapshot frozen by the planner, regardless of
/// later lake commits.
async fn open_pinned_table(
    table_path: &TablePath,
    catalog_options: &PaimonCatalogOptions,
    snapshot_id: i64,
) -> UnionReadResult<Table> {
    let mut options = Options::default();
    for (key, value) in catalog_options.as_map() {
        options.set(key, value);
    }
    let catalog = CatalogFactory::create(options)
        .await
        .map_err(|error| paimon_error("create Paimon catalog", error))?;
    let identifier = Identifier::new(table_path.database(), table_path.table());
    let table = catalog
        .get_table(&identifier)
        .await
        .map_err(|error| paimon_error("open Paimon table", error))?;

    let mut pinned = HashMap::with_capacity(1);
    pinned.insert(
        PAIMON_SCAN_VERSION_OPTION.to_string(),
        snapshot_id.to_string(),
    );
    Ok(table.copy_with_options(pinned))
}

/// Resolves a scan output projection into Paimon field names.
///
/// Paimon projects by name while UnionRead requests project by Fluss field
/// index, so the planner resolves indexes once against the frozen schema.
pub(crate) fn projected_field_names(
    row_type: &RowType,
    output_projection: Option<&[usize]>,
) -> UnionReadResult<Option<Vec<String>>> {
    let Some(projection) = output_projection else {
        return Ok(None);
    };
    let mut names = Vec::with_capacity(projection.len());
    for field_index in projection {
        let field = row_type.fields().get(*field_index).ok_or_else(|| {
            UnionReadError::InvalidRequest(format!(
                "output projection field index {field_index} exceeds table width {}",
                row_type.fields().len()
            ))
        })?;
        names.push(field.name().to_string());
    }
    Ok(Some(names))
}

/// Plans the immutable Paimon splits of one readable lake snapshot.
///
/// Splits are returned as JSON so that they can be embedded in opaque task
/// descriptors and shipped to execution workers.
pub(crate) async fn plan_snapshot_splits(
    table_path: &TablePath,
    catalog_options: &PaimonCatalogOptions,
    snapshot_id: i64,
    projected_fields: Option<&[String]>,
) -> UnionReadResult<Vec<String>> {
    let table = open_pinned_table(table_path, catalog_options, snapshot_id).await?;
    let mut read_builder = table.new_read_builder();
    if let Some(field_names) = projected_fields {
        let borrowed: Vec<&str> = field_names.iter().map(String::as_str).collect();
        read_builder.with_projection(&borrowed);
    }
    let plan = read_builder
        .new_scan()
        .plan()
        .await
        .map_err(|error| paimon_error("plan Paimon snapshot splits", error))?;

    plan.splits()
        .iter()
        .map(|split| {
            serde_json::to_string(split).map_err(|error| {
                UnionReadError::Planning(format!("failed to serialize Paimon split: {error}"))
            })
        })
        .collect()
}

/// Reads one frozen Paimon split as a finite Arrow batch stream.
pub(crate) async fn read_snapshot_split(
    table_path: &TablePath,
    catalog_options: &PaimonCatalogOptions,
    snapshot_id: i64,
    projected_fields: Option<&[String]>,
    encoded_split: &str,
) -> UnionReadResult<SendableRecordBatchStream> {
    let split: DataSplit = serde_json::from_str(encoded_split).map_err(|error| {
        UnionReadError::InvalidTask(format!("failed to decode Paimon split: {error}"))
    })?;
    let table = open_pinned_table(table_path, catalog_options, snapshot_id).await?;
    let mut read_builder = table.new_read_builder();
    if let Some(field_names) = projected_fields {
        let borrowed: Vec<&str> = field_names.iter().map(String::as_str).collect();
        read_builder.with_projection(&borrowed);
    }
    let stream = read_builder
        .new_read()
        .map_err(|error| paimon_error("create Paimon reader", error))?
        .to_arrow(&[split])
        .map_err(|error| paimon_error("read Paimon split", error))?;

    Ok(Box::pin(stream.map(|result| {
        result.map_err(|error| paimon_error("read Paimon split batch", error))
    })))
}

fn paimon_error(action: &str, error: paimon::Error) -> UnionReadError {
    UnionReadError::Execution(format!("failed to {action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluss::metadata::{DataField, DataTypes};

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("name", DataTypes::string(), None),
            DataField::new("amount", DataTypes::bigint(), None),
        ])
    }

    #[test]
    fn resolves_projection_indexes_into_paimon_field_names() {
        let names = projected_field_names(&row_type(), Some(&[2, 0])).unwrap();

        assert_eq!(
            names,
            Some(vec!["amount".to_string(), "id".to_string()]),
            "projection order must be preserved for the engine's scan output"
        );
        assert_eq!(projected_field_names(&row_type(), None).unwrap(), None);
    }

    #[test]
    fn rejects_projection_beyond_the_frozen_schema() {
        assert!(matches!(
            projected_field_names(&row_type(), Some(&[3])),
            Err(UnionReadError::InvalidRequest(_))
        ));
    }

    #[test]
    fn catalog_options_round_trip_through_a_plain_map() {
        let mut options = HashMap::new();
        options.insert("warehouse".to_string(), "/tmp/warehouse".to_string());
        let catalog_options = PaimonCatalogOptions::from_map(options.clone());

        assert_eq!(catalog_options.as_map(), &options);
    }
}
