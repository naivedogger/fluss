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

//! Deterministic FIP-49 JSON rendering for Arrow values returned by backend operations.

use crate::error::GatewayError;
use crate::protocol::rest::codec::temporal::{format_date, format_fraction, format_time};
use arrow::array::{
    Array, BinaryArray, BooleanArray, Date32Array, Decimal128Array, FixedSizeBinaryArray,
    Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryArray,
    LargeStringArray, ListArray, MapArray, RecordBatch, StringArray, StructArray,
    Time32MillisecondArray, Time32SecondArray, Time64MicrosecondArray, Time64NanosecondArray,
    TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    TimestampSecondArray,
};
use arrow::datatypes::{DataType as ArrowDataType, TimeUnit};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map as JsonMap, Number, Value as JsonValue};

const NANOS_PER_SECOND: i64 = 1_000_000_000;
const NANOS_PER_MILLI: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86_400;

/// Renders every row of a record batch as a JSON object keyed by column name.
#[allow(dead_code)]
pub(crate) fn record_batch_to_json_rows(
    batch: &RecordBatch,
) -> Result<Vec<JsonMap<String, JsonValue>>, GatewayError> {
    let schema = batch.schema();
    let mut rows = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let mut object = JsonMap::with_capacity(batch.num_columns());
        for (column, field) in batch.columns().iter().zip(schema.fields()) {
            insert_json_field(
                &mut object,
                field.name(),
                value_to_json(column.as_ref(), row)?,
                "record batch",
            )?;
        }
        rows.push(object);
    }
    Ok(rows)
}

/// Renders one Arrow array element using the FIP-49 representation.
#[allow(dead_code)]
pub(crate) fn value_to_json(array: &dyn Array, index: usize) -> Result<JsonValue, GatewayError> {
    if index >= array.len() {
        return Err(GatewayError::internal(format!(
            "Arrow index {index} is out of bounds for an array of length {}",
            array.len()
        )));
    }
    if array.is_null(index) {
        return Ok(JsonValue::Null);
    }
    match array.data_type() {
        ArrowDataType::Boolean => Ok(downcast::<BooleanArray>(array)?.value(index).into()),
        ArrowDataType::Int8 => Ok(downcast::<Int8Array>(array)?.value(index).into()),
        ArrowDataType::Int16 => Ok(downcast::<Int16Array>(array)?.value(index).into()),
        ArrowDataType::Int32 => Ok(downcast::<Int32Array>(array)?.value(index).into()),
        ArrowDataType::Int64 => Ok(downcast::<Int64Array>(array)?
            .value(index)
            .to_string()
            .into()),
        ArrowDataType::Float32 => Ok(float32_to_json(
            downcast::<Float32Array>(array)?.value(index),
        )),
        ArrowDataType::Float64 => Ok(float_to_json(downcast::<Float64Array>(array)?.value(index))),
        ArrowDataType::Utf8 => Ok(downcast::<StringArray>(array)?.value(index).into()),
        ArrowDataType::LargeUtf8 => Ok(downcast::<LargeStringArray>(array)?.value(index).into()),
        ArrowDataType::Decimal128(_, _) => Ok(downcast::<Decimal128Array>(array)?
            .value_as_string(index)
            .into()),
        ArrowDataType::Binary => Ok(BASE64
            .encode(downcast::<BinaryArray>(array)?.value(index))
            .into()),
        ArrowDataType::LargeBinary => Ok(BASE64
            .encode(downcast::<LargeBinaryArray>(array)?.value(index))
            .into()),
        ArrowDataType::FixedSizeBinary(_) => Ok(BASE64
            .encode(downcast::<FixedSizeBinaryArray>(array)?.value(index))
            .into()),
        ArrowDataType::Date32 => {
            Ok(format_date(downcast::<Date32Array>(array)?.value(index) as i64).into())
        }
        ArrowDataType::Time32(_) | ArrowDataType::Time64(_) => time_to_json(array, index),
        ArrowDataType::Timestamp(_, _) => timestamp_to_json(array, index),
        ArrowDataType::List(_) => list_to_json(array, index),
        ArrowDataType::Struct(_) => struct_to_json(array, index),
        ArrowDataType::Map(_, _) => map_to_json(array, index),
        other => Err(GatewayError::internal(format!(
            "cannot render Arrow type {other} as FIP-49 JSON"
        ))),
    }
}

