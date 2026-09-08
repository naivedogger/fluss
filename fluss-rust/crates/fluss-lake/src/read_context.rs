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

//! Source boundaries shared by the default reader and engine-native readers.

use crate::planning::FrozenReadBoundary;
use crate::{FlussLakeError, FlussLakePartitionIdentity, Result};
use fluss::metadata::{
    DataLakeFormat, JsonSerde, MergeEngineType, Schema, TableBucket, TableInfo, TablePath,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const CONTEXT_VERSION: u32 = 1;

/// One frozen, half-open Fluss log range, not an engine scheduling unit.
///
/// Offsets are comparable only within `table_bucket`. Engines must preserve
/// change types and offset order when reconciling primary-key tails. Do not
/// infer the lake file layout from the bucket id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlussLakeLogRange {
    pub(crate) table_bucket: TableBucket,
    pub(crate) partition_identity: FlussLakePartitionIdentity,
    pub(crate) start_offset: i64,
    pub(crate) stop_offset: i64,
    pub(crate) earliest_offset: i64,
}

impl FlussLakeLogRange {
    /// Server table, partition, and bucket identity used to subscribe.
    pub fn table_bucket(&self) -> &TableBucket {
        &self.table_bucket
    }

    /// Logical partition values, in table partition-key order.
    pub fn partition_identity(&self) -> &FlussLakePartitionIdentity {
        &self.partition_identity
    }

    /// Inclusive seam offset, or zero when the bucket has no lake baseline.
    pub fn start_offset(&self) -> i64 {
        self.start_offset
    }

    /// Exclusive, server-issued end offset captured during preparation.
    pub fn stop_offset(&self) -> i64 {
        self.stop_offset
    }

    /// Earliest retained offset observed during preparation.
    ///
    /// Retention may advance after preparation. This is not a lease.
    pub fn earliest_offset(&self) -> i64 {
        self.earliest_offset
    }

    /// Whether this range contains no changelog records.
    pub fn is_empty(&self) -> bool {
        self.start_offset == self.stop_offset
    }

    /// Checks whether the required range was already unavailable at preparation.
    ///
    /// Call this for every range an engine-native union read needs, after safe
    /// partition/bucket pruning. Lake-only reads do not need this check. A
    /// reader must still detect retention or truncation during execution.
    pub fn validate_available(&self) -> Result<()> {
        if self.start_offset < self.earliest_offset {
            return Err(FlussLakeError::DataUnavailable(format!(
                "required log range [{}, {}) for {} starts before retained offset {}; the frozen view cannot be reconstructed",
                self.start_offset, self.stop_offset, self.table_bucket, self.earliest_offset
            )));
        }
        Ok(())
    }

    pub(crate) fn estimated_log_rows(&self) -> Option<usize> {
        self.stop_offset
            .checked_sub(self.start_offset)
            .and_then(|rows| usize::try_from(rows).ok())
    }

    pub(crate) fn lake_only(
        table_id: i64,
        bucket_id: i32,
        partition_identity: FlussLakePartitionIdentity,
    ) -> Self {
        Self {
            table_bucket: TableBucket::new(table_id, bucket_id),
            partition_identity,
            start_offset: 0,
            stop_offset: 0,
            earliest_offset: 0,
        }
    }
}

/// Immutable source state for one table, independent of physical execution.
///
/// A context fixes the readable lake snapshot and all live Fluss bucket
/// boundaries observed during preparation. It is not a global transactional
/// snapshot, a retention lease, or a file/split plan. It carries neither
/// credentials nor projection/filter configuration, so scans with different
/// predicates can share the same boundary.
///
/// The lake snapshot may also contain partitions no longer present in Fluss.
/// Engines must plan the entire selected lake baseline, including those
/// partitions; `log_ranges()` is not a complete lake partition inventory.
/// Engine-native readers own physical planning and reconciliation and must
/// satisfy the same current-view and failure semantics as the default reader.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "ContextDescriptor", into = "ContextDescriptor")]
pub struct FlussLakeReadContext {
    descriptor: ContextDescriptor,
    schema: Schema,
}

/// V1 uses the public Fluss schema JSON, not Rust's internal Schema serde.
/// No arbitrary catalog/table property map is allowed in this descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextDescriptor {
    context_version: u32,
    table_path: TablePath,
    table_id: i64,
    table_modified_time: i64,
    schema_id: i32,
    schema: serde_json::Value,
    partition_keys: Vec<String>,
    bucket_keys: Vec<String>,
    num_buckets: i32,
    lake_format: String,
    merge_engine: Option<String>,
    lake_snapshot_id: Option<i64>,
    log_ranges: Vec<FlussLakeLogRange>,
}

