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

use crate::{FlussLakeError, FlussLakeRecordBatchStream, FlussLakeResult};
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

/// Substrings marking a catalog option as a secret.
///
/// Object-store credentials arrive mixed into the same property namespace as
/// warehouse locations, so they are classified by key rather than by an
/// allowlist: a new secret-bearing option is then withheld by default instead
/// of leaking until someone notices it.
const SENSITIVE_OPTION_MARKERS: [&str; 9] = [
    "secret",
    "password",
    "passwd",
    "token",
    "credential",
    "access-key",
    "access_key",
    "private-key",
    "private_key",
];

/// Returns whether a catalog option key carries a secret.
pub(crate) fn is_sensitive_catalog_option(key: &str) -> bool {
    let lowercase = key.to_ascii_lowercase();
    SENSITIVE_OPTION_MARKERS
        .iter()
        .any(|marker| lowercase.contains(marker))
}

/// Catalog configuration needed to reopen one Paimon table.
///
/// This is planner output that travels inside a split descriptor, so it must
/// stay a plain serializable map rather than a live catalog handle. Secrets are
/// deliberately not part of it: see [`PaimonCatalogOptions::non_sensitive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaimonCatalogOptions {
    options: HashMap<String, String>,
}

impl PaimonCatalogOptions {
    /// Extracts the Paimon catalog options from a resolved Fluss table.
    ///
    /// `table.datalake.paimon.warehouse` becomes `warehouse`, and so on for
    /// every other prefixed property.
    pub(crate) fn from_table_info(table_info: &TableInfo) -> FlussLakeResult<Self> {
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
            return Err(FlussLakeError::Planning(format!(
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

    /// Returns the options that may be embedded in a split descriptor.
    ///
    /// Splits are cached, logged and persisted by engines, so secrets must never
    /// be serialized into them. An executor re-supplies the withheld options
    /// through its execution context.
    pub(crate) fn non_sensitive(&self) -> HashMap<String, String> {
        self.options
            .iter()
            .filter(|(key, _)| !is_sensitive_catalog_option(key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    /// Merges runtime-supplied credentials over the split-carried options.
    ///
    /// Runtime values win: a credential rotated after planning must take
    /// effect without re-planning.
    pub(crate) fn with_runtime_credentials(
        mut self,
        credentials: &HashMap<String, String>,
    ) -> Self {
        for (key, value) in credentials {
            self.options.insert(key.clone(), value.clone());
        }
        self
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
) -> FlussLakeResult<Table> {
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

/// Resolves a scan output projection into explicit Paimon field names.
///
/// Paimon projects by name while UnionRead requests project by Fluss field
/// index, so the planner resolves indexes once against the frozen schema.
///
/// The resolved projection is explicit even when the request carries none:
/// Fluss tiering appends the `__bucket`, `__offset` and `__timestamp` system
/// columns to the Paimon table, so an unprojected Paimon read would leak
/// columns that are not part of the plan's output schema. Enumerating the
/// Fluss columns strips them by construction (the Java connector's
/// `PaimonRecordReader` applies the same rule with a positional
/// `ProjectedRow`).
pub(crate) fn projected_field_names(
    row_type: &RowType,
    output_projection: Option<&[usize]>,
) -> FlussLakeResult<Vec<String>> {
    let Some(projection) = output_projection else {
        return Ok(row_type
            .fields()
            .iter()
            .map(|field| field.name().to_string())
            .collect());
    };
    let mut names = Vec::with_capacity(projection.len());
    for field_index in projection {
        let field = row_type.fields().get(*field_index).ok_or_else(|| {
            FlussLakeError::InvalidRequest(format!(
                "output projection field index {field_index} exceeds table width {}",
                row_type.fields().len()
            ))
        })?;
        names.push(field.name().to_string());
    }
    Ok(names)
}

/// Plans the immutable Paimon splits of one readable lake snapshot.
///
/// Splits are returned as JSON so that they can be embedded in opaque split
/// descriptors and shipped to execution workers.
pub(crate) async fn plan_snapshot_splits(
    table_path: &TablePath,
    catalog_options: &PaimonCatalogOptions,
    snapshot_id: i64,
    projected_fields: Option<&[String]>,
) -> FlussLakeResult<Vec<String>> {
    let table = open_pinned_table(table_path, catalog_options, snapshot_id).await?;
    let mut read_builder = table.new_read_builder();
    if let Some(field_names) = projected_fields {
        let borrowed: Vec<&str> = field_names.iter().map(String::as_str).collect();
        read_builder
            .with_projection(&borrowed)
            .map_err(|error| paimon_error("apply Paimon planning projection", error))?;
    }
    let plan = read_builder
        .new_scan()
        .plan()
        .await
        .map_err(|error| paimon_error("plan Paimon snapshot splits", error))?;

    plan.splits().iter().map(encode_split).collect()
}

/// Rejects Paimon merge engines whose current view v1 cannot reproduce.
///
/// The hash-overlay merge presumes that overlaying a deduplicate changelog
/// tail onto the lake current state yields the table's current view. Under
/// any other merge engine that overlay silently produces a wrong view, so
/// everything else must fail at planning: `partial-update` until paimon-rust
/// gains write-time flush merges (apache/paimon-rust#380), `first-row` and
/// `aggregation` as out of scope. Deduplicate is also what Fluss tiering
/// writes, and Paimon's default when the option is absent.
pub(crate) fn ensure_deduplicate_merge_engine(
    table_options: &HashMap<String, String>,
    table_path: &TablePath,
) -> FlussLakeResult<()> {
    let merge_engine = paimon::spec::CoreOptions::new(table_options)
        .merge_engine()
        .map_err(|error| {
            FlussLakeError::Planning(format!(
                "failed to resolve the Paimon merge engine of {table_path}: {error}"
            ))
        })?;
    if merge_engine != paimon::spec::MergeEngine::Deduplicate {
        return Err(FlussLakeError::Planning(format!(
            "primary-key UnionRead only supports the deduplicate merge engine, but the Paimon table for {table_path} uses {merge_engine:?}; refusing to plan a read that would silently produce an incorrect current view"
        )));
    }
    Ok(())
}

/// Plans the Paimon splits of a primary-key snapshot, grouped by bucket.
///
/// A PK bucket's lake baseline and its log tail must land in the same split,
/// so splits are keyed by their Paimon bucket id — which Fluss tiering keeps
/// aligned with the Fluss bucket id. Planning is rejected up front for merge
/// engines other than deduplicate.
pub(crate) async fn plan_pk_snapshot_splits(
    table_path: &TablePath,
    catalog_options: &PaimonCatalogOptions,
    snapshot_id: i64,
) -> FlussLakeResult<HashMap<i32, Vec<String>>> {
    let table = open_pinned_table(table_path, catalog_options, snapshot_id).await?;
    ensure_deduplicate_merge_engine(table.schema().options(), table_path)?;

    let plan = table
        .new_read_builder()
        .new_scan()
        .plan()
        .await
        .map_err(|error| paimon_error("plan Paimon snapshot splits", error))?;

    let mut splits_by_bucket: HashMap<i32, Vec<String>> = HashMap::new();
    for split in plan.splits() {
        if split.bucket() < 0 {
            return Err(FlussLakeError::Planning(format!(
                "Paimon snapshot {snapshot_id} of {table_path} produced a split with negative bucket {}",
                split.bucket()
            )));
        }
        splits_by_bucket
            .entry(split.bucket())
            .or_default()
            .push(encode_split(split)?);
    }
    Ok(splits_by_bucket)
}

fn encode_split(split: &DataSplit) -> FlussLakeResult<String> {
    serde_json::to_string(split).map_err(|error| {
        FlussLakeError::Planning(format!("failed to serialize Paimon split: {error}"))
    })
}

/// Reads one frozen Paimon split as a finite Arrow batch stream.
pub(crate) async fn read_snapshot_split(
    table_path: &TablePath,
    catalog_options: &PaimonCatalogOptions,
    snapshot_id: i64,
    projected_fields: Option<&[String]>,
    encoded_split: &str,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    read_snapshot_splits(
        table_path,
        catalog_options,
        snapshot_id,
        projected_fields,
        vec![decode_split(encoded_split)?],
    )
    .await
}

/// Reads several frozen Paimon splits as one finite Arrow batch stream.
///
/// All splits must come from the same pinned snapshot. Reading them through
/// one Paimon reader matters for primary-key tables: since
/// apache/paimon-rust#374 the reader deduplicates keys across the splits it
/// is given, which is exactly the per-bucket exactly-once guarantee the
/// primary-key merge presumes.
pub(crate) async fn read_snapshot_splits(
    table_path: &TablePath,
    catalog_options: &PaimonCatalogOptions,
    snapshot_id: i64,
    projected_fields: Option<&[String]>,
    splits: Vec<DataSplit>,
) -> FlussLakeResult<FlussLakeRecordBatchStream> {
    let table = open_pinned_table(table_path, catalog_options, snapshot_id).await?;
    let mut read_builder = table.new_read_builder();
    if let Some(field_names) = projected_fields {
        let borrowed: Vec<&str> = field_names.iter().map(String::as_str).collect();
        read_builder
            .with_projection(&borrowed)
            .map_err(|error| paimon_error("apply Paimon read projection", error))?;
    }
    let stream = read_builder
        .new_read()
        .map_err(|error| paimon_error("create Paimon reader", error))?
        .to_arrow(&splits)
        .map_err(|error| paimon_error("read Paimon splits", error))?;

    Ok(Box::pin(stream.map(|result| {
        result.map_err(|error| paimon_error("read Paimon split batch", error))
    })))
}

pub(crate) fn decode_split(encoded_split: &str) -> FlussLakeResult<DataSplit> {
    serde_json::from_str(encoded_split).map_err(|error| {
        FlussLakeError::InvalidSplit(format!("failed to decode Paimon split: {error}"))
    })
}

fn paimon_error(action: &str, error: paimon::Error) -> FlussLakeError {
    FlussLakeError::Execution(format!("failed to {action}: {error}"))
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
            vec!["amount".to_string(), "id".to_string()],
            "projection order must be preserved for the engine's scan output"
        );
    }

    /// A request without a projection must still freeze an explicit column
    /// list, or the tiering-appended Paimon system columns would leak into
    /// the output schema.
    #[test]
    fn unprojected_requests_freeze_all_fluss_columns_explicitly() {
        let names = projected_field_names(&row_type(), None).unwrap();

        assert_eq!(
            names,
            vec!["id".to_string(), "name".to_string(), "amount".to_string()]
        );
    }

    #[test]
    fn non_sensitive_withholds_secret_catalog_options() {
        let mut options = HashMap::new();
        options.insert("warehouse".to_string(), "s3://bucket/warehouse".to_string());
        options.insert("s3.endpoint".to_string(), "http://minio:9000".to_string());
        options.insert("s3.access-key-id".to_string(), "AKID".to_string());
        options.insert("s3.secret-key".to_string(), "TOP-SECRET".to_string());
        options.insert("fs.oss.sts.token".to_string(), "STS-TOKEN".to_string());
        let catalog_options = PaimonCatalogOptions::from_map(options);

        let non_sensitive = catalog_options.non_sensitive();

        let mut expected = HashMap::new();
        expected.insert("warehouse".to_string(), "s3://bucket/warehouse".to_string());
        expected.insert("s3.endpoint".to_string(), "http://minio:9000".to_string());
        assert_eq!(non_sensitive, expected);
    }

    #[test]
    fn runtime_credentials_override_split_carried_options() {
        let mut split_options = HashMap::new();
        split_options.insert("warehouse".to_string(), "/tmp/warehouse".to_string());
        split_options.insert("s3.endpoint".to_string(), "http://stale:9000".to_string());
        let mut credentials = HashMap::new();
        credentials.insert("s3.secret-key".to_string(), "ROTATED".to_string());
        credentials.insert("s3.endpoint".to_string(), "http://fresh:9000".to_string());

        let merged =
            PaimonCatalogOptions::from_map(split_options).with_runtime_credentials(&credentials);

        assert_eq!(
            merged.as_map().get("s3.secret-key"),
            Some(&"ROTATED".to_string())
        );
        assert_eq!(
            merged.as_map().get("s3.endpoint"),
            Some(&"http://fresh:9000".to_string()),
            "a credential rotated after planning must win over the split value"
        );
        assert_eq!(
            merged.as_map().get("warehouse"),
            Some(&"/tmp/warehouse".to_string())
        );
    }

    #[test]
    fn rejects_projection_beyond_the_frozen_schema() {
        assert!(matches!(
            projected_field_names(&row_type(), Some(&[3])),
            Err(FlussLakeError::InvalidRequest(_))
        ));
    }

    #[test]
    fn catalog_options_round_trip_through_a_plain_map() {
        let mut options = HashMap::new();
        options.insert("warehouse".to_string(), "/tmp/warehouse".to_string());
        let catalog_options = PaimonCatalogOptions::from_map(options.clone());

        assert_eq!(catalog_options.as_map(), &options);
    }

    fn merge_engine_options(value: Option<&str>) -> HashMap<String, String> {
        let mut options = HashMap::new();
        if let Some(value) = value {
            options.insert("merge-engine".to_string(), value.to_string());
        }
        options
    }

    /// v1 admits merge engine `deduplicate` only — which is what Fluss
    /// tiering writes and what Paimon defaults to when the option is absent.
    /// Everything else must fail at planning rather than silently misread.
    #[test]
    fn merge_engine_gate_admits_deduplicate_only() {
        let table_path = TablePath::new("fluss", "pk_orders");

        ensure_deduplicate_merge_engine(&merge_engine_options(None), &table_path).unwrap();
        ensure_deduplicate_merge_engine(&merge_engine_options(Some("deduplicate")), &table_path)
            .unwrap();

        for rejected in ["partial-update", "first-row", "aggregation", "unknown"] {
            assert!(
                matches!(
                    ensure_deduplicate_merge_engine(
                        &merge_engine_options(Some(rejected)),
                        &table_path
                    ),
                    Err(FlussLakeError::Planning(_))
                ),
                "merge engine {rejected} must be rejected at planning"
            );
        }
    }
}