fn float_to_json(value: f64) -> JsonValue {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    Number::from_f64(value)
        .map(JsonValue::Number)
        .expect("finite f64 values always have a JSON number representation")
}

fn float32_to_json(value: f32) -> JsonValue {
    if value.is_finite() {
        let shortest = value
            .to_string()
            .parse::<f64>()
            .expect("a finite f32 display is valid JSON number text");
        return JsonValue::Number(
            Number::from_f64(shortest).expect("a finite f32 converts to a finite f64"),
        );
    }
    float_to_json(value as f64)
}

fn time_to_json(array: &dyn Array, index: usize) -> Result<JsonValue, GatewayError> {
    let (nanos_of_day, digits) = match array.data_type() {
        ArrowDataType::Time32(TimeUnit::Second) => (
            downcast::<Time32SecondArray>(array)?.value(index) as i64 * NANOS_PER_SECOND,
            0,
        ),
        ArrowDataType::Time32(TimeUnit::Millisecond) => (
            downcast::<Time32MillisecondArray>(array)?.value(index) as i64 * NANOS_PER_MILLI,
            3,
        ),
        ArrowDataType::Time64(TimeUnit::Microsecond) => (
            downcast::<Time64MicrosecondArray>(array)?.value(index) * 1_000,
            6,
        ),
        ArrowDataType::Time64(TimeUnit::Nanosecond) => {
            (downcast::<Time64NanosecondArray>(array)?.value(index), 9)
        }
        other => {
            return Err(GatewayError::internal(format!(
                "unsupported Arrow time type {other}"
            )));
        }
    };
    if !(0..SECONDS_PER_DAY * NANOS_PER_SECOND).contains(&nanos_of_day) {
        return Err(GatewayError::internal(format!(
            "Arrow TIME value {nanos_of_day}ns is outside one day"
        )));
    }
    let seconds = nanos_of_day.div_euclid(NANOS_PER_SECOND);
    let frac_nanos = nanos_of_day.rem_euclid(NANOS_PER_SECOND);
    Ok(format!(
        "{}{}",
        format_time(seconds),
        format_fraction(frac_nanos, digits)
    )
    .into())
}

fn timestamp_to_json(array: &dyn Array, index: usize) -> Result<JsonValue, GatewayError> {
    let ArrowDataType::Timestamp(unit, zone) = array.data_type() else {
        return Err(GatewayError::internal("expected an Arrow timestamp type"));
    };
    let (value, digits) = match unit {
        TimeUnit::Second => (downcast::<TimestampSecondArray>(array)?.value(index), 0),
        TimeUnit::Millisecond => (
            downcast::<TimestampMillisecondArray>(array)?.value(index),
            3,
        ),
        TimeUnit::Microsecond => (
            downcast::<TimestampMicrosecondArray>(array)?.value(index),
            6,
        ),
        TimeUnit::Nanosecond => (downcast::<TimestampNanosecondArray>(array)?.value(index), 9),
    };
    let (units_per_second, nanos_per_unit) = match unit {
        TimeUnit::Second => (1, NANOS_PER_SECOND),
        TimeUnit::Millisecond => (1_000, NANOS_PER_MILLI),
        TimeUnit::Microsecond => (1_000_000, 1_000),
        TimeUnit::Nanosecond => (NANOS_PER_SECOND, 1),
    };
    let total_seconds = value.div_euclid(units_per_second);
    let frac_nanos = value
        .rem_euclid(units_per_second)
        .checked_mul(nanos_per_unit)
        .ok_or_else(|| GatewayError::internal("timestamp fraction is out of range"))?;
    let days = total_seconds.div_euclid(SECONDS_PER_DAY);
    let seconds_of_day = total_seconds.rem_euclid(SECONDS_PER_DAY);
    let suffix = if zone.is_some() { "Z" } else { "" };
    Ok(format!(
        "{}T{}{}{}",
        format_date(days),
        format_time(seconds_of_day),
        format_fraction(frac_nanos, digits),
        suffix
    )
    .into())
}

