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

//! Schema-aware streaming conversion from one raw FIP-49 JSON row to [`GenericRow`].
//!
//! Serde owns JSON syntax and container traversal. Schema-matching ARRAY, MAP, and ROW values
//! recurse only along the validated [`RowType`], bounded by [`MAX_TYPE_NESTING`]. Scalar and
//! ignored subtrees use serde_json's iterative [`RawValue`] and [`IgnoredAny`] scan path, so JSON
//! depth does not grow this decoder's call stack. No container subtree is reparsed, keeping work
//! linear in row bytes apart from local scalar decoding; the HTTP layer separately bounds body
//! size. Recheck the iterative-scan assumption when upgrading serde_json.

use crate::error::GatewayError;
use crate::protocol::rest::codec::temporal::{parse_date, parse_time, parse_timestamp};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fluss::metadata::{DataType, RowType};
use fluss::row::{
    Date, Datum, Decimal, FlussArrayWriter, FlussMapWriter, GenericRow, Time, TimestampLtz,
    TimestampNtz,
};
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

/// Bounds schema-recursive validation and decoding; it is not a JSON body-depth limit.
const MAX_TYPE_NESTING: usize = 64;
const NANOS_PER_MILLI: i64 = 1_000_000;
const MILLIS_PER_SECOND: i64 = 1_000;
const MILLIS_PER_DAY: i64 = 86_400_000;

/// Whether a row supplies a complete mutation or only selected fields.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) enum RowShape<'a> {
    /// Every non-nullable column must be present. Missing nullable columns become null.
    Complete,
    /// Only the named columns must be present and non-null. Other missing columns become null.
    Sparse(&'a [String]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowDecodeErrorKind {
    SchemaMismatch,
    InvalidValue,
}

/// One row decode failure plus whether refreshed metadata could plausibly resolve it.
#[derive(Debug)]
pub(crate) struct RowDecodeError {
    kind: RowDecodeErrorKind,
    error: GatewayError,
}

#[allow(dead_code)]
impl RowDecodeError {
    fn schema_mismatch(error: GatewayError) -> Self {
        Self {
            kind: RowDecodeErrorKind::SchemaMismatch,
            error,
        }
    }

    fn invalid(error: GatewayError) -> Self {
        Self {
            kind: RowDecodeErrorKind::InvalidValue,
            error,
        }
    }

    /// Returns true only for failures that a newer table schema may resolve.
    pub(crate) fn is_schema_mismatch(&self) -> bool {
        self.kind == RowDecodeErrorKind::SchemaMismatch
    }

    /// Returns the client-safe error message.
    pub(crate) fn message(&self) -> &str {
        self.error.message()
    }

    /// Converts this failure into the standard Gateway error envelope input.
    pub(crate) fn into_gateway_error(self) -> GatewayError {
        self.error
    }
}

impl From<GatewayError> for RowDecodeError {
    fn from(error: GatewayError) -> Self {
        Self::invalid(error)
    }
}

struct SchemaDecoderInner {
    row_type: RowType,
    column_indexes: HashMap<String, usize>,
}

/// Immutable reusable decoder compiled from one Fluss row type.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct SchemaDecoder {
    inner: Arc<SchemaDecoderInner>,
}

impl fmt::Debug for SchemaDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SchemaDecoder")
            .field("row_type", &self.inner.row_type)
            .finish()
    }
}

#[allow(dead_code)]
impl SchemaDecoder {
    /// Builds a decoder for a complete table row type.
    pub(crate) fn new(row_type: RowType) -> Result<Self, GatewayError> {
        validate_row_type(&row_type, 0)?;
        if row_type.fields().is_empty() {
            return Err(GatewayError::internal(
                "cannot build a row decoder for an empty table schema",
            ));
        }
        let column_indexes = row_type
            .fields()
            .iter()
            .enumerate()
            .map(|(index, field)| (field.name.clone(), index))
            .collect();
        Ok(Self {
            inner: Arc::new(SchemaDecoderInner {
                row_type,
                column_indexes,
            }),
        })
    }

    /// Returns the exact row type this decoder was built from.
    pub(crate) fn row_type(&self) -> &RowType {
        &self.inner.row_type
    }

    /// Decodes one raw JSON object into complete schema order.
    pub(crate) fn decode_row(
        &self,
        label: &str,
        row_json: &[u8],
        shape: RowShape<'_>,
    ) -> Result<GenericRow<'static>, RowDecodeError> {
        let required = self.required_columns(label, shape)?;
        let seed = RootRowSeed {
            label,
            row_type: &self.inner.row_type,
            column_indexes: &self.inner.column_indexes,
            required: &required,
        };
        let mut deserializer = serde_json::Deserializer::from_slice(row_json);
        let decoded = seed
            .deserialize(&mut deserializer)
            .map_err(|error| invalid_json_error(label, error))?;
        deserializer
            .end()
            .map_err(|error| invalid_json_error(label, error))?;
        decoded
    }

    fn required_columns(
        &self,
        label: &str,
        shape: RowShape<'_>,
    ) -> Result<Vec<bool>, RowDecodeError> {
        match shape {
            RowShape::Complete => Ok(self
                .inner
                .row_type
                .fields()
                .iter()
                .map(|field| !field.data_type.is_nullable())
                .collect()),
            RowShape::Sparse(required_columns) => {
                let mut required = vec![false; self.inner.row_type.fields().len()];
                for column in required_columns {
                    let Some(index) = self.inner.column_indexes.get(column) else {
                        return Err(RowDecodeError::schema_mismatch(
                            GatewayError::invalid_argument(format!(
                                "{label}: required column `{column}` is not part of the table schema"
                            )),
                        ));
                    };
                    required[*index] = true;
                }
                Ok(required)
            }
        }
    }
}

fn invalid_json_error(label: &str, error: serde_json::Error) -> RowDecodeError {
    RowDecodeError::invalid(GatewayError::invalid_argument(format!(
        "{label}: invalid JSON row: {error}"
    )))
}

fn validate_row_type(row_type: &RowType, depth: usize) -> Result<(), GatewayError> {
    if depth > MAX_TYPE_NESTING {
        return Err(GatewayError::internal(format!(
            "row type nesting exceeds {MAX_TYPE_NESTING} levels"
        )));
    }
    let mut names = HashSet::with_capacity(row_type.fields().len());
    for field in row_type.fields() {
        if field.name.is_empty() {
            return Err(GatewayError::internal(
                "row type contains an empty field name",
            ));
        }
        if !names.insert(field.name.as_str()) {
            return Err(GatewayError::internal(format!(
                "row type contains duplicate field `{}`",
                field.name
            )));
        }
        validate_data_type(&field.data_type, depth + 1)?;
    }
    Ok(())
}

fn validate_data_type(data_type: &DataType, depth: usize) -> Result<(), GatewayError> {
    if depth > MAX_TYPE_NESTING {
        return Err(GatewayError::internal(format!(
            "row type nesting exceeds {MAX_TYPE_NESTING} levels"
        )));
    }
    match data_type {
        DataType::Char(data_type) if data_type.length() == 0 => {
            Err(GatewayError::internal("CHAR length must be at least one"))
        }
        DataType::Binary(data_type) if data_type.length() == 0 => {
            Err(GatewayError::internal("BINARY length must be at least one"))
        }
        DataType::Array(data_type) => validate_data_type(data_type.get_element_type(), depth + 1),
        DataType::Map(data_type) => {
            if data_type.key_type().is_nullable() {
                return Err(GatewayError::internal("MAP key type must not be nullable"));
            }
            if is_complex(data_type.key_type()) {
                return Err(GatewayError::invalid_argument(
                    "FIP-49 JSON does not support ARRAY, MAP, or ROW values as MAP keys",
                ));
            }
            validate_data_type(data_type.key_type(), depth + 1)?;
            validate_data_type(data_type.value_type(), depth + 1)
        }
        DataType::Row(row_type) => validate_row_type(row_type, depth + 1),
        _ => Ok(()),
    }
}

