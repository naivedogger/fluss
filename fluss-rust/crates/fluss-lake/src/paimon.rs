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

use crate::{FlussLakeError, FlussLakePartitionIdentity, RecordBatchStream, Result};
use arrow::array::StructArray;
use arrow::record_batch::RecordBatch;
use fluss::metadata::{DataType, RowType, TableInfo, TablePath};
use fluss::predicate::{BoundLiteral, BoundPredicate, CompoundFunction, LeafFunction};
use futures::StreamExt;
use paimon::catalog::Identifier;
use paimon::spec::{
    BucketFunctionType, CoreOptions, Datum as PaimonDatum, Predicate as PaimonPredicate,
    PredicateBuilder,
};
use paimon::table::Table;
use paimon::{CatalogFactory, DataSplit, DeletionFile, Options};
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Formatter};

/// Property prefix carrying the Paimon catalog configuration of a table.
const PAIMON_PROPERTY_PREFIX: &str = "table.datalake.paimon.";

/// Paimon table option that pins a scan to one snapshot id.
const PAIMON_SCAN_VERSION_OPTION: &str = "scan.version";

#[derive(Debug, Default)]
pub(crate) struct PlannedPaimonBucket {
    pub(crate) splits: Vec<String>,
    pub(crate) estimated_rows: Option<usize>,
    pub(crate) estimated_size: Option<usize>,
}

pub(crate) struct ExpectedPaimonLayout<'a> {
    pub(crate) partition_keys: &'a [String],
    pub(crate) primary_keys: &'a [String],
    pub(crate) bucket_keys: &'a [String],
    pub(crate) num_buckets: i32,
}

/// Catalog configuration needed to reopen one Paimon table.
///
/// It remains reader-local and is never serialized into a
/// [`crate::FlussLakeReadSplit`].
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PaimonCatalogOptions {
    options: HashMap<String, String>,
}

impl Debug for PaimonCatalogOptions {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaimonCatalogOptions")
            .field("option_count", &self.options.len())
            .finish_non_exhaustive()
    }
}

impl PaimonCatalogOptions {
    /// Merges caller-provided catalog properties over server table metadata.
    ///
    /// Full `table.datalake.paimon.*` keys are preferred. Bare Paimon option
    /// names remain accepted for execution-context compatibility.
    pub(crate) fn from_table_info_with_overrides(
        table_info: &TableInfo,
        overrides: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut options: HashMap<String, String> = table_info
            .properties
            .iter()
            .chain(table_info.custom_properties.iter())
            .filter_map(|(key, value)| {
                key.strip_prefix(PAIMON_PROPERTY_PREFIX)
                    .map(|suffix| (suffix.to_string(), value.clone()))
            })
            .collect();
        for (key, value) in overrides {
            if let Some(suffix) = key.strip_prefix(PAIMON_PROPERTY_PREFIX) {
                options.insert(suffix.to_string(), value.clone());
            } else if !key.starts_with("table.datalake.") {
                options.insert(key.clone(), value.clone());
            }
        }
        if !options.contains_key("warehouse") {
            return Err(FlussLakeError::PlanningFailed(format!(
                "table {} has a readable lake snapshot but no {PAIMON_PROPERTY_PREFIX}warehouse property",
                table_info.table_path
            )));
        }
        Ok(Self { options })
    }

    pub(crate) fn as_map(&self) -> &HashMap<String, String> {
        &self.options
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
) -> Result<Table> {
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
#[allow(dead_code)]
pub(crate) fn projected_field_names(
    row_type: &RowType,
    output_projection: Option<&[usize]>,
) -> Result<Vec<String>> {
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
            FlussLakeError::PlanningFailed(format!(
                "output projection field index {field_index} exceeds table width {}",
                row_type.fields().len()
            ))
        })?;
        names.push(field.name().to_string());
    }
    Ok(names)
}

/// Selects the part of the exact core predicate that is safe to evaluate
/// inside the Paimon baseline reader.
///
/// Append and lake-only reads may push the complete predicate. During a
/// primary-key UnionRead, only primary-key predicates are immutable across
/// the lake baseline and changelog tail. Mixed top-level `AND` expressions
/// therefore contribute only their safe key conjuncts; `OR` is pushed only
/// when every branch is key-only.
pub(crate) fn lake_pushdown_filter(
    predicate: &BoundPredicate,
    table_info: &TableInfo,
    reconcile_primary_key: bool,
) -> Option<BoundPredicate> {
    if matches!(predicate, BoundPredicate::AlwaysTrue) {
        return None;
    }
    if !reconcile_primary_key {
        return Some(predicate.clone());
    }

    let primary_key_indexes: HashSet<usize> = table_info
        .primary_keys
        .iter()
        .filter_map(|key| {
            table_info
                .row_type()
                .fields()
                .iter()
                .position(|field| field.name() == key)
        })
        .collect();
    if primary_key_indexes.len() != table_info.primary_keys.len() {
        return None;
    }
    project_predicate_to_fields(predicate, &primary_key_indexes)
}

fn project_predicate_to_fields(
    predicate: &BoundPredicate,
    allowed_fields: &HashSet<usize>,
) -> Option<BoundPredicate> {
    match predicate {
        BoundPredicate::AlwaysTrue => None,
        BoundPredicate::Leaf { field_index, .. } => allowed_fields
            .contains(field_index)
            .then(|| predicate.clone()),
        BoundPredicate::Compound {
            function: CompoundFunction::And,
            children,
        } => {
            let children: Vec<_> = children
                .iter()
                .filter_map(|child| project_predicate_to_fields(child, allowed_fields))
                .collect();
            (!children.is_empty()).then_some(BoundPredicate::Compound {
                function: CompoundFunction::And,
                children,
            })
        }
        BoundPredicate::Compound {
            function: CompoundFunction::Or,
            children,
        } => {
            let children: Option<Vec<_>> = children
                .iter()
                .map(|child| project_predicate_to_fields(child, allowed_fields))
                .collect();
            Some(BoundPredicate::Compound {
                function: CompoundFunction::Or,
                children: children?,
            })
        }
    }
}