fn list_to_json(array: &dyn Array, index: usize) -> Result<JsonValue, GatewayError> {
    let list = downcast::<ListArray>(array)?;
    let element = list.value(index);
    let mut values = Vec::with_capacity(element.len());
    for position in 0..element.len() {
        values.push(value_to_json(element.as_ref(), position)?);
    }
    Ok(JsonValue::Array(values))
}

fn struct_to_json(array: &dyn Array, index: usize) -> Result<JsonValue, GatewayError> {
    let row = downcast::<StructArray>(array)?;
    let mut object = JsonMap::with_capacity(row.num_columns());
    for (column, field) in row.columns().iter().zip(row.fields()) {
        insert_json_field(
            &mut object,
            field.name(),
            value_to_json(column.as_ref(), index)?,
            "struct",
        )?;
    }
    Ok(JsonValue::Object(object))
}

fn map_to_json(array: &dyn Array, index: usize) -> Result<JsonValue, GatewayError> {
    let map = downcast::<MapArray>(array)?;
    let entries = map.value(index);
    if entries.num_columns() != 2 {
        return Err(GatewayError::internal(format!(
            "Arrow MAP entries must have two columns, got {}",
            entries.num_columns()
        )));
    }
    let keys = entries.column(0);
    let values = entries.column(1);
    let mut rendered = Vec::with_capacity(entries.len());
    for position in 0..entries.len() {
        let mut entry = JsonMap::with_capacity(2);
        entry.insert("key".to_string(), value_to_json(keys.as_ref(), position)?);
        entry.insert(
            "value".to_string(),
            value_to_json(values.as_ref(), position)?,
        );
        rendered.push(JsonValue::Object(entry));
    }
    Ok(JsonValue::Array(rendered))
}

fn insert_json_field(
    object: &mut JsonMap<String, JsonValue>,
    name: &str,
    value: JsonValue,
    context: &str,
) -> Result<(), GatewayError> {
    if object.insert(name.to_string(), value).is_some() {
        return Err(GatewayError::internal(format!(
            "Arrow {context} contains duplicate field `{name}`"
        )));
    }
    Ok(())
}