fn is_complex(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Array(_) | DataType::Map(_) | DataType::Row(_)
    )
}

#[derive(Clone)]
struct ValuePath<'a> {
    label: &'a str,
    path: String,
}

impl<'a> ValuePath<'a> {
    fn column(label: &'a str, column: &str) -> Self {
        Self {
            label,
            path: column.to_string(),
        }
    }

    fn element(&self, index: usize) -> Self {
        self.nested(format!("{}[{index}]", self.path))
    }

    fn nested_field(&self, name: &str) -> Self {
        self.nested(format!("{}.{name}", self.path))
    }

    fn map_part(&self, index: usize, part: &str) -> Self {
        self.nested(format!("{}[{index}].{part}", self.path))
    }

    fn nested(&self, path: String) -> Self {
        Self {
            label: self.label,
            path,
        }
    }

    fn invalid(&self, reason: impl fmt::Display) -> RowDecodeError {
        RowDecodeError::invalid(GatewayError::invalid_argument(format!(
            "{}: column `{}` {reason}",
            self.label, self.path
        )))
    }

    fn schema_mismatch(&self, reason: impl fmt::Display) -> RowDecodeError {
        RowDecodeError::schema_mismatch(GatewayError::invalid_argument(format!(
            "{}: column `{}` {reason}",
            self.label, self.path
        )))
    }

    fn type_error(&self, expected: &str, actual: &str) -> RowDecodeError {
        self.invalid(format!("expects {expected}, got {actual}"))
    }
}

type DecodeResult<T> = Result<T, RowDecodeError>;

enum ScalarValue<'a> {
    Null,
    Boolean(bool),
    ExactNumber(&'a str),
    String(String),
    Array,
    Object,
}

impl ScalarValue<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean(_) => "a boolean",
            Self::ExactNumber(_) => "a number",
            Self::String(_) => "a string",
            Self::Array => "an array",
            Self::Object => "an object",
        }
    }
}

fn parse_scalar_value(raw: &RawValue) -> Result<ScalarValue<'_>, serde_json::Error> {
    let text = raw.get().trim();
    match text.as_bytes().first().copied() {
        Some(b'n') => Ok(ScalarValue::Null),
        Some(b't') => Ok(ScalarValue::Boolean(true)),
        Some(b'f') => Ok(ScalarValue::Boolean(false)),
        Some(b'"') => serde_json::from_str(text).map(ScalarValue::String),
        Some(b'-' | b'0'..=b'9') => Ok(ScalarValue::ExactNumber(text)),
        Some(b'[') => Ok(ScalarValue::Array),
        Some(b'{') => Ok(ScalarValue::Object),
        _ => unreachable!("RawValue always contains one complete JSON value"),
    }
}

struct RootRowSeed<'a> {
    label: &'a str,
    row_type: &'a RowType,
    column_indexes: &'a HashMap<String, usize>,
    required: &'a [bool],
}

impl<'de> DeserializeSeed<'de> for RootRowSeed<'_> {
    type Value = DecodeResult<GenericRow<'static>>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_any(RootRowVisitor {
            label: self.label,
            row_type: self.row_type,
            column_indexes: self.column_indexes,
            required: self.required,
        })
    }
}

struct RootRowVisitor<'a> {
    label: &'a str,
    row_type: &'a RowType,
    column_indexes: &'a HashMap<String, usize>,
    required: &'a [bool],
}

impl RootRowVisitor<'_> {
    fn type_error(&self, actual: &str) -> DecodeResult<GenericRow<'static>> {
        Err(RowDecodeError::invalid(GatewayError::invalid_argument(
            format!("{}: row must be a JSON object, got {actual}", self.label),
        )))
    }
}

impl<'de> Visitor<'de> for RootRowVisitor<'_> {
    type Value = DecodeResult<GenericRow<'static>>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON row object")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(self.type_error("null"))
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(self.type_error("a boolean"))
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(self.type_error("a number"))
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(self.type_error("a number"))
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(self.type_error("a number"))
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(self.type_error("a string"))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_sequence(&mut sequence)?;
        Ok(self.type_error("an array"))
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        decode_row_map(
            map,
            self.row_type,
            RowContext::Root {
                label: self.label,
                column_indexes: self.column_indexes,
                required: self.required,
            },
        )
    }
}

enum RowContext<'a> {
    Root {
        label: &'a str,
        column_indexes: &'a HashMap<String, usize>,
        required: &'a [bool],
    },
    Nested {
        path: ValuePath<'a>,
    },
}

impl<'a> RowContext<'a> {
    fn field_index(&self, row_type: &RowType, name: &str) -> Option<usize> {
        match self {
            Self::Root { column_indexes, .. } => column_indexes.get(name).copied(),
            Self::Nested { .. } => row_type.get_field_index(name),
        }
    }

    fn field_path(&self, name: &str) -> ValuePath<'a> {
        match self {
            Self::Root { label, .. } => ValuePath::column(label, name),
            Self::Nested { path } => path.nested_field(name),
        }
    }

    fn required(&self, index: usize, data_type: &DataType) -> bool {
        match self {
            Self::Root { required, .. } => required[index],
            Self::Nested { .. } => !data_type.is_nullable(),
        }
    }

    fn duplicate(&self, name: &str) -> RowDecodeError {
        match self {
            Self::Root { label, .. } => RowDecodeError::invalid(GatewayError::invalid_argument(
                format!("{label}: duplicate column `{name}`"),
            )),
            Self::Nested { path } => path.invalid(format!("has a duplicate field `{name}`")),
        }
    }

    fn unknown(&self, name: &str) -> RowDecodeError {
        match self {
            Self::Root { label, .. } => RowDecodeError::schema_mismatch(
                GatewayError::invalid_argument(format!("{label}: unknown column `{name}`")),
            ),
            Self::Nested { path } => path.schema_mismatch(format!("has an unknown field `{name}`")),
        }
    }

    fn missing(&self, name: &str) -> RowDecodeError {
        match self {
            Self::Root { label, .. } => {
                RowDecodeError::schema_mismatch(GatewayError::invalid_argument(format!(
                    "{label}: column `{name}` is required and was not provided"
                )))
            }
            Self::Nested { path } => path
                .nested_field(name)
                .schema_mismatch("is required and was not provided"),
        }
    }

    fn required_null(&self, name: &str) -> RowDecodeError {
        match self {
            Self::Root { label, .. } => RowDecodeError::invalid(GatewayError::invalid_argument(
                format!("{label}: column `{name}` is required and must not be null"),
            )),
            Self::Nested { path } => path.nested_field(name).invalid("must not be null"),
        }
    }
}