impl FlussLakeReadContext {
    pub(crate) fn from_boundary(table: &TableInfo, boundary: FrozenReadBoundary) -> Result<Self> {
        ContextDescriptor {
            context_version: CONTEXT_VERSION,
            table_path: table.table_path.clone(),
            table_id: table.table_id,
            table_modified_time: table.modified_time,
            schema_id: table.schema_id,
            schema: table.schema.serialize_json().map_err(invalid_context)?,
            partition_keys: table.partition_keys.to_vec(),
            bucket_keys: table.bucket_keys.clone(),
            num_buckets: table.num_buckets,
            lake_format: table
                .table_config
                .get_datalake_format()
                .map_err(invalid_context)?
                .ok_or_else(|| invalid_context("missing lake format"))?
                .to_string(),
            merge_engine: table
                .properties
                .get("table.merge-engine")
                .map(|value| value.to_ascii_lowercase()),
            lake_snapshot_id: boundary.readable_lake_snapshot_id(),
            log_ranges: boundary.bucket_ranges().to_vec(),
        }
        .try_into()
    }

    /// Context transport version, independent of default split versions.
    pub fn context_version(&self) -> u32 {
        self.descriptor.context_version
    }

    /// Logical Fluss table path. Catalog credentials are supplied separately.
    pub fn table_path(&self) -> &TablePath {
        &self.descriptor.table_path
    }

    /// Server-assigned identity, used to reject a dropped/recreated table.
    pub fn table_id(&self) -> i64 {
        self.descriptor.table_id
    }

    /// Source metadata modification time, not a query snapshot timestamp.
    ///
    /// Used conservatively to reject table-property changes without including
    /// arbitrary properties or credentials in the context.
    pub fn table_modified_time(&self) -> i64 {
        self.descriptor.table_modified_time
    }

    /// Frozen Fluss schema identity.
    pub fn schema_id(&self) -> i32 {
        self.descriptor.schema_id
    }

    /// Full, unprojected source schema, including stable field identities.
    pub fn table_schema(&self) -> &Schema {
        &self.schema
    }

    /// Table partition-key names in canonical order.
    pub fn partition_keys(&self) -> &[String] {
        &self.descriptor.partition_keys
    }

    /// Bucket-key names. The engine must use the source's bucketing rules.
    pub fn bucket_keys(&self) -> &[String] {
        &self.descriptor.bucket_keys
    }

    /// Number of source buckets per partition, not requested parallelism.
    pub fn num_buckets(&self) -> i32 {
        self.descriptor.num_buckets
    }

    /// Lake format identifier, such as `paimon`.
    pub fn lake_format(&self) -> &str {
        &self.descriptor.lake_format
    }

    /// Explicit Fluss merge engine, or `None` for the default semantics.
    ///
    /// Default semantics are append for log tables and deduplicate for PK
    /// tables. The lake adapter must separately validate its baseline merge
    /// engine and layout before executing a union read.
    pub fn merge_engine(&self) -> Option<&str> {
        self.descriptor.merge_engine.as_deref()
    }

    /// Readable lake snapshot to pass to the lake planner without refreshing it.
    pub fn lake_snapshot_id(&self) -> Option<i64> {
        self.descriptor.lake_snapshot_id
    }

    /// All live bucket ranges captured before scan-specific pruning.
    pub fn log_ranges(&self) -> &[FlussLakeLogRange] {
        &self.descriptor.log_ranges
    }