/// Converts a schema-bound core predicate into Paimon's predicate model.
///
/// Conversion failure deliberately disables this optimization instead of
/// failing the query. The Arrow-level evaluator still applies the original
/// bound predicate after reading and remains the source of exact semantics.
fn to_paimon_predicate(
    predicate: &BoundPredicate,
    fields: &[paimon::spec::DataField],
) -> Option<PaimonPredicate> {
    let builder = PredicateBuilder::new(fields);
    convert_paimon_predicate(predicate, &builder).ok()
}

fn convert_paimon_predicate(
    predicate: &BoundPredicate,
    builder: &PredicateBuilder,
) -> paimon::Result<PaimonPredicate> {
    match predicate {
        BoundPredicate::AlwaysTrue => Ok(PaimonPredicate::AlwaysTrue),
        BoundPredicate::Compound { function, children } => {
            let children = children
                .iter()
                .map(|child| convert_paimon_predicate(child, builder))
                .collect::<paimon::Result<Vec<_>>>()?;
            Ok(match function {
                CompoundFunction::And => PaimonPredicate::and(children),
                CompoundFunction::Or => PaimonPredicate::or(children),
            })
        }
        BoundPredicate::Leaf {
            field_name,
            data_type,
            function,
            literals,
            ..
        } => {
            let non_null_literals = || {
                literals
                    .iter()
                    .filter(|literal| !literal.is_null())
                    .map(|literal| to_paimon_datum(literal, data_type))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| paimon::Error::ConfigInvalid {
                        message: format!(
                            "cannot translate the bound predicate on '{field_name}' to Paimon"
                        ),
                    })
            };
            match function {
                LeafFunction::IsNull => builder.is_null(field_name),
                LeafFunction::IsNotNull => builder.is_not_null(field_name),
                LeafFunction::In => {
                    let literals = non_null_literals()?;
                    if literals.is_empty() {
                        Ok(PaimonPredicate::AlwaysFalse)
                    } else {
                        builder.is_in(field_name, literals)
                    }
                }
                LeafFunction::NotIn if literals.iter().any(BoundLiteral::is_null) => {
                    Ok(PaimonPredicate::AlwaysFalse)
                }
                LeafFunction::NotIn => builder.is_not_in(field_name, non_null_literals()?),
                LeafFunction::StartsWith => {
                    builder.starts_with(field_name, required_paimon_literal(literals, data_type)?)
                }
                LeafFunction::EndsWith => {
                    builder.ends_with(field_name, required_paimon_literal(literals, data_type)?)
                }
                LeafFunction::Contains => {
                    builder.contains(field_name, required_paimon_literal(literals, data_type)?)
                }
                LeafFunction::Equal => {
                    builder.equal(field_name, required_paimon_literal(literals, data_type)?)
                }
                LeafFunction::NotEqual => {
                    builder.not_equal(field_name, required_paimon_literal(literals, data_type)?)
                }
                LeafFunction::LessThan => {
                    builder.less_than(field_name, required_paimon_literal(literals, data_type)?)
                }
                LeafFunction::LessOrEqual => {
                    builder.less_or_equal(field_name, required_paimon_literal(literals, data_type)?)
                }
                LeafFunction::GreaterThan => {
                    builder.greater_than(field_name, required_paimon_literal(literals, data_type)?)
                }
                LeafFunction::GreaterOrEqual => builder
                    .greater_or_equal(field_name, required_paimon_literal(literals, data_type)?),
            }
        }
    }
}

fn required_paimon_literal(
    literals: &[BoundLiteral],
    data_type: &DataType,
) -> paimon::Result<PaimonDatum> {
    literals
        .first()
        .and_then(|literal| to_paimon_datum(literal, data_type))
        .ok_or_else(|| paimon::Error::ConfigInvalid {
            message: format!("cannot translate bound literal for Fluss type {data_type}"),
        })
}

fn to_paimon_datum(literal: &BoundLiteral, data_type: &DataType) -> Option<PaimonDatum> {
    match literal {
        BoundLiteral::Null => None,
        BoundLiteral::Boolean(value) => Some(PaimonDatum::Bool(*value)),
        BoundLiteral::Int8(value) => Some(PaimonDatum::TinyInt(*value)),
        BoundLiteral::Int16(value) => Some(PaimonDatum::SmallInt(*value)),
        BoundLiteral::Int32(value) => Some(PaimonDatum::Int(*value)),
        BoundLiteral::Int64(value) => Some(PaimonDatum::Long(*value)),
        BoundLiteral::Float32(value) => Some(PaimonDatum::Float(*value)),
        BoundLiteral::Float64(value) => Some(PaimonDatum::Double(*value)),
        BoundLiteral::String(value) => Some(PaimonDatum::String(value.clone())),
        BoundLiteral::Binary(value) => Some(PaimonDatum::Bytes(value.clone())),
        BoundLiteral::Decimal(value) => {
            let DataType::Decimal(decimal_type) = data_type else {
                return None;
            };
            Some(PaimonDatum::Decimal {
                unscaled: decimal_to_i128(value)?,
                precision: decimal_type.precision(),
                scale: decimal_type.scale(),
            })
        }
        BoundLiteral::Date(value) => Some(PaimonDatum::Date(*value)),
        BoundLiteral::Time(value) => Some(PaimonDatum::Time(*value)),
        BoundLiteral::TimestampNtz(value) => Some(PaimonDatum::Timestamp {
            millis: value.get_millisecond(),
            nanos: value.get_nano_of_millisecond(),
        }),
        BoundLiteral::TimestampLtz(value) => Some(PaimonDatum::LocalZonedTimestamp {
            millis: value.get_epoch_millisecond(),
            nanos: value.get_nano_of_millisecond(),
        }),
    }
}

fn decimal_to_i128(value: &fluss::row::Decimal) -> Option<i128> {
    let bytes = value.to_unscaled_bytes();
    if bytes.len() > size_of::<i128>() {
        return None;
    }
    let fill = if bytes.first().is_some_and(|value| value & 0x80 != 0) {
        0xff
    } else {
        0
    };
    let mut result = [fill; size_of::<i128>()];
    result[size_of::<i128>() - bytes.len()..].copy_from_slice(&bytes);
    Some(i128::from_be_bytes(result))
}