fn decode_row_map<'de, A>(
    mut map: A,
    row_type: &RowType,
    context: RowContext<'_>,
) -> Result<DecodeResult<GenericRow<'static>>, A::Error>
where
    A: MapAccess<'de>,
{
    let mut seen = HashSet::with_capacity(map.size_hint().unwrap_or(0));
    let mut duplicate = None;
    let mut unknown = None;
    let mut provided = std::iter::repeat_with(|| None)
        .take(row_type.fields().len())
        .collect::<Vec<Option<DecodeResult<Datum<'static>>>>>();

    while let Some(name) = map.next_key::<String>()? {
        if !seen.insert(name.clone()) {
            map.next_value::<IgnoredAny>()?;
            duplicate.get_or_insert(name);
            continue;
        }
        let Some(index) = context.field_index(row_type, &name) else {
            map.next_value::<IgnoredAny>()?;
            unknown.get_or_insert(name);
            continue;
        };
        let field = &row_type.fields()[index];
        let value = map.next_value_seed(ValueSeed {
            path: context.field_path(&field.name),
            data_type: &field.data_type,
        })?;
        provided[index] = Some(value);
    }

    if let Some(name) = duplicate {
        return Ok(Err(context.duplicate(&name)));
    }
    if let Some(name) = unknown {
        return Ok(Err(context.unknown(&name)));
    }

    let mut row = GenericRow::new(row_type.fields().len());
    for (index, field) in row_type.fields().iter().enumerate() {
        let datum = match provided[index].take() {
            Some(Ok(Datum::Null)) if context.required(index, &field.data_type) => {
                return Ok(Err(context.required_null(&field.name)));
            }
            Some(Ok(datum)) => datum,
            Some(Err(error)) => return Ok(Err(error)),
            None if context.required(index, &field.data_type) => {
                return Ok(Err(context.missing(&field.name)));
            }
            None => Datum::Null,
        };
        row.set_field(index, datum);
    }
    Ok(Ok(row))
}

struct ValueSeed<'a> {
    path: ValuePath<'a>,
    data_type: &'a DataType,
}

impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = DecodeResult<Datum<'static>>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        if is_complex(self.data_type) {
            deserializer.deserialize_any(ComplexValueVisitor {
                path: self.path,
                data_type: self.data_type,
            })
        } else {
            let raw = <&RawValue>::deserialize(deserializer)?;
            let value = parse_scalar_value(raw).map_err(de::Error::custom)?;
            Ok(decode_scalar(&self.path, self.data_type, &value))
        }
    }
}

struct ComplexValueVisitor<'a> {
    path: ValuePath<'a>,
    data_type: &'a DataType,
}

impl ComplexValueVisitor<'_> {
    fn type_error(&self, actual: &str) -> DecodeResult<Datum<'static>> {
        Err(self
            .path
            .type_error(complex_expected(self.data_type), actual))
    }

    fn null(&self) -> DecodeResult<Datum<'static>> {
        if self.data_type.is_nullable() {
            Ok(Datum::Null)
        } else {
            Err(self.path.invalid("must not be null"))
        }
    }
}

impl<'de> Visitor<'de> for ComplexValueVisitor<'_> {
    type Value = DecodeResult<Datum<'static>>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(complex_expected(self.data_type))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(self.null())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(self.type_error("a boolean"))
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(self.type_error("a number"))
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(self.type_error("a number"))
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(self.type_error("a number"))
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(self.type_error("a string"))
    }

    fn visit_seq<A>(self, sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        match self.data_type {
            DataType::Array(data_type) => {
                decode_array_sequence(sequence, &self.path, data_type.get_element_type())
            }
            DataType::Map(data_type) => decode_map_sequence(
                sequence,
                &self.path,
                data_type.key_type(),
                data_type.value_type(),
            ),
            DataType::Row(_) => {
                let mut sequence = sequence;
                drain_sequence(&mut sequence)?;
                Ok(self.type_error("an array"))
            }
            _ => unreachable!("complex visitor receives only ARRAY, MAP, or ROW"),
        }
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        match self.data_type {
            DataType::Row(row_type) => {
                let row = decode_row_map(map, row_type, RowContext::Nested { path: self.path })?;
                Ok(row.map(|row| Datum::Row(Box::new(row))))
            }
            DataType::Array(_) | DataType::Map(_) => {
                let mut map = map;
                drain_map(&mut map)?;
                Ok(self.type_error("an object"))
            }
            _ => unreachable!("complex visitor receives only ARRAY, MAP, or ROW"),
        }
    }
}

fn complex_expected(data_type: &DataType) -> &'static str {
    match data_type {
        DataType::Array(_) => "ARRAY (a JSON array)",
        DataType::Map(_) => "MAP (an array of {key, value} objects)",
        DataType::Row(_) => "ROW (a JSON object of its declared fields)",
        _ => unreachable!("only complex data types have a container expectation"),
    }
}

fn drain_sequence<'de, A>(sequence: &mut A) -> Result<(), A::Error>
where
    A: SeqAccess<'de>,
{
    while sequence.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn drain_map<'de, A>(map: &mut A) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
{
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

fn decode_scalar(
    path: &ValuePath<'_>,
    data_type: &DataType,
    value: &ScalarValue<'_>,
) -> DecodeResult<Datum<'static>> {
    if matches!(value, ScalarValue::Null) {
        return if data_type.is_nullable() {
            Ok(Datum::Null)
        } else {
            Err(path.invalid("must not be null"))
        };
    }
    match data_type {
        DataType::Boolean(_) => match value {
            ScalarValue::Boolean(parsed) => Ok(Datum::Bool(*parsed)),
            _ => Err(path.type_error("BOOLEAN (a JSON boolean)", value.kind())),
        },
        DataType::TinyInt(_) => {
            decode_integer(path, value, "TINYINT", i8::MIN as i64, i8::MAX as i64)
                .map(|parsed| Datum::Int8(parsed as i8))
        }
        DataType::SmallInt(_) => {
            decode_integer(path, value, "SMALLINT", i16::MIN as i64, i16::MAX as i64)
                .map(|parsed| Datum::Int16(parsed as i16))
        }
        DataType::Int(_) => decode_integer(path, value, "INT", i32::MIN as i64, i32::MAX as i64)
            .map(|parsed| Datum::Int32(parsed as i32)),
        DataType::BigInt(_) => decode_bigint(path, value).map(Datum::Int64),
        DataType::Float(_) => decode_float32(path, value),
        DataType::Double(_) => decode_float(path, value, "DOUBLE").map(Datum::from),
        DataType::Char(_) | DataType::String(_) => match value {
            ScalarValue::String(text) => Ok(Datum::String(Cow::Owned(text.clone()))),
            _ => Err(path.type_error("STRING (a JSON string)", value.kind())),
        },
        DataType::Decimal(data_type) => {
            decode_decimal(path, value, data_type.precision(), data_type.scale())
        }
        DataType::Bytes(_) => decode_binary(path, value, None),
        DataType::Binary(data_type) => decode_binary(path, value, Some(data_type.length())),
        DataType::Date(_) => decode_date(path, value),
        DataType::Time(data_type) => decode_time(path, value, data_type.precision()),
        DataType::Timestamp(data_type) => {
            decode_timestamp(path, value, data_type.precision(), false)
        }
        DataType::TimestampLTz(data_type) => {
            decode_timestamp(path, value, data_type.precision(), true)
        }
        DataType::Array(_) | DataType::Map(_) | DataType::Row(_) => {
            unreachable!("container values use the complex visitor")
        }
    }
}
fn decode_integer(
    path: &ValuePath<'_>,
    value: &ScalarValue<'_>,
    type_name: &str,
    min: i64,
    max: i64,
) -> Result<i64, RowDecodeError> {
    let expected = format!("{type_name} (an integer in [{min}, {max}])");
    let ScalarValue::ExactNumber(lexeme) = value else {
        return Err(path.type_error(&expected, value.kind()));
    };
    let parsed = integer_lexeme(lexeme).ok_or_else(|| path.type_error(&expected, value.kind()))?;
    if parsed < min || parsed > max {
        return Err(path.invalid(format!("expects {expected}, value is out of range")));
    }
    Ok(parsed)
}

fn decode_bigint(path: &ValuePath<'_>, value: &ScalarValue<'_>) -> Result<i64, RowDecodeError> {
    let expected = "BIGINT (an exact integer number or a base-10 string)";
    match value {
        ScalarValue::ExactNumber(lexeme) => {
            integer_lexeme(lexeme).ok_or_else(|| path.type_error(expected, value.kind()))
        }
        ScalarValue::String(text) => text
            .parse::<i64>()
            .map_err(|_| path.type_error(expected, value.kind())),
        _ => Err(path.type_error(expected, value.kind())),
    }
}

fn integer_lexeme(lexeme: &str) -> Option<i64> {
    if lexeme.contains(['.', 'e', 'E']) {
        return None;
    }
    lexeme.parse::<i64>().ok()
}

fn decode_float(
    path: &ValuePath<'_>,
    value: &ScalarValue<'_>,
    type_name: &str,
) -> Result<f64, RowDecodeError> {
    let expected = format!("{type_name} (a number or \"NaN\", \"Infinity\", \"-Infinity\")");
    match value {
        ScalarValue::ExactNumber(lexeme) => {
            let parsed = lexeme
                .parse::<f64>()
                .map_err(|_| path.type_error(&expected, value.kind()))?;
            if !parsed.is_finite() {
                return Err(path.invalid(format!(
                    "expects {type_name}, finite number is out of range"
                )));
            }
            Ok(parsed)
        }
        ScalarValue::String(text) => match text.as_str() {
            "NaN" => Ok(f64::NAN),
            "Infinity" => Ok(f64::INFINITY),
            "-Infinity" => Ok(f64::NEG_INFINITY),
            _ => Err(path.type_error(&expected, value.kind())),
        },
        _ => Err(path.type_error(&expected, value.kind())),
    }
}

fn decode_float32(
    path: &ValuePath<'_>,
    value: &ScalarValue<'_>,
) -> Result<Datum<'static>, RowDecodeError> {
    let parsed = decode_float(path, value, "FLOAT")?;
    let narrowed = parsed as f32;
    if narrowed.is_infinite() && parsed.is_finite() {
        return Err(path.invalid("expects FLOAT, value is out of 32-bit float range"));
    }
    Ok(Datum::from(narrowed))
}