    /// Serializes the public, credential-free context as UTF-8 JSON.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(&self.descriptor).map_err(invalid_context)
    }

    /// Decodes and validates a public context, including version and coverage.
    ///
    /// Transport is not authentication: receive contexts only from a trusted
    /// query coordinator and supply credentials outside the context.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(invalid_context)?;
        let version = value
            .get("context_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| invalid_context("missing or invalid context_version"))?;
        validate_version(version)?;
        serde_json::from_value::<ContextDescriptor>(value)
            .map_err(invalid_context)?
            .try_into()
    }

    /// Rejects live metadata that would reinterpret this frozen context.
    ///
    /// This does not refresh the snapshot, add newly created partitions, or
    /// verify that data is still retained. An engine-native adapter must also
    /// validate its lake layout and enforce bounded log availability.
    pub fn validate_table(&self, table: &TableInfo) -> Result<()> {
        crate::table::validate_lake_readable(table)?;
        let format = table
            .table_config
            .get_datalake_format()
            .map_err(invalid_context)?
            .map(|format| format.to_string());
        let merge = table
            .properties
            .get("table.merge-engine")
            .map(|value| value.to_ascii_lowercase());
        if self.table_path() != &table.table_path
            || self.table_id() != table.table_id
            || self.table_modified_time() != table.modified_time
            || self.schema_id() != table.schema_id
            || self.descriptor.schema != table.schema.serialize_json().map_err(invalid_context)?
            || self.partition_keys() != table.partition_keys.as_ref()
            || self.bucket_keys() != table.bucket_keys
            || self.num_buckets() != table.num_buckets
            || Some(self.lake_format()) != format.as_deref()
            || self.merge_engine() != merge.as_deref()
        {
            return Err(FlussLakeError::SchemaIncompatible(format!(
                "table metadata no longer matches the frozen read context for {} (table id {}, schema id {})",
                self.table_path(),
                self.table_id(),
                self.schema_id()
            )));
        }
        Ok(())
    }
}

impl From<FlussLakeReadContext> for ContextDescriptor {
    fn from(context: FlussLakeReadContext) -> Self {
        context.descriptor
    }
}

impl TryFrom<ContextDescriptor> for FlussLakeReadContext {
    type Error = FlussLakeError;

    fn try_from(descriptor: ContextDescriptor) -> Result<Self> {
        validate_version(u64::from(descriptor.context_version))?;
        if descriptor.table_path.database().is_empty()
            || descriptor.table_path.table().is_empty()
            || descriptor.table_id < 0
            || descriptor.table_modified_time < 0
            || descriptor.schema_id < 0
            || descriptor.num_buckets <= 0
            || descriptor.lake_snapshot_id.is_some_and(|id| id < 0)
        {
            return Err(invalid_context(
                "invalid table, schema, snapshot, or bucket identity",
            ));
        }
        descriptor
            .lake_format
            .parse::<DataLakeFormat>()
            .map_err(invalid_context)?;
        if let Some(engine) = &descriptor.merge_engine {
            engine.parse::<MergeEngineType>().map_err(invalid_context)?;
        }
        let schema = Schema::deserialize_json(&descriptor.schema).map_err(invalid_context)?;
        // The core parser accepts some legacy shapes and ignores unknown
        // fields. A versioned context must not silently reinterpret them or
        // preserve arbitrary properties in its schema envelope.
        if schema.serialize_json().map_err(invalid_context)? != descriptor.schema {
            return Err(invalid_context(
                "schema must use the canonical Fluss schema JSON",
            ));
        }
        let columns: HashSet<_> = schema.column_names().into_iter().collect();
        for keys in [&descriptor.partition_keys, &descriptor.bucket_keys] {
            let mut seen = HashSet::new();
            if keys
                .iter()
                .any(|key| !columns.contains(key.as_str()) || !seen.insert(key))
            {
                return Err(invalid_context("unknown or duplicate partition/bucket key"));
            }
        }
        if descriptor
            .bucket_keys
            .iter()
            .any(|key| descriptor.partition_keys.contains(key))
        {
            return Err(invalid_context(
                "bucket keys must not contain partition keys",
            ));
        }
        if schema.primary_key().is_some() {
            let primary_keys = schema.primary_key_column_names();
            if descriptor.bucket_keys.is_empty()
                || descriptor
                    .partition_keys
                    .iter()
                    .chain(&descriptor.bucket_keys)
                    .any(|key| !primary_keys.contains(&key.as_str()))
            {
                return Err(invalid_context(
                    "PK partition and bucket keys must belong to the primary key",
                ));
            }
        }
        let mut groups = HashMap::new();
        let mut partition_ids = HashMap::new();
        for range in &descriptor.log_ranges {
            let bucket = range.table_bucket();
            if bucket.table_id() != descriptor.table_id
                || bucket.bucket_id() < 0
                || bucket.bucket_id() >= descriptor.num_buckets
                || bucket.partition_id().is_some_and(|id| id < 0)
                || range.start_offset < 0
                || range.earliest_offset < 0
                || range.start_offset > range.stop_offset
                || range.earliest_offset > range.stop_offset
                || (descriptor.lake_snapshot_id.is_none() && range.start_offset != 0)
            {
                return Err(invalid_context("invalid bucket identity or log boundary"));
            }
            match range.partition_identity() {
                FlussLakePartitionIdentity::Unpartitioned => {
                    if !descriptor.partition_keys.is_empty() || bucket.partition_id().is_some() {
                        return Err(invalid_context(
                            "unpartitioned range in a partitioned context",
                        ));
                    }
                }
                FlussLakePartitionIdentity::KeyValues(values) => {
                    if descriptor.partition_keys.is_empty()
                        || bucket.partition_id().is_none()
                        || !values
                            .iter()
                            .map(|(key, _)| key)
                            .eq(descriptor.partition_keys.iter())
                    {
                        return Err(invalid_context(
                            "partition values do not match the frozen schema",
                        ));
                    }
                }
            }
            if let Some(existing) =
                partition_ids.insert(bucket.partition_id(), range.partition_identity())
                && existing != range.partition_identity()
            {
                return Err(invalid_context(
                    "partition id has conflicting logical identities",
                ));
            }
            let (partition_id, buckets) = groups
                .entry(range.partition_identity())
                .or_insert_with(|| (bucket.partition_id(), HashSet::new()));
            if *partition_id != bucket.partition_id() || !buckets.insert(bucket.bucket_id()) {
                return Err(invalid_context(
                    "duplicate or conflicting partition/bucket range",
                ));
            }
        }
        if (descriptor.partition_keys.is_empty() && groups.len() != 1)
            || groups
                .values()
                .any(|(_, buckets)| buckets.len() != descriptor.num_buckets as usize)
        {
            return Err(invalid_context(
                "context must cover every bucket of each captured live partition",
            ));
        }
        Ok(Self { descriptor, schema })
    }
}