/// Plans the immutable Paimon splits of one readable lake snapshot.
///
/// Splits are returned as JSON so that they can be embedded in opaque split
/// descriptors and shipped to execution workers.
pub(crate) async fn plan_snapshot_splits(
    table_path: &TablePath,
    catalog_options: &PaimonCatalogOptions,
    snapshot_id: i64,
    expected_layout: ExpectedPaimonLayout<'_>,
    validate_merge_engine: bool,
    pushdown_filter: Option<&BoundPredicate>,
) -> Result<HashMap<(FlussLakePartitionIdentity, i32), PlannedPaimonBucket>> {
    let table = open_pinned_table(table_path, catalog_options, snapshot_id).await?;
    if validate_merge_engine {
        ensure_deduplicate_merge_engine(table.schema().options(), table_path)?;
    }
    validate_paimon_layout(
        table_path,
        table.schema().partition_keys(),
        table.schema().primary_keys(),
        table.schema().trimmed_primary_keys(),
        table.schema().options(),
        &expected_layout,
    )?;
    let partition_keys = table.schema().partition_keys().to_vec();
    let mut read_builder = table.new_read_builder();
    if let Some(filter) =
        pushdown_filter.and_then(|filter| to_paimon_predicate(filter, table.schema().fields()))
    {
        read_builder.with_filter(filter);
    }
    let plan = read_builder
        .new_scan()
        .plan()
        .await
        .map_err(|error| paimon_error("plan Paimon snapshot splits", error))?;

    let mut grouped = HashMap::new();
    for split in plan.splits() {
        if split.snapshot_id() != snapshot_id {
            return Err(FlussLakeError::PlanningFailed(format!(
                "Paimon scan pinned to snapshot {snapshot_id} of {table_path} produced a split for snapshot {}",
                split.snapshot_id()
            )));
        }
        if split.bucket() < 0 {
            return Err(FlussLakeError::PlanningFailed(format!(
                "Paimon snapshot {snapshot_id} of {table_path} produced a split with negative bucket {}",
                split.bucket()
            )));
        }
        let partition = split_partition_identity(split, &partition_keys)?;
        let bucket = grouped
            .entry((partition, split.bucket()))
            .or_insert_with(|| PlannedPaimonBucket {
                splits: Vec::new(),
                estimated_rows: Some(0),
                estimated_size: Some(0),
            });
        bucket
            .splits
            .push(encode_portable_split(split, table.location())?);
        bucket.estimated_rows = add_i64_estimate(bucket.estimated_rows, split.merged_row_count());
        let split_size = split
            .data_files()
            .iter()
            .try_fold(0_i64, |total, file| total.checked_add(file.file_size));
        bucket.estimated_size = add_i64_estimate(bucket.estimated_size, split_size);
    }
    Ok(grouped)
}

fn validate_paimon_layout(
    table_path: &TablePath,
    actual_partition_keys: &[String],
    actual_primary_keys: &[String],
    default_bucket_keys: Vec<String>,
    table_options: &HashMap<String, String>,
    expected: &ExpectedPaimonLayout<'_>,
) -> Result<()> {
    if actual_partition_keys != expected.partition_keys {
        return Err(FlussLakeError::PlanningFailed(format!(
            "Paimon partition keys {:?} do not match Fluss partition keys {:?} for {table_path}",
            actual_partition_keys, expected.partition_keys
        )));
    }
    if actual_primary_keys != expected.primary_keys {
        return Err(FlussLakeError::PlanningFailed(format!(
            "Paimon primary keys {:?} do not match Fluss primary keys {:?} for {table_path}",
            actual_primary_keys, expected.primary_keys
        )));
    }
    let core_options = CoreOptions::new(table_options);
    let bucket_keys = core_options.bucket_key().unwrap_or(default_bucket_keys);
    if bucket_keys != expected.bucket_keys {
        return Err(FlussLakeError::PlanningFailed(format!(
            "Paimon bucket keys {:?} do not match Fluss bucket keys {:?} for {table_path}",
            bucket_keys, expected.bucket_keys
        )));
    }
    if core_options.bucket() != expected.num_buckets {
        return Err(FlussLakeError::PlanningFailed(format!(
            "Paimon bucket count {} does not match Fluss bucket count {} for {table_path}",
            core_options.bucket(),
            expected.num_buckets
        )));
    }
    let bucket_function = core_options.bucket_function_type().map_err(|error| {
        FlussLakeError::PlanningFailed(format!(
            "failed to resolve the Paimon bucket function of {table_path}: {error}"
        ))
    })?;
    if bucket_function != BucketFunctionType::Default {
        return Err(FlussLakeError::PlanningFailed(format!(
            "Paimon bucket function {bucket_function:?} is incompatible with Fluss bucket assignment for {table_path}"
        )));
    }
    Ok(())
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
) -> Result<()> {
    let merge_engine = paimon::spec::CoreOptions::new(table_options)
        .merge_engine()
        .map_err(|error| {
            FlussLakeError::UnsupportedMergeEngine(format!(
                "failed to resolve the Paimon merge engine of {table_path}: {error}"
            ))
        })?;
    if merge_engine != paimon::spec::MergeEngine::Deduplicate {
        return Err(FlussLakeError::UnsupportedMergeEngine(format!(
            "primary-key UnionRead only supports the deduplicate merge engine, but the Paimon table for {table_path} uses {merge_engine:?}; refusing to plan a read that would silently produce an incorrect current view"
        )));
    }
    Ok(())
}

fn add_i64_estimate(current: Option<usize>, increment: Option<i64>) -> Option<usize> {
    current?.checked_add(usize::try_from(increment?).ok()?)
}