fn decode_decimal(
    path: &ValuePath<'_>,
    value: &ScalarValue<'_>,
    precision: u32,
    scale: u32,
) -> Result<Datum<'static>, RowDecodeError> {
    let expected = format!("DECIMAL({precision}, {scale}) (a base-10 string or a number)");
    let text = match value {
        ScalarValue::String(text) => text.as_str(),
        ScalarValue::ExactNumber(lexeme) => lexeme,
        _ => return Err(path.type_error(&expected, value.kind())),
    };
    let unscaled = decimal_to_unscaled(text, precision, scale)
        .map_err(|reason| path.invalid(format!("expects {expected}: {reason}")))?;
    let decimal = Decimal::from_unscaled_bytes(&unscaled.to_be_bytes(), precision, scale)
        .map_err(|error| path.invalid(format!("expects {expected}: {error}")))?;
    Ok(Datum::Decimal(decimal))
}

fn decimal_to_unscaled(text: &str, precision: u32, scale: u32) -> Result<i128, String> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (int_part, frac_part) = match unsigned.split_once('.') {
        Some((int_part, frac_part)) => (int_part, frac_part),
        None => (unsigned, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err("not a decimal number".to_string());
    }
    if !int_part.chars().all(|character| character.is_ascii_digit())
        || !frac_part
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return Err("not a plain base-10 decimal (exponents are not accepted)".to_string());
    }
    let scale = scale as usize;
    let significant_frac = frac_part.trim_end_matches('0');
    if significant_frac.len() > scale {
        return Err(format!(
            "value has {} fractional digits but the scale is {scale}",
            significant_frac.len()
        ));
    }
    let mut digits = String::with_capacity(int_part.len() + scale);
    digits.push_str(int_part);
    digits.push_str(frac_part.get(..scale.min(frac_part.len())).unwrap_or(""));
    for _ in frac_part.len()..scale {
        digits.push('0');
    }
    let unscaled: i128 = digits
        .parse()
        .map_err(|_| "value does not fit a 128-bit decimal".to_string())?;
    let trimmed = digits.trim_start_matches('0');
    let significant = if trimmed.is_empty() { 1 } else { trimmed.len() };
    if significant > precision as usize {
        return Err(format!(
            "value needs {significant} digits of precision but the type allows {precision}"
        ));
    }
    Ok(if negative { -unscaled } else { unscaled })
}

fn decode_binary(
    path: &ValuePath<'_>,
    value: &ScalarValue<'_>,
    fixed_length: Option<usize>,
) -> Result<Datum<'static>, RowDecodeError> {
    let expected = "BINARY (a base64 string)";
    let ScalarValue::String(text) = value else {
        return Err(path.type_error(expected, value.kind()));
    };
    let bytes = BASE64.decode(text).map_err(|error| {
        path.invalid(format!(
            "expects {expected}, the string is not valid base64: {error}"
        ))
    })?;
    if let Some(length) = fixed_length
        && bytes.len() != length
    {
        return Err(path.invalid(format!(
            "expects BINARY({length}), got {} bytes",
            bytes.len()
        )));
    }
    Ok(Datum::Blob(Cow::Owned(bytes)))
}

fn decode_date(
    path: &ValuePath<'_>,
    value: &ScalarValue<'_>,
) -> Result<Datum<'static>, RowDecodeError> {
    let expected = "DATE (an ISO-8601 string like \"2026-01-31\")";
    let ScalarValue::String(text) = value else {
        return Err(path.type_error(expected, value.kind()));
    };
    let days = parse_date(text).ok_or_else(|| path.type_error(expected, value.kind()))?;
    let days = i32::try_from(days)
        .map_err(|_| path.invalid(format!("expects {expected}, the date is out of range")))?;
    Ok(Datum::Date(Date::new(days)))
}

fn decode_time(
    path: &ValuePath<'_>,
    value: &ScalarValue<'_>,
    precision: u32,
) -> Result<Datum<'static>, RowDecodeError> {
    let expected = "TIME (an ISO-8601 string like \"12:34:56.789\")";
    let ScalarValue::String(text) = value else {
        return Err(path.type_error(expected, value.kind()));
    };
    let (seconds_of_day, frac_nanos) =
        parse_time(text).ok_or_else(|| path.type_error(expected, value.kind()))?;
    check_fraction_granularity(path, frac_nanos, precision)?;
    if frac_nanos % NANOS_PER_MILLI != 0 {
        return Err(path.invalid(
            "cannot use sub-millisecond TIME values because the native representation stores milliseconds",
        ));
    }
    let millis_of_day = seconds_of_day * MILLIS_PER_SECOND + frac_nanos / NANOS_PER_MILLI;
    Ok(Datum::Time(Time::new(millis_of_day as i32)))
}