fn downcast<T: 'static>(array: &dyn Array) -> Result<&T, GatewayError> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        GatewayError::internal(format!(
            "Arrow array does not match its declared type {}",
            array.data_type()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::rest::codec::{RowShape, SchemaDecoder};
    use arrow::array::{
        ArrayRef, Int32Builder, ListBuilder, MapBuilder, StringBuilder, StructArray, make_builder,
    };
    use arrow::datatypes::{Field, Fields, Schema};
    use fluss::metadata::{
        ArrayType, BigIntType, BinaryType, BooleanType, DataField, DataType, DateType, DecimalType,
        DoubleType, FloatType, IntType, MapType, RowType, StringType, TimeType, TimestampLTzType,
        TimestampType,
    };
    use fluss::record::to_arrow_schema;
    use serde_json::json;
    use std::sync::Arc;

    fn as_json(array: &dyn Array, index: usize) -> JsonValue {
        value_to_json(array, index).expect("conversion succeeds")
    }

    fn field(name: &str, data_type: DataType) -> DataField {
        DataField::new(name, data_type, None)
    }

    fn round_trip(row_type: RowType, input: &str) -> JsonValue {
        let decoder = SchemaDecoder::new(row_type.clone()).unwrap();
        let row = decoder
            .decode_row("entry `round-trip`", input.as_bytes(), RowShape::Complete)
            .unwrap();
        let schema = to_arrow_schema(&row_type).unwrap();
        let columns = row
            .values
            .iter()
            .zip(row_type.fields())
            .zip(schema.fields())
            .map(|((datum, fluss_field), arrow_field)| {
                let mut builder = make_builder(arrow_field.data_type(), 1);
                datum
                    .append_to(
                        builder.as_mut(),
                        fluss_field.data_type(),
                        arrow_field.data_type(),
                    )
                    .unwrap();
                builder.finish()
            })
            .collect();
        let batch = RecordBatch::try_new(schema, columns).unwrap();
        JsonValue::Object(record_batch_to_json_rows(&batch).unwrap().remove(0))
    }

    #[test]
    fn renders_scalars_without_precision_loss() {
        let bigint = Int64Array::from(vec![Some(i64::MAX), Some(i64::MIN), None]);
        assert_eq!(as_json(&bigint, 0), json!("9223372036854775807"));
        assert_eq!(as_json(&bigint, 1), json!("-9223372036854775808"));
        assert_eq!(as_json(&bigint, 2), JsonValue::Null);
        assert_eq!(
            as_json(&Float32Array::from(vec![1.1_f32]), 0).to_string(),
            "1.1"
        );
        assert_eq!(
            as_json(&Float64Array::from(vec![f64::INFINITY]), 0),
            json!("Infinity")
        );
        let decimal = Decimal128Array::from(vec![Some(12_345_i128)])
            .with_precision_and_scale(10, 2)
            .unwrap();
        assert_eq!(as_json(&decimal, 0), json!("123.45"));
        assert_eq!(
            as_json(&BinaryArray::from(vec![b"\x00\xff".as_slice()]), 0),
            json!("AP8=")
        );
    }

    #[test]
    fn renders_temporal_values_deterministically() {
        assert_eq!(
            as_json(&Date32Array::from(vec![20_484]), 0),
            json!("2026-01-31")
        );
        assert_eq!(
            as_json(&Time32MillisecondArray::from(vec![45_296_789]), 0),
            json!("12:34:56.789")
        );
        let timestamp =
            TimestampMicrosecondArray::from(vec![1_769_862_896_789_123_i64]).with_timezone("UTC");
        assert_eq!(as_json(&timestamp, 0), json!("2026-01-31T12:34:56.789123Z"));
    }

    #[test]
    fn renders_nested_list_struct_and_map_values() {
        let mut list_builder = ListBuilder::new(Int32Builder::new());
        list_builder.values().append_value(1);
        list_builder.values().append_null();
        list_builder.append(true);
        assert_eq!(as_json(&list_builder.finish(), 0), json!([1, null]));

        let fields = Fields::from(vec![
            Field::new("id", ArrowDataType::Int32, false),
            Field::new("name", ArrowDataType::Utf8, true),
        ]);
        let row = StructArray::new(
            fields,
            vec![
                Arc::new(Int32Array::from(vec![1])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("Ada")])) as ArrayRef,
            ],
            None,
        );
        assert_eq!(as_json(&row, 0), json!({"id": 1, "name": "Ada"}));

        let mut map_builder = MapBuilder::new(None, StringBuilder::new(), Int32Builder::new());
        map_builder.keys().append_value("b");
        map_builder.values().append_value(2);
        map_builder.keys().append_value("a");
        map_builder.values().append_value(1);
        map_builder.append(true).unwrap();
        assert_eq!(
            as_json(&map_builder.finish(), 0),
            json!([
                {"key": "b", "value": 2},
                {"key": "a", "value": 1}
            ])
        );
    }

    #[test]
    fn renders_record_batches_as_row_objects() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", ArrowDataType::Int32, false),
            Field::new("name", ArrowDataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec![Some("Ada"), None])),
            ],
        )
        .unwrap();
        let rows = record_batch_to_json_rows(&batch).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], json!(1));
        assert_eq!(rows[0]["name"], json!("Ada"));
        assert_eq!(rows[1]["name"], JsonValue::Null);
    }

    #[test]
    fn rejects_duplicate_arrow_field_names_instead_of_overwriting_values() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", ArrowDataType::Int32, false),
            Field::new("id", ArrowDataType::Int32, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1])),
                Arc::new(Int32Array::from(vec![2])),
            ],
        )
        .unwrap();
        assert!(record_batch_to_json_rows(&batch).is_err());
    }

    #[test]
    fn rejects_out_of_bounds_indexes_and_unsupported_arrow_types() {
        let values = Int32Array::from(vec![1]);
        assert!(value_to_json(&values, 1).is_err());
        let values = arrow::array::UInt32Array::from(vec![1]);
        assert!(value_to_json(&values, 0).is_err());
    }

    #[test]
    fn renders_timestamp_numeric_boundaries_without_overflow() {
        for value in [i64::MIN, i64::MAX] {
            let rendered = as_json(&TimestampSecondArray::from(vec![value]), 0);
            assert!(rendered.as_str().unwrap().contains('T'));
        }
    }

    #[test]
    fn json_rows_round_trip_through_generic_row_and_arrow() {
        let nested_row = RowType::new(vec![
            field("flag", DataType::Boolean(BooleanType::new())),
            field("note", DataType::String(StringType::new())),
        ]);
        let row_type = RowType::new(vec![
            field("id", DataType::Int(IntType::with_nullable(false))),
            field("big", DataType::BigInt(BigIntType::with_nullable(false))),
            field("score", DataType::Float(FloatType::new())),
            field("ratio", DataType::Double(DoubleType::new())),
            field(
                "amount",
                DataType::Decimal(DecimalType::new(20, 4).unwrap()),
            ),
            field("payload", DataType::Binary(BinaryType::new(2))),
            field("day", DataType::Date(DateType::new())),
            field("clock", DataType::Time(TimeType::new(6).unwrap())),
            field(
                "created",
                DataType::Timestamp(TimestampType::new(6).unwrap()),
            ),
            field(
                "observed",
                DataType::TimestampLTz(TimestampLTzType::new(3).unwrap()),
            ),
            field(
                "tags",
                DataType::Array(ArrayType::new(DataType::String(StringType::new()))),
            ),
            field(
                "attributes",
                DataType::Map(MapType::new(
                    DataType::String(StringType::new()),
                    DataType::Int(IntType::new()),
                )),
            ),
            field("profile", DataType::Row(nested_row)),
        ]);
        let rendered = round_trip(
            row_type,
            r#"{
                "id": 7,
                "big": 9007199254740993,
                "score": 1.1,
                "ratio": "Infinity",
                "amount": 1234567890123456.7800,
                "payload": "AP8=",
                "day": "2026-01-31",
                "clock": "12:34:56.789000",
                "created": "1969-12-31T23:59:59.999999",
                "observed": "2026-01-31T14:34:56.789+02:00",
                "tags": ["a", null],
                "attributes": [{"key": "b", "value": 2}, {"key": "a", "value": null}],
                "profile": {"flag": true}
            }"#,
        );
        assert_eq!(
            rendered,
            json!({
                "id": 7,
                "big": "9007199254740993",
                "score": 1.1,
                "ratio": "Infinity",
                "amount": "1234567890123456.7800",
                "payload": "AP8=",
                "day": "2026-01-31",
                "clock": "12:34:56.789000",
                "created": "1969-12-31T23:59:59.999999",
                "observed": "2026-01-31T12:34:56.789Z",
                "tags": ["a", null],
                "attributes": [{"key": "b", "value": 2}, {"key": "a", "value": null}],
                "profile": {"flag": true, "note": null}
            })
        );
    }

    #[test]
    fn preserves_non_string_map_keys_through_round_trip() {
        let row_type = RowType::new(vec![field(
            "attributes",
            DataType::Map(MapType::new(
                DataType::Int(IntType::new()),
                DataType::String(StringType::new()),
            )),
        )]);

        let rendered = round_trip(
            row_type,
            r#"{"attributes":[{"key":2,"value":"b"},{"key":1,"value":null}]}"#,
        );

        assert_eq!(
            rendered,
            json!({
                "attributes": [
                    {"key": 2, "value": "b"},
                    {"key": 1, "value": null}
                ]
            })
        );
    }
}