fn split_partition_identity(
    split: &DataSplit,
    partition_keys: &[String],
) -> Result<FlussLakePartitionIdentity> {
    if partition_keys.is_empty() {
        return Ok(FlussLakePartitionIdentity::Unpartitioned);
    }
    let bucket_segment = format!("bucket-{}", split.bucket());
    let mut segments: Vec<&str> = split
        .bucket_path()
        .trim_end_matches('/')
        .split('/')
        .collect();
    if segments.pop() != Some(bucket_segment.as_str()) || segments.len() < partition_keys.len() {
        return Err(FlussLakeError::PlanningFailed(format!(
            "Paimon split bucket path '{}' does not end in the expected partition/bucket layout",
            split.bucket_path()
        )));
    }
    let partition_segments = &segments[segments.len() - partition_keys.len()..];
    let mut key_values = Vec::with_capacity(partition_keys.len());
    for (expected_key, segment) in partition_keys.iter().zip(partition_segments) {
        let (encoded_key, encoded_value) = segment.split_once('=').ok_or_else(|| {
            FlussLakeError::PlanningFailed(format!(
                "Paimon split bucket path '{}' contains partition segment '{segment}' without key=value syntax",
                split.bucket_path()
            ))
        })?;
        let key = unescape_path_name(encoded_key).ok_or_else(|| {
            FlussLakeError::PlanningFailed(format!(
                "Paimon split bucket path '{}' contains invalid escaped partition key '{encoded_key}'",
                split.bucket_path()
            ))
        })?;
        if key != *expected_key {
            return Err(FlussLakeError::PlanningFailed(format!(
                "Paimon split bucket path '{}' does not match partition key '{expected_key}'",
                split.bucket_path()
            )));
        }
        let value = unescape_path_name(encoded_value).ok_or_else(|| {
            FlussLakeError::PlanningFailed(format!(
                "Paimon split bucket path '{}' contains invalid escaped value for partition key '{expected_key}'",
                split.bucket_path()
            ))
        })?;
        key_values.push((key, value));
    }
    Ok(FlussLakePartitionIdentity::KeyValues(key_values))
}