fn decode_timestamp(
    path: &ValuePath<'_>,
    value: &ScalarValue<'_>,
    precision: u32,
    with_zone: bool,
) -> Result<Datum<'static>, RowDecodeError> {
    let expected = if with_zone {
        "TIMESTAMP_LTZ (an ISO-8601 string with a zone or epoch milliseconds)"
    } else {
        "TIMESTAMP (a zone-free ISO-8601 string or epoch milliseconds)"
    };
    let (millis, nanos_of_milli) = match value {
        ScalarValue::ExactNumber(lexeme) => {
            let millis =
                integer_lexeme(lexeme).ok_or_else(|| path.type_error(expected, value.kind()))?;
            let frac_nanos = millis.rem_euclid(MILLIS_PER_SECOND) * NANOS_PER_MILLI;
            check_fraction_granularity(path, frac_nanos, precision)?;
            (millis, 0)
        }
        ScalarValue::String(text) => {
            let parsed =
                parse_timestamp(text).ok_or_else(|| path.type_error(expected, value.kind()))?;
            if with_zone && parsed.offset_seconds.is_none() {
                return Err(path.invalid(format!("expects {expected}, the value has no zone")));
            }
            if !with_zone && parsed.offset_seconds.is_some() {
                return Err(path.invalid(format!("expects {expected}, the value carries a zone")));
            }
            check_fraction_granularity(path, parsed.frac_nanos, precision)?;
            let local_millis = parsed
                .days
                .checked_mul(MILLIS_PER_DAY)
                .and_then(|day_millis| {
                    day_millis.checked_add(
                        parsed.seconds_of_day * MILLIS_PER_SECOND
                            + parsed.frac_nanos / NANOS_PER_MILLI,
                    )
                })
                .ok_or_else(|| {
                    path.invalid(format!("expects {expected}, the value is out of range"))
                })?;
            let offset_millis = i64::from(parsed.offset_seconds.unwrap_or(0)) * MILLIS_PER_SECOND;
            let millis = local_millis.checked_sub(offset_millis).ok_or_else(|| {
                path.invalid(format!("expects {expected}, the value is out of range"))
            })?;
            (millis, (parsed.frac_nanos % NANOS_PER_MILLI) as i32)
        }
        _ => return Err(path.type_error(expected, value.kind())),
    };
    if with_zone {
        TimestampLtz::from_millis_nanos(millis, nanos_of_milli)
            .map(Datum::TimestampLtz)
            .map_err(|error| path.invalid(format!("expects {expected}: {error}")))
    } else {
        TimestampNtz::from_millis_nanos(millis, nanos_of_milli)
            .map(Datum::TimestampNtz)
            .map_err(|error| path.invalid(format!("expects {expected}: {error}")))
    }
}

fn check_fraction_granularity(
    path: &ValuePath<'_>,
    frac_nanos: i64,
    precision: u32,
) -> Result<(), RowDecodeError> {
    let granularity = 10_i64.pow(9_u32.saturating_sub(precision.min(9)));
    if frac_nanos % granularity != 0 {
        return Err(path.invalid(format!(
            "declares precision {precision} but the value carries finer fractional seconds"
        )));
    }
    Ok(())
}

fn decode_array_sequence<'de, A>(
    mut sequence: A,
    path: &ValuePath<'_>,
    element_type: &DataType,
) -> Result<DecodeResult<Datum<'static>>, A::Error>
where
    A: SeqAccess<'de>,
{
    let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
    let mut first_error = None;
    let mut index = 0;
    while let Some(value) = sequence.next_element_seed(ValueSeed {
        path: path.element(index),
        data_type: element_type,
    })? {
        match value {
            Ok(value) if first_error.is_none() => values.push(value),
            Ok(_) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
        index += 1;
    }
    if let Some(error) = first_error {
        return Ok(Err(error));
    }

    let mut writer = FlussArrayWriter::new(values.len(), element_type);
    for (index, datum) in values.into_iter().enumerate() {
        let element_path = path.element(index);
        if let Err(reason) = write_element(&mut writer, index, datum, element_type) {
            return Ok(Err(element_path.invalid(reason)));
        }
    }
    Ok(writer
        .complete()
        .map(Datum::Array)
        .map_err(|error| path.invalid(format!("could not be encoded as an ARRAY: {error}"))))
}

fn decode_map_sequence<'de, A>(
    mut sequence: A,
    path: &ValuePath<'_>,
    key_type: &DataType,
    value_type: &DataType,
) -> Result<DecodeResult<Datum<'static>>, A::Error>
where
    A: SeqAccess<'de>,
{
    let mut entries = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
    let mut first_error = None;
    let mut index = 0;
    while let Some(entry) = sequence.next_element_seed(MapEntrySeed {
        path: path.clone(),
        key_type,
        value_type,
        index,
    })? {
        match entry {
            Ok(entry) if first_error.is_none() => entries.push(entry),
            Ok(_) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
        index += 1;
    }
    if let Some(error) = first_error {
        return Ok(Err(error));
    }

    let mut writer = FlussMapWriter::new(entries.len(), key_type, value_type);
    for (key, value) in entries {
        if let Err(error) = writer.write_entry(key, value) {
            return Ok(Err(
                path.invalid(format!("could not be encoded as a MAP: {error}"))
            ));
        }
    }
    Ok(writer
        .complete()
        .map(Datum::Map)
        .map_err(|error| path.invalid(format!("could not be encoded as a MAP: {error}"))))
}

struct MapEntrySeed<'a> {
    path: ValuePath<'a>,
    key_type: &'a DataType,
    value_type: &'a DataType,
    index: usize,
}

impl<'de> DeserializeSeed<'de> for MapEntrySeed<'_> {
    type Value = DecodeResult<(Datum<'static>, Datum<'static>)>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_any(MapEntryVisitor {
            path: self.path,
            key_type: self.key_type,
            value_type: self.value_type,
            index: self.index,
        })
    }
}

struct MapEntryVisitor<'a> {
    path: ValuePath<'a>,
    key_type: &'a DataType,
    value_type: &'a DataType,
    index: usize,
}

impl MapEntryVisitor<'_> {
    fn entry_path(&self) -> ValuePath<'_> {
        self.path.element(self.index)
    }

    fn type_error(&self, actual: &str) -> DecodeResult<(Datum<'static>, Datum<'static>)> {
        Err(self
            .entry_path()
            .type_error("a map entry object with `key` and `value`", actual))
    }
}

impl<'de> Visitor<'de> for MapEntryVisitor<'_> {
    type Value = DecodeResult<(Datum<'static>, Datum<'static>)>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a map entry object with `key` and `value`")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(self.type_error("null"))
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(self.type_error("a boolean"))
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(self.type_error("a number"))
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(self.type_error("a number"))
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(self.type_error("a number"))
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(self.type_error("a string"))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        drain_sequence(&mut sequence)?;
        Ok(self.type_error("an array"))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen = HashSet::with_capacity(map.size_hint().unwrap_or(0));
        let mut duplicate = None;
        let mut unknown = None;
        let mut key = None;
        let mut value = None;
        while let Some(name) = map.next_key::<String>()? {
            if !seen.insert(name.clone()) {
                map.next_value::<IgnoredAny>()?;
                duplicate.get_or_insert(name);
                continue;
            }
            match name.as_str() {
                "key" => {
                    key = Some(map.next_value_seed(ValueSeed {
                        path: self.path.map_part(self.index, "key"),
                        data_type: self.key_type,
                    })?);
                }
                "value" => {
                    value = Some(map.next_value_seed(ValueSeed {
                        path: self.path.map_part(self.index, "value"),
                        data_type: self.value_type,
                    })?);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                    unknown.get_or_insert(name);
                }
            }
        }

        if let Some(name) = duplicate {
            return Ok(Err(self
                .entry_path()
                .invalid(format!("has a duplicate field `{name}`"))));
        }
        if let Some(name) = unknown {
            return Ok(Err(self
                .entry_path()
                .invalid(format!("has an unknown field `{name}`"))));
        }
        let key = match key {
            Some(Ok(key)) => key,
            Some(Err(error)) => return Ok(Err(error)),
            None => {
                return Ok(Err(self
                    .path
                    .map_part(self.index, "key")
                    .invalid("is required in a map entry")));
            }
        };
        let value = match value {
            Some(Ok(value)) => value,
            Some(Err(error)) => return Ok(Err(error)),
            None => {
                return Ok(Err(self
                    .path
                    .map_part(self.index, "value")
                    .invalid("is required in a map entry")));
            }
        };
        Ok(Ok((key, value)))
    }
}