fn validate_version(version: u64) -> Result<()> {
    if version != u64::from(CONTEXT_VERSION) {
        return Err(FlussLakeError::IncompatibleReadContextVersion(format!(
            "context version {version}; supported version is {CONTEXT_VERSION}"
        )));
    }
    Ok(())
}

fn invalid_context(error: impl std::fmt::Display) -> FlussLakeError {
    FlussLakeError::InvalidReadContext(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluss::metadata::DataTypes;
    use serde_json::json;

    fn table(partitioned: bool) -> TableInfo {
        TableInfo::new(
            TablePath::new("fluss", "orders"),
            7,
            2,
            Schema::builder()
                .column("id", DataTypes::int())
                .column("region", DataTypes::string())
                .column("amount", DataTypes::bigint())
                .primary_key_named("orders_pk", vec!["id", "region"])
                .unwrap()
                .build()
                .unwrap(),
            vec!["id".to_string()],
            if partitioned {
                vec!["region".to_string()]
            } else {
                vec![]
            }
            .into(),
            2,
            HashMap::from([
                ("table.datalake.enabled".to_string(), "true".to_string()),
                ("table.datalake.format".to_string(), "paimon".to_string()),
                (
                    "table.datalake.paimon.s3.secret-key".to_string(),
                    "SECRET".to_string(),
                ),
                (
                    "table.datalake.paimon.warehouse".to_string(),
                    "s3://private/warehouse".to_string(),
                ),
            ]),
            HashMap::new(),
            None,
            0,
            0,
        )
    }

    fn context(partitioned: bool) -> FlussLakeReadContext {
        let partition = if partitioned {
            FlussLakePartitionIdentity::KeyValues(vec![("region".to_string(), "US".to_string())])
        } else {
            FlussLakePartitionIdentity::Unpartitioned
        };
        FlussLakeReadContext::from_boundary(
            &table(partitioned),
            FrozenReadBoundary {
                readable_lake_snapshot_id: Some(42),
                bucket_ranges: (0..2)
                    .map(|bucket| FlussLakeLogRange {
                        table_bucket: TableBucket::new_with_partition(
                            7,
                            partitioned.then_some(9),
                            bucket,
                        ),
                        partition_identity: partition.clone(),
                        start_offset: 12,
                        stop_offset: 20,
                        earliest_offset: 8,
                    })
                    .collect(),
            },
        )
        .unwrap()
    }

    fn value(partitioned: bool) -> serde_json::Value {
        serde_json::from_slice(&context(partitioned).to_json().unwrap()).unwrap()
    }

    fn assert_invalid(value: serde_json::Value) {
        assert!(
            matches!(
                FlussLakeReadContext::from_json(&serde_json::to_vec(&value).unwrap()),
                Err(FlussLakeError::InvalidReadContext(_))
            ),
            "unexpectedly accepted {value}"
        );
        assert!(serde_json::from_value::<FlussLakeReadContext>(value).is_err());
    }

    #[test]
    fn public_context_round_trips_without_a_lake_backend() {
        for partitioned in [false, true] {
            let original = context(partitioned);
            let bytes = original.to_json().unwrap();
            let decoded = FlussLakeReadContext::from_json(&bytes).unwrap();
            assert_eq!(decoded.to_json().unwrap(), bytes);
            assert_eq!(decoded.log_ranges(), original.log_ranges());
            assert_eq!(decoded.table_schema(), original.table_schema());
            assert_eq!(decoded.table_schema().columns()[0].id(), 0);
            // The canonical schema intentionally omits constraint names.
            // A named PK must not be mistaken for schema drift.
            decoded.validate_table(&table(partitioned)).unwrap();
            let via_serde: FlussLakeReadContext = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(serde_json::to_vec(&via_serde).unwrap(), bytes);
        }
    }

    #[test]
    fn version_one_transport_shape_is_stable() {
        let fixture = json!({
            "context_version": 1,
            "table_path": {"database": "fluss", "table": "orders"},
            "table_id": 7,
            "table_modified_time": 0,
            "schema_id": 2,
            "schema": {
                "version": 1,
                "columns": [
                    {"name": "id", "id": 0, "data_type": {"type": "INTEGER", "nullable": false}},
                    {"name": "region", "id": 1, "data_type": {"type": "STRING", "nullable": false}},
                    {"name": "amount", "id": 2, "data_type": {"type": "BIGINT"}}
                ],
                "primary_key": ["id", "region"],
                "highest_field_id": 2
            },
            "partition_keys": ["region"],
            "bucket_keys": ["id"],
            "num_buckets": 2,
            "lake_format": "paimon",
            "merge_engine": null,
            "lake_snapshot_id": 42,
            "log_ranges": [
                {
                    "table_bucket": {"table_id": 7, "partition_id": 9, "bucket": 0},
                    "partition_identity": {"KeyValues": [["region", "US"]]},
                    "start_offset": 12, "stop_offset": 20, "earliest_offset": 8
                },
                {
                    "table_bucket": {"table_id": 7, "partition_id": 9, "bucket": 1},
                    "partition_identity": {"KeyValues": [["region", "US"]]},
                    "start_offset": 12, "stop_offset": 20, "earliest_offset": 8
                }
            ]
        });
        assert_eq!(value(true), fixture);
        FlussLakeReadContext::from_json(&serde_json::to_vec(&fixture).unwrap()).unwrap();
    }

    #[test]
    fn transport_contains_only_source_contract_fields() {
        let encoded = String::from_utf8(context(true).to_json().unwrap()).unwrap();
        for excluded in [
            "SECRET",
            "secret-key",
            "warehouse",
            "properties",
            "projection",
            "filter",
            "batch_size",
            "encoded_split",
            "catalog",
        ] {
            assert!(!encoded.contains(excluded), "context leaked {excluded}");
        }
        let mut unknown = value(false);
        unknown["properties"] = json!({"secret": "value"});
        assert_invalid(unknown);
        let mut unknown_schema = value(false);
        unknown_schema["schema"]["properties"] = json!({"secret": "value"});
        assert_invalid(unknown_schema);
    }

    #[test]
    fn incompatible_versions_are_explicit_errors() {
        for version in [0, 2, u64::MAX] {
            let mut value = value(false);
            value["context_version"] = json!(version);
            assert!(matches!(
                FlussLakeReadContext::from_json(&serde_json::to_vec(&value).unwrap()),
                Err(FlussLakeError::IncompatibleReadContextVersion(_))
            ));
        }
        for version in [json!(null), json!("1"), json!(-1)] {
            let mut value = value(false);
            value["context_version"] = version;
            assert_invalid(value);
        }
    }

    #[test]
    fn rejects_invalid_source_identities_and_schema_shapes() {
        for (key, invalid) in [
            ("table_id", json!(-1)),
            ("table_modified_time", json!(-1)),
            ("schema_id", json!(-1)),
            ("lake_snapshot_id", json!(-1)),
            ("num_buckets", json!(0)),
            ("lake_format", json!("unknown")),
            ("merge_engine", json!("deduplicate")),
            ("bucket_keys", json!(["missing"])),
            ("bucket_keys", json!(["id", "id"])),
            ("bucket_keys", json!([])),
            ("bucket_keys", json!(["amount"])),
            ("partition_keys", json!(["amount"])),
        ] {
            let mut value = value(false);
            value[key] = invalid;
            assert_invalid(value);
        }
        let mut value = value(false);
        value["schema"]["version"] = json!(2);
        assert_invalid(value);
    }

    #[test]
    fn rejects_incomplete_duplicate_or_conflicting_buckets() {
        let mut missing = value(false);
        missing["log_ranges"].as_array_mut().unwrap().pop();
        assert_invalid(missing);
        let mut duplicate = value(true);
        duplicate["log_ranges"][1] = duplicate["log_ranges"][0].clone();
        assert_invalid(duplicate);
        let mut conflicting = value(true);
        conflicting["log_ranges"][1]["partition_identity"] = serde_json::to_value(
            FlussLakePartitionIdentity::KeyValues(vec![("region".to_string(), "EU".to_string())]),
        )
        .unwrap();
        assert_invalid(conflicting);
        let mut empty = value(false);
        empty["log_ranges"] = json!([]);
        assert_invalid(empty);
    }

    #[test]
    fn rejects_wrong_table_bucket_or_partition_identity() {
        for (key, invalid) in [
            ("table_id", json!(8)),
            ("bucket", json!(-1)),
            ("bucket", json!(2)),
            ("partition_id", json!(-1)),
            ("partition_id", json!(null)),
        ] {
            let mut value = value(true);
            value["log_ranges"][0]["table_bucket"][key] = invalid;
            assert_invalid(value);
        }
        let mut value = value(true);
        value["log_ranges"][0]["partition_identity"] = json!({"KeyValues": [["other_key", "US"]]});
        assert_invalid(value);
    }

    #[test]
    fn empty_partitioned_table_is_valid_but_not_a_lake_inventory() {
        let mut value = value(true);
        value["log_ranges"] = json!([]);
        let decoded =
            FlussLakeReadContext::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(decoded.log_ranges().is_empty());
        assert_eq!(decoded.lake_snapshot_id(), Some(42));
    }

    #[test]
    fn rejects_malformed_bounds_and_missing_baseline_seams() {
        for (key, invalid) in [
            ("start_offset", -1),
            ("stop_offset", 11),
            ("earliest_offset", -1),
            ("earliest_offset", 21),
        ] {
            let mut value = value(false);
            value["log_ranges"][0][key] = json!(invalid);
            assert_invalid(value);
        }
        let mut no_baseline = value(false);
        no_baseline["lake_snapshot_id"] = json!(null);
        assert_invalid(no_baseline);
    }

    #[test]
    fn retained_gap_is_rejected_only_when_range_is_required() {
        let mut value = value(false);
        value["lake_snapshot_id"] = json!(null);
        for range in value["log_ranges"].as_array_mut().unwrap() {
            range["start_offset"] = json!(0);
        }
        let context =
            FlussLakeReadContext::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        // Lake-only reads and safely pruned ranges do not consume the tail.
        // Native union execution must check each required range.
        assert!(matches!(
            context.log_ranges()[0].validate_available(),
            Err(FlussLakeError::DataUnavailable(_))
        ));
    }

    #[test]
    fn validates_schema_table_and_layout_without_refreshing_bounds() {
        let context = context(true);
        let mut recreated = table(true);
        recreated.table_id += 1;
        let mut evolved = table(true);
        evolved.schema_id += 1;
        let mut rebucketed = table(true);
        rebucketed.num_buckets += 1;
        let mut moved = table(true);
        moved.table_path = TablePath::new("fluss", "another");
        let mut changed_properties = table(true);
        changed_properties.modified_time += 1;
        for table in [recreated, evolved, rebucketed, moved, changed_properties] {
            assert!(matches!(
                context.validate_table(&table),
                Err(FlussLakeError::SchemaIncompatible(_))
            ));
        }
        assert_eq!(context.lake_snapshot_id(), Some(42));
        assert_eq!(context.log_ranges()[0].stop_offset(), 20);
    }
}