fn unescape_path_name(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut unescaped = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            unescaped.push((high << 4) | low);
            index += 3;
        } else {
            unescaped.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(unescaped).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_portable_split(split: &DataSplit, table_location: &str) -> Result<String> {
    validate_split_file_names(split, |field, path, reason| {
        FlussLakeError::PlanningFailed(format!(
            "Paimon split {field} '{path}' cannot be distributed safely: {reason}"
        ))
    })?;
    let portable = rewrite_split_paths(split, |path, field| {
        storage_relative_path(table_location, path).map_err(|reason| {
            FlussLakeError::PlanningFailed(format!(
                "Paimon split {field} '{path}' cannot be distributed safely: {reason}"
            ))
        })
    })?;
    serde_json::to_string(&portable).map_err(|error| {
        FlussLakeError::PlanningFailed(format!("failed to serialize Paimon split: {error}"))
    })
}

/// Reads several frozen Paimon splits as one finite Arrow batch stream.
///
/// All splits must come from the same pinned snapshot. Reading them through
/// one Paimon reader matters for primary-key tables: since
/// apache/paimon-rust#374 the reader deduplicates keys across the splits it
/// is given, which is exactly the per-bucket exactly-once guarantee the
/// primary-key merge presumes.
#[allow(dead_code)]
pub(crate) async fn read_snapshot_splits(
    table_path: &TablePath,
    catalog_options: &PaimonCatalogOptions,
    snapshot_id: i64,
    expected_bucket_id: i32,
    projected_fields: Option<&[String]>,
    encoded_splits: &[String],
    pushdown_filter: Option<&BoundPredicate>,
) -> Result<RecordBatchStream> {
    let table = open_pinned_table(table_path, catalog_options, snapshot_id).await?;
    let splits = encoded_splits
        .iter()
        .map(|encoded| decode_portable_split(encoded, table.location()))
        .collect::<Result<Vec<_>>>()?;
    let mut expected_bucket_path: Option<&str> = None;
    for split in &splits {
        if split.snapshot_id() != snapshot_id {
            return Err(FlussLakeError::Internal(format!(
                "Paimon split snapshot id {} does not match frozen snapshot id {snapshot_id}",
                split.snapshot_id()
            )));
        }
        if split.bucket() != expected_bucket_id {
            return Err(FlussLakeError::Internal(format!(
                "Paimon split bucket id {} does not match logical split bucket id {expected_bucket_id}",
                split.bucket()
            )));
        }
        match expected_bucket_path {
            Some(path) if path != split.bucket_path() => {
                return Err(FlussLakeError::Internal(format!(
                    "one logical bucket split contains multiple Paimon bucket paths: '{path}' and '{}'",
                    split.bucket_path()
                )));
            }
            None => expected_bucket_path = Some(split.bucket_path()),
            _ => {}
        }
    }
    let mut read_builder = table.new_read_builder();
    if let Some(field_names) = projected_fields {
        let borrowed: Vec<&str> = field_names.iter().map(String::as_str).collect();
        read_builder
            .with_projection(&borrowed)
            .map_err(|error| paimon_error("apply Paimon read projection", error))?;
    }
    if let Some(filter) =
        pushdown_filter.and_then(|filter| to_paimon_predicate(filter, table.schema().fields()))
    {
        read_builder.with_filter(filter);
    }
    let stream = read_builder
        .new_read()
        .map_err(|error| paimon_error("create Paimon reader", error))?
        .to_arrow(&splits)
        .map_err(|error| paimon_error("read Paimon splits", error))?;

    Ok(Box::pin(stream.map(|result| {
        result
            .map_err(|error| paimon_error("read Paimon split batch", error))
            .and_then(upgrade_paimon_arrow_batch)
    })))
}

/// Imports a Paimon Arrow 58 batch into the workspace Arrow 59 ABI without
/// copying its buffers.
///
/// Both versions implement the stable Arrow C Data Interface. The producer's
/// release callbacks remain attached to the exported structs, so imported
/// Arrow 59 arrays keep the Arrow 58 buffers alive until their final drop.
#[allow(dead_code)]
fn upgrade_paimon_arrow_batch(batch: arrow_array_58::RecordBatch) -> Result<RecordBatch> {
    use arrow::array::ffi::{
        FFI_ArrowArray as ArrowArray59, FFI_ArrowSchema as ArrowSchema59, from_ffi,
    };
    use arrow_array_58::StructArray as StructArray58;
    use std::mem::{align_of, size_of};
    use std::ptr::addr_of_mut;
    use std::sync::Arc;

    if size_of::<ArrowArray59>() != size_of::<arrow_array_58::ffi::FFI_ArrowArray>()
        || align_of::<ArrowArray59>() != align_of::<arrow_array_58::ffi::FFI_ArrowArray>()
        || size_of::<ArrowSchema59>() != size_of::<arrow_array_58::ffi::FFI_ArrowSchema>()
        || align_of::<ArrowSchema59>() != align_of::<arrow_array_58::ffi::FFI_ArrowSchema>()
    {
        return Err(FlussLakeError::Internal(
            "Arrow 58 and Arrow 59 C Data Interface layouts are incompatible".to_string(),
        ));
    }

    let source: arrow_array_58::ArrayRef = Arc::new(StructArray58::from(batch));
    let mut array = ArrowArray59::empty();
    let mut schema = ArrowSchema59::empty();
    // SAFETY: Both FFI structs are `repr(C)` implementations of the same
    // stable Arrow C Data Interface. Size and alignment are checked above.
    // Arrow 58 initializes the Arrow 59 storage in place, including producer
    // release callbacks; `from_ffi` then takes ownership of those callbacks.
    #[allow(deprecated)]
    unsafe {
        arrow_array_58::ffi::export_array_into_raw(
            source,
            addr_of_mut!(array).cast(),
            addr_of_mut!(schema).cast(),
        )
        .map_err(|error| {
            FlussLakeError::Internal(format!(
                "failed to export a Paimon Arrow batch through the C Data Interface: {error}"
            ))
        })?;
    }
    // SAFETY: `array` and `schema` were initialized together by the Arrow 58
    // exporter and ownership is transferred exactly once to Arrow 59.
    let data = unsafe { from_ffi(array, &schema) }.map_err(|error| {
        FlussLakeError::Internal(format!(
            "failed to import a Paimon Arrow batch through the C Data Interface: {error}"
        ))
    })?;
    Ok(RecordBatch::from(StructArray::from(data)))
}

#[allow(dead_code)]
fn decode_portable_split(encoded_split: &str, table_location: &str) -> Result<DataSplit> {
    let portable: DataSplit = serde_json::from_str(encoded_split).map_err(|error| {
        FlussLakeError::Internal(format!("failed to decode Paimon split: {error}"))
    })?;
    validate_split_file_names(&portable, |field, path, reason| {
        FlussLakeError::Internal(format!(
            "Paimon split {field} '{path}' is not a valid storage-relative path: {reason}"
        ))
    })?;
    rewrite_split_paths(&portable, |path, field| {
        absolute_storage_path(table_location, path).map_err(|reason| {
            FlussLakeError::Internal(format!(
                "Paimon split {field} '{path}' is not a valid storage-relative path: {reason}"
            ))
        })
    })
}

fn validate_split_file_names<F>(split: &DataSplit, mut invalid: F) -> Result<()>
where
    F: FnMut(&str, &str, String) -> FlussLakeError,
{
    for (file_index, file) in split.data_files().iter().enumerate() {
        validate_storage_relative_path(&file.file_name).map_err(|reason| {
            invalid(
                &format!("data file {file_index} name"),
                &file.file_name,
                reason,
            )
        })?;
        for (extra_index, extra_file) in file.extra_files.iter().enumerate() {
            validate_storage_relative_path(extra_file).map_err(|reason| {
                invalid(
                    &format!("data file {file_index} extra file {extra_index} name"),
                    extra_file,
                    reason,
                )
            })?;
        }
    }
    Ok(())
}

fn rewrite_split_paths<F>(split: &DataSplit, mut rewrite: F) -> Result<DataSplit>
where
    F: FnMut(&str, &str) -> Result<String>,
{
    let bucket_path = rewrite(split.bucket_path(), "bucket path")?;
    let mut data_files = split.data_files().to_vec();
    for (index, file) in data_files.iter_mut().enumerate() {
        if let Some(path) = file.external_path.as_deref() {
            file.external_path = Some(rewrite(path, &format!("data file {index} external path"))?);
        }
    }
    let deletion_files = split
        .data_deletion_files()
        .map(|files| {
            files
                .iter()
                .enumerate()
                .map(|(index, file)| {
                    file.as_ref()
                        .map(|file| {
                            rewrite(file.path(), &format!("deletion file {index} path")).map(
                                |path| {
                                    DeletionFile::new(
                                        path,
                                        file.offset(),
                                        file.length(),
                                        file.cardinality(),
                                    )
                                },
                            )
                        })
                        .transpose()
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;

    let mut builder = DataSplit::builder()
        .with_snapshot(split.snapshot_id())
        .with_partition(split.partition().clone())
        .with_bucket(split.bucket())
        .with_bucket_path(bucket_path)
        .with_total_buckets(split.total_buckets())
        .with_data_files(data_files)
        .with_raw_convertible(split.raw_convertible());
    if let Some(deletion_files) = deletion_files {
        builder = builder.with_data_deletion_files(deletion_files);
    }
    if let Some(row_ranges) = split.row_ranges() {
        builder = builder.with_row_ranges(row_ranges.to_vec());
    }
    builder.build().map_err(|error| {
        FlussLakeError::Internal(format!(
            "failed to rebuild Paimon split while rewriting storage paths: {error}"
        ))
    })
}

fn storage_relative_path(table_location: &str, path: &str) -> std::result::Result<String, String> {
    let table_location = table_location.trim_end_matches('/');
    if table_location.is_empty() {
        return Err("the resolved Paimon table location is empty".to_string());
    }
    let relative = if let Some(suffix) = path.strip_prefix(table_location) {
        suffix
            .strip_prefix('/')
            .ok_or_else(|| format!("path is outside the Paimon table root '{table_location}'"))?
    } else {
        validate_storage_relative_path(path)?;
        return Ok(path.to_string());
    };
    validate_storage_relative_path(relative)?;
    Ok(relative.to_string())
}

#[allow(dead_code)]
fn absolute_storage_path(
    table_location: &str,
    relative: &str,
) -> std::result::Result<String, String> {
    validate_storage_relative_path(relative)?;
    let table_location = table_location.trim_end_matches('/');
    if table_location.is_empty() {
        return Err("the resolved Paimon table location is empty".to_string());
    }
    Ok(format!("{table_location}/{relative}"))
}

fn validate_storage_relative_path(path: &str) -> std::result::Result<(), String> {
    if path.is_empty() {
        return Err("path is empty".to_string());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err("absolute paths are forbidden".to_string());
    }
    if path.contains('\\') {
        return Err("backslash path separators are forbidden".to_string());
    }
    if path
        .split('/')
        .next()
        .is_some_and(|first_segment| first_segment.contains(':'))
    {
        return Err("URI schemes and drive prefixes are forbidden".to_string());
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err("empty, '.' and '..' path segments are forbidden".to_string());
    }
    Ok(())
}

fn paimon_error(action: &str, error: paimon::Error) -> FlussLakeError {
    // Storage errors may be wrapped by Parquet before they reach Paimon. The
    // outer variant is then `ParquetDataUnexpected`, while the rendered error
    // chain still contains the underlying OpenDAL NotFound classification.
    let message = error.to_string().to_ascii_lowercase();
    let unavailable = message.contains("notfound")
        || message.contains("not found")
        || message.contains("does not exist")
        || message.contains("no such file")
        || message.contains("entity not found");
    let connection_error = matches!(
        &error,
        paimon::Error::IoUnexpected { .. } | paimon::Error::RestApi { .. }
    ) || message.contains("connection")
        || message.contains("timed out")
        || message.contains("timeout")
        || message.contains("temporarily unavailable");
    if unavailable {
        FlussLakeError::DataUnavailable(format!("failed to {action}: {error}"))
    } else if connection_error {
        FlussLakeError::ConnectionError(format!("failed to {action}: {error}"))
    } else {
        FlussLakeError::Internal(format!("failed to {action}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array_58::{ArrayRef as ArrayRef58, Int32Array as Int32Array58};
    use fluss::metadata::{DataField, DataTypes, Schema};
    use fluss::predicate::{Literal, Predicate, col};
    use paimon::spec::DataFileMeta;
    use std::sync::Arc;

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("name", DataTypes::string(), None),
            DataField::new("amount", DataTypes::bigint(), None),
        ])
    }

    #[test]
    fn imports_paimon_arrow_batches_without_changing_values() {
        let source = arrow_array_58::RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(Int32Array58::from(vec![Some(1), None, Some(3)])) as ArrayRef58,
        )])
        .unwrap();

        let imported = upgrade_paimon_arrow_batch(source).unwrap();
        let ids = imported
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int32Array>()
            .unwrap();

        assert_eq!(imported.schema().field(0).name(), "id");
        assert_eq!(ids.iter().collect::<Vec<_>>(), vec![Some(1), None, Some(3)]);
    }

    fn table_info(
        properties: HashMap<String, String>,
        custom_properties: HashMap<String, String>,
    ) -> TableInfo {
        let schema = Schema::builder()
            .column("id", DataTypes::int())
            .column("name", DataTypes::string())
            .build()
            .unwrap();
        TableInfo::new(
            TablePath::new("fluss", "orders"),
            7,
            1,
            schema,
            vec!["id".to_string()],
            Vec::<String>::new().into(),
            1,
            properties,
            custom_properties,
            None,
            0,
            0,
        )
    }

    fn pk_table_info() -> TableInfo {
        let schema = Schema::builder()
            .column("id", DataTypes::int())
            .column("name", DataTypes::string())
            .column("amount", DataTypes::bigint())
            .primary_key(["id"])
            .build()
            .unwrap();
        TableInfo::new(
            TablePath::new("fluss", "pk_orders"),
            7,
            1,
            schema,
            vec!["id".to_string()],
            Vec::<String>::new().into(),
            4,
            HashMap::new(),
            HashMap::new(),
            None,
            0,
            0,
        )
    }

    #[test]
    fn pk_union_pushes_only_safe_primary_key_conjuncts() {
        let table_info = pk_table_info();
        let predicate = col("id").eq(1_i32).and(col("amount").gt(10_i64));
        let bound = BoundPredicate::bind(Some(&predicate), table_info.row_type()).unwrap();

        let union_filter = lake_pushdown_filter(&bound, &table_info, true).unwrap();
        assert_eq!(union_filter.referenced_field_indexes(), vec![0]);

        let lake_only_filter = lake_pushdown_filter(&bound, &table_info, false).unwrap();
        assert_eq!(lake_only_filter.referenced_field_indexes(), vec![0, 2]);

        let mixed_or = col("id").eq(1_i32).or(col("amount").gt(10_i64));
        let mixed_or = BoundPredicate::bind(Some(&mixed_or), table_info.row_type()).unwrap();
        assert!(lake_pushdown_filter(&mixed_or, &table_info, true).is_none());
    }

    #[test]
    fn translates_core_string_membership_and_null_semantics_to_paimon() {
        use paimon::spec::{
            DataField as PaimonField, DataType as PaimonType, IntType, VarCharType,
        };

        let fields = vec![
            PaimonField::new(0, "id".to_string(), PaimonType::Int(IntType::new())),
            PaimonField::new(
                1,
                "name".to_string(),
                PaimonType::VarChar(VarCharType::string_type()),
            ),
        ];
        let row_type = RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("name", DataTypes::string(), None),
        ]);
        let predicate = col("id")
            .is_in([1_i32, 2_i32])
            .and(col("name").contains("a"));
        let bound = BoundPredicate::bind(Some(&predicate), &row_type).unwrap();
        assert!(matches!(
            to_paimon_predicate(&bound, &fields),
            Some(PaimonPredicate::And(children)) if children.len() == 2
        ));

        let not_in_with_null = Predicate::Leaf {
            field: "id".to_string(),
            function: LeafFunction::NotIn,
            literals: vec![Literal::Int32(1), Literal::Null],
        };
        let bound = BoundPredicate::bind(Some(&not_in_with_null), &row_type).unwrap();
        assert_eq!(
            to_paimon_predicate(&bound, &fields),
            Some(PaimonPredicate::AlwaysFalse)
        );
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
    fn caller_catalog_properties_override_server_metadata() {
        let mut properties = HashMap::new();
        properties.insert(
            "table.datalake.paimon.warehouse".to_string(),
            "s3://server/warehouse".to_string(),
        );
        properties.insert(
            "table.datalake.paimon.s3.endpoint".to_string(),
            "http://server:9000".to_string(),
        );
        let mut overrides = HashMap::new();
        overrides.insert(
            "table.datalake.paimon.warehouse".to_string(),
            "s3://caller/warehouse".to_string(),
        );
        overrides.insert(
            "table.datalake.paimon.s3.secret-key".to_string(),
            "CALLER-SECRET".to_string(),
        );

        let options = PaimonCatalogOptions::from_table_info_with_overrides(
            &table_info(properties, HashMap::new()),
            &overrides,
        )
        .unwrap();

        assert_eq!(
            options.as_map().get("warehouse"),
            Some(&"s3://caller/warehouse".to_string())
        );
        assert_eq!(
            options.as_map().get("s3.endpoint"),
            Some(&"http://server:9000".to_string())
        );
        assert_eq!(
            options.as_map().get("s3.secret-key"),
            Some(&"CALLER-SECRET".to_string())
        );
        let debug = format!("{options:?}");
        assert!(!debug.contains("CALLER-SECRET"));
        assert!(!debug.contains("s3.secret-key"));
    }

    #[test]
    fn rejects_projection_beyond_the_frozen_schema() {
        assert!(matches!(
            projected_field_names(&row_type(), Some(&[3])),
            Err(FlussLakeError::PlanningFailed(_))
        ));
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
                    Err(FlussLakeError::UnsupportedMergeEngine(_))
                ),
                "merge engine {rejected} must be rejected at planning"
            );
        }
    }

    #[test]
    fn paimon_layout_must_match_fluss_partition_primary_and_bucket_contract() {
        let table_path = TablePath::new("fluss", "pk_orders");
        let partition_keys = vec!["region".to_string()];
        let primary_keys = vec!["id".to_string(), "region".to_string()];
        let bucket_keys = vec!["id".to_string()];
        let matching_options = HashMap::from([
            ("bucket".to_string(), "4".to_string()),
            ("bucket-key".to_string(), "id".to_string()),
        ]);

        validate_paimon_layout(
            &table_path,
            &partition_keys,
            &primary_keys,
            bucket_keys.clone(),
            &matching_options,
            &ExpectedPaimonLayout {
                partition_keys: &partition_keys,
                primary_keys: &primary_keys,
                bucket_keys: &bucket_keys,
                num_buckets: 4,
            },
        )
        .unwrap();

        for (actual_primary_keys, options) in [
            (
                vec!["other".to_string(), "region".to_string()],
                matching_options.clone(),
            ),
            (
                primary_keys.clone(),
                HashMap::from([
                    ("bucket".to_string(), "2".to_string()),
                    ("bucket-key".to_string(), "id".to_string()),
                ]),
            ),
            (
                primary_keys.clone(),
                HashMap::from([
                    ("bucket".to_string(), "4".to_string()),
                    ("bucket-key".to_string(), "other".to_string()),
                ]),
            ),
            (
                primary_keys.clone(),
                HashMap::from([
                    ("bucket".to_string(), "4".to_string()),
                    ("bucket-key".to_string(), "id".to_string()),
                    ("bucket-function.type".to_string(), "mod".to_string()),
                ]),
            ),
        ] {
            assert!(matches!(
                validate_paimon_layout(
                    &table_path,
                    &partition_keys,
                    &actual_primary_keys,
                    bucket_keys.clone(),
                    &options,
                    &ExpectedPaimonLayout {
                        partition_keys: &partition_keys,
                        primary_keys: &primary_keys,
                        bucket_keys: &bucket_keys,
                        num_buckets: 4,
                    },
                ),
                Err(FlussLakeError::PlanningFailed(_))
            ));
        }
    }

    #[test]
    fn missing_snapshot_file_maps_to_data_unavailable() {
        let error = paimon::Error::DataInvalid {
            message: "snapshot file does not exist: snapshot-42".to_string(),
            source: None,
        };

        assert!(matches!(
            paimon_error("open pinned snapshot", error),
            FlussLakeError::DataUnavailable(_)
        ));
    }

    #[test]
    fn extracts_partition_and_bucket_identity_from_split_path() {
        let split = DataSplit::builder()
            .with_snapshot(42)
            .with_partition(paimon::spec::BinaryRow::new(2))
            .with_bucket(3)
            .with_bucket_path(
                "s3://warehouse/fluss/orders/region=U%2FS/day=a%3Db/bucket-3".to_string(),
            )
            .with_total_buckets(4)
            .with_data_files(Vec::new())
            .build()
            .unwrap();

        assert_eq!(
            split_partition_identity(&split, &["region".to_string(), "day".to_string()]).unwrap(),
            FlussLakePartitionIdentity::KeyValues(vec![
                ("region".to_string(), "U/S".to_string()),
                ("day".to_string(), "a=b".to_string()),
            ])
        );
    }

    #[test]
    fn rejects_invalid_or_reordered_partition_paths() {
        let split = |path: &str| {
            DataSplit::builder()
                .with_snapshot(42)
                .with_partition(paimon::spec::BinaryRow::new(1))
                .with_bucket(3)
                .with_bucket_path(path.to_string())
                .with_total_buckets(4)
                .with_data_files(Vec::new())
                .build()
                .unwrap()
        };

        assert!(matches!(
            split_partition_identity(
                &split("s3://warehouse/fluss/orders/day=1/region=US/bucket-3"),
                &["region".to_string(), "day".to_string()]
            ),
            Err(FlussLakeError::PlanningFailed(_))
        ));
        assert!(matches!(
            split_partition_identity(
                &split("s3://warehouse/fluss/orders/region=%ZZ/bucket-3"),
                &["region".to_string()]
            ),
            Err(FlussLakeError::PlanningFailed(_))
        ));
    }

    #[test]
    fn portable_split_paths_are_relative_and_restored_at_execution() {
        let table_location = "s3://warehouse/fluss/orders";
        let split = DataSplit::builder()
            .with_snapshot(42)
            .with_partition(paimon::spec::BinaryRow::new(0))
            .with_bucket(3)
            .with_bucket_path(format!("{table_location}/region=US/bucket-3"))
            .with_total_buckets(4)
            .with_data_files(Vec::new())
            .build()
            .unwrap();

        let encoded = encode_portable_split(&split, table_location).unwrap();
        assert!(!encoded.contains(table_location));
        assert!(encoded.contains("region=US/bucket-3"));

        let restored = decode_portable_split(&encoded, table_location).unwrap();
        assert_eq!(
            restored.bucket_path(),
            "s3://warehouse/fluss/orders/region=US/bucket-3"
        );
    }

    #[test]
    fn portable_split_rewrites_every_embedded_storage_path() {
        let table_location = "s3://warehouse/fluss/orders";
        let data_file: DataFileMeta = serde_json::from_value(serde_json::json!({
            "_FILE_NAME": "data-0.parquet",
            "_FILE_SIZE": 10,
            "_ROW_COUNT": 1,
            "_MIN_KEY": [],
            "_MAX_KEY": [],
            "_KEY_STATS": {
                "_MIN_VALUES": [],
                "_MAX_VALUES": [],
                "_NULL_COUNTS": []
            },
            "_VALUE_STATS": {
                "_MIN_VALUES": [],
                "_MAX_VALUES": [],
                "_NULL_COUNTS": []
            },
            "_MIN_SEQUENCE_NUMBER": 0,
            "_MAX_SEQUENCE_NUMBER": 0,
            "_SCHEMA_ID": 1,
            "_LEVEL": 1,
            "_EXTRA_FILES": [],
            "_CREATION_TIME": null,
            "_DELETE_ROW_COUNT": 0,
            "_EMBEDDED_FILE_INDEX": null,
            "_EXTERNAL_PATH": format!("{table_location}/external/data-0.parquet")
        }))
        .unwrap();
        let split = DataSplit::builder()
            .with_snapshot(42)
            .with_partition(paimon::spec::BinaryRow::new(0))
            .with_bucket(0)
            .with_bucket_path(format!("{table_location}/bucket-0"))
            .with_total_buckets(1)
            .with_data_files(vec![data_file])
            .with_data_deletion_files(vec![Some(DeletionFile::new(
                format!("{table_location}/index/deletion-vector"),
                7,
                11,
                Some(1),
            ))])
            .build()
            .unwrap();

        let encoded = encode_portable_split(&split, table_location).unwrap();
        assert!(!encoded.contains(table_location));

        let restored = decode_portable_split(&encoded, table_location).unwrap();
        assert_eq!(
            restored.data_files()[0].external_path.as_deref(),
            Some("s3://warehouse/fluss/orders/external/data-0.parquet")
        );
        assert_eq!(
            restored.data_deletion_files().unwrap()[0]
                .as_ref()
                .unwrap()
                .path(),
            "s3://warehouse/fluss/orders/index/deletion-vector"
        );
    }

    #[test]
    fn portable_split_rejects_paths_outside_the_table_root() {
        let split = DataSplit::builder()
            .with_snapshot(42)
            .with_partition(paimon::spec::BinaryRow::new(0))
            .with_bucket(0)
            .with_bucket_path("s3://other-bucket/orders/bucket-0".to_string())
            .with_total_buckets(1)
            .with_data_files(Vec::new())
            .build()
            .unwrap();

        assert!(matches!(
            encode_portable_split(&split, "s3://warehouse/fluss/orders"),
            Err(FlussLakeError::PlanningFailed(_))
        ));
    }

    #[test]
    fn reader_rejects_absolute_paths_in_distributed_split_payloads() {
        let split = DataSplit::builder()
            .with_snapshot(42)
            .with_partition(paimon::spec::BinaryRow::new(0))
            .with_bucket(0)
            .with_bucket_path("s3://attacker-controlled/orders/bucket-0".to_string())
            .with_total_buckets(1)
            .with_data_files(Vec::new())
            .build()
            .unwrap();
        let encoded = serde_json::to_string(&split).unwrap();

        assert!(matches!(
            decode_portable_split(&encoded, "s3://warehouse/fluss/orders"),
            Err(FlussLakeError::Internal(_))
        ));
    }

    #[test]
    fn reader_rejects_unsafe_data_and_extra_file_names() {
        let table_location = "s3://warehouse/fluss/orders";
        let data_file: DataFileMeta = serde_json::from_value(serde_json::json!({
            "_FILE_NAME": "data-0.parquet",
            "_FILE_SIZE": 10,
            "_ROW_COUNT": 1,
            "_MIN_KEY": [],
            "_MAX_KEY": [],
            "_KEY_STATS": {
                "_MIN_VALUES": [],
                "_MAX_VALUES": [],
                "_NULL_COUNTS": []
            },
            "_VALUE_STATS": {
                "_MIN_VALUES": [],
                "_MAX_VALUES": [],
                "_NULL_COUNTS": []
            },
            "_MIN_SEQUENCE_NUMBER": 0,
            "_MAX_SEQUENCE_NUMBER": 0,
            "_SCHEMA_ID": 1,
            "_LEVEL": 1,
            "_EXTRA_FILES": ["data-0.index"],
            "_CREATION_TIME": null,
            "_DELETE_ROW_COUNT": 0,
            "_EMBEDDED_FILE_INDEX": null
        }))
        .unwrap();
        let split_with_file = |data_file: DataFileMeta| {
            DataSplit::builder()
                .with_snapshot(42)
                .with_partition(paimon::spec::BinaryRow::new(0))
                .with_bucket(0)
                .with_bucket_path("bucket-0".to_string())
                .with_total_buckets(1)
                .with_data_files(vec![data_file])
                .build()
                .unwrap()
        };

        for unsafe_name in [
            "../outside.parquet",
            "/tmp/outside.parquet",
            "s3://attacker/outside.parquet",
            "nested/../../outside.parquet",
        ] {
            let mut tampered = data_file.clone();
            tampered.file_name = unsafe_name.to_string();
            let encoded = serde_json::to_string(&split_with_file(tampered)).unwrap();
            assert!(
                matches!(
                    decode_portable_split(&encoded, table_location),
                    Err(FlussLakeError::Internal(_))
                ),
                "reader must reject unsafe data file name {unsafe_name}"
            );

            let mut tampered = data_file.clone();
            tampered.extra_files = vec![unsafe_name.to_string()];
            let encoded = serde_json::to_string(&split_with_file(tampered)).unwrap();
            assert!(
                matches!(
                    decode_portable_split(&encoded, table_location),
                    Err(FlussLakeError::Internal(_))
                ),
                "reader must reject unsafe extra file name {unsafe_name}"
            );
        }
    }
}