fn write_element(
    writer: &mut FlussArrayWriter,
    index: usize,
    datum: Datum<'static>,
    element_type: &DataType,
) -> Result<(), String> {
    match datum {
        Datum::Null => writer.set_null_at(index),
        Datum::Bool(value) => writer.write_boolean(index, value),
        Datum::Int8(value) => writer.write_byte(index, value),
        Datum::Int16(value) => writer.write_short(index, value),
        Datum::Int32(value) => writer.write_int(index, value),
        Datum::Int64(value) => writer.write_long(index, value),
        Datum::Float32(value) => writer.write_float(index, value.into_inner()),
        Datum::Float64(value) => writer.write_double(index, value.into_inner()),
        Datum::String(value) => writer.write_string(index, &value),
        Datum::Blob(value) => writer.write_binary_bytes(index, value.as_ref()),
        Datum::Decimal(value) => match element_type {
            DataType::Decimal(data_type) => {
                writer.write_decimal(index, &value, data_type.precision());
            }
            _ => return Err("is a DECIMAL that does not match its declared element type".into()),
        },
        Datum::Date(value) => writer.write_date(index, value),
        Datum::Time(value) => writer.write_time(index, value),
        Datum::TimestampNtz(value) => match element_type {
            DataType::Timestamp(data_type) => {
                writer.write_timestamp_ntz(index, &value, data_type.precision());
            }
            _ => return Err("is a TIMESTAMP that does not match its declared element type".into()),
        },
        Datum::TimestampLtz(value) => match element_type {
            DataType::TimestampLTz(data_type) => {
                writer.write_timestamp_ltz(index, &value, data_type.precision());
            }
            _ => {
                return Err(
                    "is a TIMESTAMP_LTZ that does not match its declared element type".into(),
                );
            }
        },
        Datum::Array(value) => writer.write_array(index, &value),
        Datum::Map(value) => writer.write_map(index, &value),
        Datum::Row(value) => writer
            .write_row(index, value.as_ref())
            .map_err(|error| format!("could not be encoded as a nested ROW: {error}"))?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use fluss::metadata::{
        ArrayType, BigIntType, BinaryType, BooleanType, BytesType, CharType, DataField, DateType,
        DecimalType, DoubleType, FloatType, IntType, MapType, SmallIntType, StringType, TimeType,
        TimestampLTzType, TimestampType, TinyIntType,
    };
    use serde::Deserialize;
    use serde_json::value::RawValue;

    #[derive(Deserialize)]
    struct BorrowedRow<'a> {
        #[serde(borrow)]
        row: &'a RawValue,
    }

    fn field(name: &str, data_type: DataType) -> DataField {
        DataField::new(name, data_type, None)
    }

    fn row_type(fields: Vec<DataField>) -> RowType {
        RowType::new(fields)
    }

    fn decode_one(data_type: DataType, json: &str) -> Result<Datum<'static>, RowDecodeError> {
        let decoder = SchemaDecoder::new(row_type(vec![field("v", data_type)])).unwrap();
        let row = decoder.decode_row(
            "entry `e1`",
            format!("{{\"v\":{json}}}").as_bytes(),
            RowShape::Complete,
        )?;
        Ok(row.values[0].clone())
    }

    #[test]
    fn borrowed_raw_value_reaches_decoder_without_losing_precision() {
        let body: BorrowedRow<'_> =
            serde_json::from_slice(br#"{"row":{"v":9007199254740993}}"#).unwrap();
        let decoder = SchemaDecoder::new(row_type(vec![field(
            "v",
            DataType::BigInt(BigIntType::with_nullable(false)),
        )]))
        .unwrap();

        let row = decoder
            .decode_row("entry `raw`", body.row.get().as_bytes(), RowShape::Complete)
            .unwrap();

        assert_eq!(row.values, vec![Datum::Int64(9_007_199_254_740_993)]);
    }

    #[test]
    fn streams_deep_large_values_without_recursive_rescanning_budget() {
        let mut data_type = DataType::String(StringType::new());
        let mut value = format!("\"{}\"", "x".repeat(256 * 1024));
        for _ in 0..16 {
            data_type = DataType::Array(ArrayType::new(data_type));
            value = format!("[{value}]");
        }

        assert!(decode_one(data_type, &value).is_ok());
    }

    #[test]
    fn complete_and_sparse_rows_follow_schema_order() {
        let decoder = SchemaDecoder::new(row_type(vec![
            field("id", DataType::Int(IntType::with_nullable(false))),
            field("name", DataType::String(StringType::new())),
            field("amount", DataType::BigInt(BigIntType::with_nullable(false))),
        ]))
        .unwrap();
        let complete = decoder
            .decode_row("entry `c`", br#"{"amount":9,"id":1}"#, RowShape::Complete)
            .unwrap();
        assert_eq!(
            complete.values,
            vec![Datum::Int32(1), Datum::Null, Datum::Int64(9)]
        );

        let required = vec!["id".to_string()];
        let sparse = decoder
            .decode_row("entry `s`", br#"{"id":2}"#, RowShape::Sparse(&required))
            .unwrap();
        assert_eq!(
            sparse.values,
            vec![Datum::Int32(2), Datum::Null, Datum::Null]
        );
    }

    #[test]
    fn sparse_rows_require_named_columns_even_when_the_schema_allows_null() {
        let decoder = SchemaDecoder::new(row_type(vec![
            field("key", DataType::String(StringType::new())),
            field("version", DataType::Int(IntType::with_nullable(false))),
            field("note", DataType::String(StringType::new())),
        ]))
        .unwrap();
        let required = vec!["key".to_string()];

        let missing = decoder
            .decode_row("entry `missing`", br#"{}"#, RowShape::Sparse(&required))
            .unwrap_err();
        assert!(missing.is_schema_mismatch());

        let explicit_null = decoder
            .decode_row(
                "entry `null`",
                br#"{"key":null}"#,
                RowShape::Sparse(&required),
            )
            .unwrap_err();
        assert!(!explicit_null.is_schema_mismatch());

        let decoded = decoder
            .decode_row(
                "entry `present`",
                br#"{"note":"kept","key":"k"}"#,
                RowShape::Sparse(&required),
            )
            .unwrap();
        assert_eq!(
            decoded.values,
            vec![
                Datum::String(Cow::Borrowed("k")),
                Datum::Null,
                Datum::String(Cow::Borrowed("kept")),
            ]
        );
    }

    #[test]
    fn classifies_shape_and_value_failures() {
        let decoder = SchemaDecoder::new(row_type(vec![field(
            "id",
            DataType::Int(IntType::with_nullable(false)),
        )]))
        .unwrap();

        let unknown = decoder
            .decode_row("entry", br#"{"new":1}"#, RowShape::Complete)
            .unwrap_err();
        assert!(unknown.is_schema_mismatch());

        let missing = decoder
            .decode_row("entry", br#"{}"#, RowShape::Complete)
            .unwrap_err();
        assert!(missing.is_schema_mismatch());

        let wrong_type = decoder
            .decode_row("entry", br#"{"id":"1"}"#, RowShape::Complete)
            .unwrap_err();
        assert!(!wrong_type.is_schema_mismatch());

        let out_of_range = decoder
            .decode_row("entry", br#"{"id":2147483648}"#, RowShape::Complete)
            .unwrap_err();
        assert!(!out_of_range.is_schema_mismatch());

        let nullability = decoder
            .decode_row("entry", br#"{"id":null}"#, RowShape::Complete)
            .unwrap_err();
        assert!(!nullability.is_schema_mismatch());

        let format = decode_one(DataType::Bytes(BytesType::new()), "\"not-base64\"").unwrap_err();
        assert!(!format.is_schema_mismatch());

        let duplicate_unknown = decoder
            .decode_row("entry", br#"{"new":1,"new":2}"#, RowShape::Complete)
            .unwrap_err();
        assert!(!duplicate_unknown.is_schema_mismatch());
        assert!(duplicate_unknown.message().contains("duplicate"));

        let duplicate_known = decoder
            .decode_row("entry", br#"{"id":"bad","id":1}"#, RowShape::Complete)
            .unwrap_err();
        assert!(!duplicate_known.is_schema_mismatch());
        assert!(duplicate_known.message().contains("duplicate"));

        let unknown_after_invalid_value = decoder
            .decode_row(
                "entry",
                br#"{"id":"bad","added_column":1}"#,
                RowShape::Complete,
            )
            .unwrap_err();
        assert!(unknown_after_invalid_value.is_schema_mismatch());
        assert!(
            unknown_after_invalid_value
                .message()
                .contains("added_column")
        );

        let message = wrong_type.message().to_string();
        let invalid = wrong_type.into_gateway_error();
        assert_eq!(invalid.kind(), ErrorKind::InvalidArgument);
        assert_eq!(invalid.message(), message);
    }

    #[test]
    fn decodes_integer_boundaries_without_coercion() {
        for (data_type, json, expected) in [
            (
                DataType::TinyInt(TinyIntType::new()),
                "-128",
                Datum::Int8(i8::MIN),
            ),
            (
                DataType::TinyInt(TinyIntType::new()),
                "127",
                Datum::Int8(i8::MAX),
            ),
            (
                DataType::SmallInt(SmallIntType::new()),
                "-32768",
                Datum::Int16(i16::MIN),
            ),
            (
                DataType::SmallInt(SmallIntType::new()),
                "32767",
                Datum::Int16(i16::MAX),
            ),
            (
                DataType::Int(IntType::new()),
                "-2147483648",
                Datum::Int32(i32::MIN),
            ),
            (
                DataType::Int(IntType::new()),
                "2147483647",
                Datum::Int32(i32::MAX),
            ),
        ] {
            assert_eq!(decode_one(data_type, json).unwrap(), expected);
        }
        for (data_type, json) in [
            (DataType::TinyInt(TinyIntType::new()), "128"),
            (DataType::SmallInt(SmallIntType::new()), "-32769"),
            (DataType::Int(IntType::new()), "1.0"),
            (DataType::Int(IntType::new()), "1e0"),
        ] {
            assert!(decode_one(data_type, json).is_err(), "{json}");
        }
    }

    #[test]
    fn decodes_exact_numeric_decimal_and_non_finite_values() {
        assert_eq!(
            decode_one(DataType::BigInt(BigIntType::new()), "9007199254740993").unwrap(),
            Datum::Int64(9_007_199_254_740_993)
        );
        assert_eq!(
            decode_one(
                DataType::BigInt(BigIntType::new()),
                "\"-9223372036854775808\""
            )
            .unwrap(),
            Datum::Int64(i64::MIN)
        );
        for json in ["9223372036854775808", "\"1.0\"", "1e0"] {
            assert!(
                decode_one(DataType::BigInt(BigIntType::new()), json).is_err(),
                "{json}"
            );
        }

        let decimal_type = DataType::Decimal(DecimalType::new(38, 18).unwrap());
        let Datum::Decimal(decimal) =
            decode_one(decimal_type.clone(), "9007199254740993.000000000000000001").unwrap()
        else {
            panic!("expected decimal");
        };
        assert_eq!(
            decimal.to_big_decimal().to_string(),
            "9007199254740993.000000000000000001"
        );
        let Datum::Decimal(decimal) = decode_one(
            DataType::Decimal(DecimalType::new(5, 2).unwrap()),
            "\"12.3400\"",
        )
        .unwrap() else {
            panic!("expected decimal");
        };
        assert_eq!(decimal.to_big_decimal().to_string(), "12.34");
        for json in ["\"12.345\"", "\"1e2\"", "\"1000.00\""] {
            assert!(
                decode_one(DataType::Decimal(DecimalType::new(5, 2).unwrap()), json).is_err(),
                "{json}"
            );
        }
        assert!(
            decode_one(DataType::Double(DoubleType::new()), "1e400").is_err(),
            "a finite JSON number must not silently become infinity"
        );
        let Datum::Float64(value) =
            decode_one(DataType::Double(DoubleType::new()), "\"Infinity\"").unwrap()
        else {
            panic!("expected double");
        };
        assert!(value.into_inner().is_infinite());
        assert!(
            decode_one(DataType::Float(FloatType::new()), "3.5e38").is_err(),
            "a finite number that overflows f32 must be rejected"
        );
        let Datum::Float32(value) =
            decode_one(DataType::Float(FloatType::new()), "\"NaN\"").unwrap()
        else {
            panic!("expected float");
        };
        assert!(value.into_inner().is_nan());
    }

    #[test]
    fn decodes_strings_and_base64_binary() {
        assert_eq!(
            decode_one(DataType::Char(CharType::new(2)), "\"longer\"").unwrap(),
            Datum::String(Cow::Owned("longer".to_string()))
        );
        assert_eq!(
            decode_one(DataType::String(StringType::new()), "\"a\\n雪\"").unwrap(),
            Datum::String(Cow::Owned("a\n雪".to_string()))
        );
        assert_eq!(
            decode_one(DataType::Bytes(BytesType::new()), "\"AP8=\"").unwrap(),
            Datum::Blob(Cow::Owned(vec![0, 255]))
        );
        assert_eq!(
            decode_one(DataType::Binary(BinaryType::new(2)), "\"AP8=\"").unwrap(),
            Datum::Blob(Cow::Owned(vec![0, 255]))
        );
        for (data_type, json) in [
            (DataType::Bytes(BytesType::new()), "\"not-base64\""),
            (DataType::Binary(BinaryType::new(3)), "\"AP8=\""),
        ] {
            assert!(decode_one(data_type, json).is_err(), "{json}");
        }
    }

    #[test]
    fn decodes_dates_and_rejects_invalid_calendar_values() {
        assert_eq!(
            decode_one(DataType::Date(DateType::new()), "\"1970-01-01\"").unwrap(),
            Datum::Date(Date::new(0))
        );
        assert_eq!(
            decode_one(DataType::Date(DateType::new()), "\"-0001-12-31\"").unwrap(),
            Datum::Date(Date::new(-719_529))
        );
        for json in ["\"2026-02-29\"", "\"2026-13-01\"", "\"26-01-01\""] {
            assert!(
                decode_one(DataType::Date(DateType::new()), json).is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn decodes_temporal_values_without_losing_supported_precision() {
        assert_eq!(
            decode_one(
                DataType::Time(TimeType::new(3).unwrap()),
                "\"12:34:56.789\""
            )
            .unwrap(),
            Datum::Time(Time::new(45_296_789))
        );
        assert!(
            decode_one(
                DataType::Time(TimeType::new(6).unwrap()),
                "\"12:34:56.789123\""
            )
            .is_err()
        );
        assert_eq!(
            decode_one(
                DataType::Timestamp(TimestampType::new(6).unwrap()),
                "\"1969-12-31T23:59:59.999999\""
            )
            .unwrap(),
            Datum::TimestampNtz(TimestampNtz::from_millis_nanos(-1, 999_000).unwrap())
        );
        assert_eq!(
            decode_one(
                DataType::TimestampLTz(TimestampLTzType::new(3).unwrap()),
                "\"2026-01-31T14:34:56.789+02:00\""
            )
            .unwrap(),
            Datum::TimestampLtz(TimestampLtz::new(1_769_862_896_789))
        );
        assert!(
            decode_one(
                DataType::Timestamp(TimestampType::new(3).unwrap()),
                "\"2026-01-31T12:34:56.789Z\""
            )
            .is_err()
        );
        assert!(
            decode_one(
                DataType::TimestampLTz(TimestampLTzType::new(3).unwrap()),
                "\"2026-01-31T12:34:56.789\""
            )
            .is_err()
        );
        assert_eq!(
            decode_one(DataType::Timestamp(TimestampType::new(3).unwrap()), "-1").unwrap(),
            Datum::TimestampNtz(TimestampNtz::new(-1))
        );
        assert!(decode_one(DataType::Timestamp(TimestampType::new(0).unwrap()), "1").is_err());
        assert!(
            decode_one(
                DataType::Timestamp(TimestampType::new(9).unwrap()),
                "\"雪\""
            )
            .is_err()
        );
    }

    #[test]
    fn decodes_nested_arrays_maps_and_rows() {
        let nested_row = RowType::new(vec![
            field("id", DataType::BigInt(BigIntType::with_nullable(false))),
            field("name", DataType::String(StringType::new())),
        ]);
        let map = DataType::Map(MapType::new(
            DataType::String(StringType::new()),
            DataType::Row(nested_row),
        ));
        let array = DataType::Array(ArrayType::new(map));
        let datum = decode_one(
            array,
            r#"[ [{"key":"a","value":{"id":"9007199254740993"}}] ]"#,
        )
        .unwrap();
        let Datum::Array(array) = datum else {
            panic!("expected array");
        };
        assert_eq!(array.size(), 1);
    }

    #[test]
    fn rejects_malformed_containers_and_enforces_recursive_nullability() {
        let non_null_int = DataType::Int(IntType::with_nullable(false));
        let array = DataType::Array(ArrayType::new(non_null_int.clone()));
        let error = decode_one(array, "[1,null]").unwrap_err();
        assert!(error.message().contains("v[1]"));
        assert!(!error.is_schema_mismatch());

        let map = DataType::Map(MapType::new(
            DataType::String(StringType::new()),
            non_null_int,
        ));
        for json in [
            r#"[{"key":"a"}]"#,
            r#"[{"key":"a","value":1,"extra":2}]"#,
            r#"[{"key":"a","key":"b","value":1}]"#,
            r#"[{"key":null,"value":1}]"#,
            r#"[1]"#,
        ] {
            let error = decode_one(map.clone(), json).unwrap_err();
            assert!(!error.is_schema_mismatch(), "{json}");
            assert!(error.message().contains("v[0]"), "{json}");
        }
    }

    #[test]
    fn nested_row_shape_errors_are_schema_mismatches() {
        let nested = DataType::Row(RowType::new(vec![field(
            "id",
            DataType::Int(IntType::with_nullable(false)),
        )]));
        let missing = decode_one(nested.clone(), "{}").unwrap_err();
        assert!(missing.is_schema_mismatch());
        let unknown = decode_one(nested, r#"{"new":1}"#).unwrap_err();
        assert!(unknown.is_schema_mismatch());
    }

    #[test]
    fn reports_nested_paths_and_rejects_duplicate_nested_fields() {
        let nested = DataType::Row(RowType::new(vec![field(
            "items",
            DataType::Array(ArrayType::new(DataType::Int(IntType::new()))),
        )]));
        let error = decode_one(nested.clone(), r#"{"items":[1,"bad"]}"#).unwrap_err();
        assert!(error.message().contains("v.items[1]"));
        let duplicate = decode_one(nested, r#"{"items":[1],"items":[2]}"#).unwrap_err();
        assert!(!duplicate.is_schema_mismatch());
        assert!(duplicate.message().contains("duplicate"));
    }

    #[test]
    fn rejects_non_object_rows_malformed_json_and_unknown_sparse_columns() {
        let decoder = SchemaDecoder::new(row_type(vec![field(
            "id",
            DataType::Int(IntType::with_nullable(false)),
        )]))
        .unwrap();
        for json in [b"[]".as_slice(), b"{".as_slice(), b"{\"id\":1} trailing"] {
            let error = decoder
                .decode_row("entry `bad`", json, RowShape::Complete)
                .unwrap_err();
            assert!(!error.is_schema_mismatch());
            assert!(error.message().contains("entry `bad`"));
        }
        let malformed_after_semantic_error = decoder
            .decode_row(
                "entry `bad`",
                br#"{"unknown":1,"id":"bad","later":[}"#,
                RowShape::Complete,
            )
            .unwrap_err();
        assert!(!malformed_after_semantic_error.is_schema_mismatch());
        assert!(
            malformed_after_semantic_error
                .message()
                .contains("invalid JSON row")
        );

        let required = vec!["renamed_id".to_string()];
        let error = decoder
            .decode_row("entry", br#"{"id":1}"#, RowShape::Sparse(&required))
            .unwrap_err();
        assert!(error.is_schema_mismatch());
    }

    #[test]
    fn validates_schema_invariants_and_rest_map_key_support() {
        assert!(
            SchemaDecoder::new(RowType::new(Vec::new()))
                .unwrap_err()
                .message()
                .contains("empty")
        );
        let duplicate = row_type(vec![
            field("id", DataType::Int(IntType::new())),
            field("id", DataType::String(StringType::new())),
        ]);
        assert!(
            SchemaDecoder::new(duplicate)
                .unwrap_err()
                .message()
                .contains("duplicate")
        );
        let complex_key = DataType::Map(MapType::new(
            DataType::Array(ArrayType::new(DataType::Int(IntType::new()))),
            DataType::String(StringType::new()),
        ));
        let error = SchemaDecoder::new(row_type(vec![field("v", complex_key)])).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);

        let mut too_deep = DataType::Int(IntType::new());
        for _ in 0..=MAX_TYPE_NESTING {
            too_deep = DataType::Array(ArrayType::new(too_deep));
        }
        assert!(SchemaDecoder::new(row_type(vec![field("v", too_deep)])).is_err());
    }

    #[test]
    fn decoder_is_send_sync_and_clone() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SchemaDecoder>();
        let decoder = SchemaDecoder::new(row_type(vec![field(
            "flag",
            DataType::Boolean(BooleanType::new()),
        )]))
        .unwrap();
        assert_eq!(decoder.clone().row_type(), decoder.row_type());
    }
}
