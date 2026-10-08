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

//! Exact Arrow evaluation of schema-bound core predicates.

use crate::error::Error::IllegalArgument;
use crate::error::Result;
use crate::metadata::DataType as FlussDataType;
use crate::predicate::{BoundLiteral, BoundPredicate, CompoundFunction, LeafFunction};
use crate::record::to_arrow_type;
use arrow::array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal128Array, FixedSizeBinaryArray,
    Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, StringArray,
    Time32MillisecondArray, Time32SecondArray, Time64MicrosecondArray, Time64NanosecondArray,
    TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    TimestampSecondArray,
};
use arrow::compute::kernels::boolean::{and, is_not_null, is_null, not, or};
use arrow::compute::kernels::cmp::{eq, gt, gt_eq, lt, lt_eq, neq};
use arrow::datatypes::{DataType as ArrowDataType, TimeUnit};
use arrow::record_batch::RecordBatch;
use std::sync::Arc;

impl BoundPredicate {
    /// Evaluates this predicate exactly against an Arrow record batch.
    ///
    /// Leaf predicates follow Fluss predicate semantics: null input values
    /// evaluate to false, and `NOT IN` with a null literal matches no rows.
    #[doc(hidden)]
    pub fn evaluate_batch(&self, batch: &RecordBatch) -> Result<BooleanArray> {
        match self {
            Self::AlwaysTrue => Ok(BooleanArray::from(vec![true; batch.num_rows()])),
            Self::Compound { function, children } => {
                let initial = match function {
                    CompoundFunction::And => true,
                    CompoundFunction::Or => false,
                };
                let mut result = BooleanArray::from(vec![initial; batch.num_rows()]);
                for child in children {
                    let child = child.evaluate_batch(batch)?;
                    result = match function {
                        CompoundFunction::And => and(&result, &child),
                        CompoundFunction::Or => or(&result, &child),
                    }?;
                }
                Ok(result)
            }
            Self::Leaf {
                field_name,
                data_type,
                function,
                literals,
                ..
            } => evaluate_leaf(batch, field_name, data_type, *function, literals),
        }
    }
}

fn evaluate_leaf(
    batch: &RecordBatch,
    field_name: &str,
    data_type: &FlussDataType,
    function: LeafFunction,
    literals: &[BoundLiteral],
) -> Result<BooleanArray> {
    let array = batch
        .column_by_name(field_name)
        .ok_or_else(|| IllegalArgument {
            message: format!("Arrow batch is missing predicate column '{field_name}'"),
        })?;
    let expected_arrow_type = to_arrow_type(data_type)?;
    if array.data_type() != &expected_arrow_type {
        return Err(IllegalArgument {
            message: format!(
                "predicate column '{field_name}' has Arrow type {}, expected {expected_arrow_type}",
                array.data_type()
            ),
        });
    }

    match function {
        LeafFunction::IsNull => is_null(array.as_ref()).map_err(Into::into),
        LeafFunction::IsNotNull => is_not_null(array.as_ref()).map_err(Into::into),
        LeafFunction::In | LeafFunction::NotIn => {
            evaluate_membership(array, data_type, function, literals, batch.num_rows())
        }
        LeafFunction::StartsWith | LeafFunction::EndsWith | LeafFunction::Contains => {
            evaluate_string_function(array, function, &literals[0])
        }
        _ => compare(array, function, &literals[0], data_type, batch.num_rows()),
    }
}

fn evaluate_membership(
    array: &ArrayRef,
    data_type: &FlussDataType,
    function: LeafFunction,
    literals: &[BoundLiteral],
    len: usize,
) -> Result<BooleanArray> {
    if function == LeafFunction::NotIn && literals.iter().any(BoundLiteral::is_null) {
        return Ok(BooleanArray::from(vec![false; len]));
    }

    let mut result = BooleanArray::from(vec![false; len]);
    for literal in literals.iter().filter(|literal| !literal.is_null()) {
        let mask = compare(array, LeafFunction::Equal, literal, data_type, len)?;
        result = or(&result, &mask)?;
    }

    if function == LeafFunction::In {
        return Ok(result);
    }

    let non_null = is_not_null(array.as_ref())?;
    and(&non_null, &not(&result)?).map_err(Into::into)
}

fn compare(
    array: &ArrayRef,
    function: LeafFunction,
    literal: &BoundLiteral,
    data_type: &FlussDataType,
    len: usize,
) -> Result<BooleanArray> {
    let constant = literal_array(literal, data_type, len)?;
    let left: &dyn Array = array.as_ref();
    let right: &dyn Array = constant.as_ref();
    let result = match function {
        LeafFunction::Equal => eq(&left, &right),
        LeafFunction::NotEqual => neq(&left, &right),
        LeafFunction::LessThan => lt(&left, &right),
        LeafFunction::LessOrEqual => lt_eq(&left, &right),
        LeafFunction::GreaterThan => gt(&left, &right),
        LeafFunction::GreaterOrEqual => gt_eq(&left, &right),
        _ => unreachable!("non-comparison leaf reached compare"),
    }?;
    Ok(null_as_false(&result))
}

fn evaluate_string_function(
    array: &ArrayRef,
    function: LeafFunction,
    literal: &BoundLiteral,
) -> Result<BooleanArray> {
    let expected = literal.as_string().ok_or_else(|| IllegalArgument {
        message: "string predicate was not bound to a string literal".to_string(),
    })?;
    let strings = array
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| IllegalArgument {
            message: format!(
                "string predicate cannot be evaluated against Arrow type {}",
                array.data_type()
            ),
        })?;
    Ok(strings
        .iter()
        .map(|value| {
            value
                .map(|value| matches_string(value, expected, function))
                .unwrap_or(false)
        })
        .collect())
}

fn matches_string(value: &str, expected: &str, function: LeafFunction) -> bool {
    match function {
        LeafFunction::StartsWith => value.starts_with(expected),
        LeafFunction::EndsWith => value.ends_with(expected),
        LeafFunction::Contains => value.contains(expected),
        _ => unreachable!("non-string function reached string evaluator"),
    }
}

fn null_as_false(mask: &BooleanArray) -> BooleanArray {
    mask.iter().map(|value| value.unwrap_or(false)).collect()
}

fn literal_array(
    literal: &BoundLiteral,
    data_type: &FlussDataType,
    len: usize,
) -> Result<ArrayRef> {
    let arrow_type = to_arrow_type(data_type)?;
    let array: ArrayRef = match (literal, &arrow_type) {
        (BoundLiteral::Bool(value), ArrowDataType::Boolean) => {
            Arc::new(BooleanArray::from(vec![*value; len]))
        }
        (BoundLiteral::Int8(value), ArrowDataType::Int8) => {
            Arc::new(Int8Array::from(vec![*value; len]))
        }
        (BoundLiteral::Int16(value), ArrowDataType::Int16) => {
            Arc::new(Int16Array::from(vec![*value; len]))
        }
        (BoundLiteral::Int32(value), ArrowDataType::Int32) => {
            Arc::new(Int32Array::from(vec![*value; len]))
        }
        (BoundLiteral::Int64(value), ArrowDataType::Int64) => {
            Arc::new(Int64Array::from(vec![*value; len]))
        }
        (BoundLiteral::Float32(value), ArrowDataType::Float32) => {
            Arc::new(Float32Array::from(vec![*value; len]))
        }
        (BoundLiteral::Float64(value), ArrowDataType::Float64) => {
            Arc::new(Float64Array::from(vec![*value; len]))
        }
        (BoundLiteral::String(value), ArrowDataType::Utf8) => {
            Arc::new(StringArray::from(vec![value.as_str(); len]))
        }
        (BoundLiteral::Bytes(value), ArrowDataType::Binary) => {
            Arc::new(BinaryArray::from(vec![value.as_slice(); len]))
        }
        (BoundLiteral::Bytes(value), ArrowDataType::FixedSizeBinary(width)) => {
            Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                std::iter::repeat_n(Some(value.as_slice()), len),
                *width,
            )?)
        }
        (BoundLiteral::Decimal(value), ArrowDataType::Decimal128(precision, scale)) => Arc::new(
            Decimal128Array::from(vec![decimal_to_i128(value)?; len])
                .with_precision_and_scale(*precision, *scale)?,
        ),
        (BoundLiteral::Date(value), ArrowDataType::Date32) => {
            Arc::new(Date32Array::from(vec![*value; len]))
        }
        (BoundLiteral::Time(value), ArrowDataType::Time32(unit)) => {
            time32_array(*value, *unit, len)?
        }
        (BoundLiteral::Time(value), ArrowDataType::Time64(unit)) => {
            time64_array(*value, *unit, len)?
        }
        (BoundLiteral::TimestampNtz(value), ArrowDataType::Timestamp(unit, timezone @ None)) => {
            timestamp_array(
                value.get_millisecond(),
                value.get_nano_of_millisecond(),
                *unit,
                timezone.clone(),
                len,
            )?
        }
        (BoundLiteral::TimestampLtz(value), ArrowDataType::Timestamp(unit, timezone @ Some(_))) => {
            timestamp_array(
                value.get_epoch_millisecond(),
                value.get_nano_of_millisecond(),
                *unit,
                timezone.clone(),
                len,
            )?
        }
        _ => {
            return Err(IllegalArgument {
                message: format!(
                    "bound predicate literal {literal:?} does not match column type {data_type}"
                ),
            });
        }
    };
    debug_assert_eq!(array.data_type(), &arrow_type);
    Ok(array)
}

fn time32_array(value: i32, unit: TimeUnit, len: usize) -> Result<ArrayRef> {
    match unit {
        TimeUnit::Second => {
            if value % 1_000 != 0 {
                return Err(inexact_time(value, unit));
            }
            Ok(Arc::new(Time32SecondArray::from(vec![value / 1_000; len])))
        }
        TimeUnit::Millisecond => Ok(Arc::new(Time32MillisecondArray::from(vec![value; len]))),
        unit => Err(IllegalArgument {
            message: format!("Arrow Time32 does not support {unit:?}"),
        }),
    }
}

fn time64_array(value: i32, unit: TimeUnit, len: usize) -> Result<ArrayRef> {
    match unit {
        TimeUnit::Microsecond => Ok(Arc::new(Time64MicrosecondArray::from(vec![
            i64::from(value)
                * 1_000;
            len
        ]))),
        TimeUnit::Nanosecond => Ok(Arc::new(Time64NanosecondArray::from(vec![
            i64::from(value)
                * 1_000_000;
            len
        ]))),
        unit => Err(IllegalArgument {
            message: format!("Arrow Time64 does not support {unit:?}"),
        }),
    }
}

fn timestamp_array(
    millis: i64,
    nano_of_millisecond: i32,
    unit: TimeUnit,
    timezone: Option<Arc<str>>,
    len: usize,
) -> Result<ArrayRef> {
    let value = timestamp_to_arrow_value(millis, nano_of_millisecond, unit)?;
    Ok(match unit {
        TimeUnit::Second => {
            Arc::new(TimestampSecondArray::from(vec![value; len]).with_timezone_opt(timezone))
        }
        TimeUnit::Millisecond => {
            Arc::new(TimestampMillisecondArray::from(vec![value; len]).with_timezone_opt(timezone))
        }
        TimeUnit::Microsecond => {
            Arc::new(TimestampMicrosecondArray::from(vec![value; len]).with_timezone_opt(timezone))
        }
        TimeUnit::Nanosecond => {
            Arc::new(TimestampNanosecondArray::from(vec![value; len]).with_timezone_opt(timezone))
        }
    })
}

fn timestamp_to_arrow_value(millis: i64, nano_of_millisecond: i32, unit: TimeUnit) -> Result<i64> {
    let total_nanos = i128::from(millis) * 1_000_000 + i128::from(nano_of_millisecond);
    let divisor = match unit {
        TimeUnit::Second => 1_000_000_000,
        TimeUnit::Millisecond => 1_000_000,
        TimeUnit::Microsecond => 1_000,
        TimeUnit::Nanosecond => 1,
    };
    if total_nanos % divisor != 0 {
        return Err(IllegalArgument {
            message: format!(
                "timestamp predicate literal cannot be represented exactly as Arrow {unit:?}"
            ),
        });
    }
    i64::try_from(total_nanos / divisor).map_err(|_| IllegalArgument {
        message: format!("timestamp predicate literal exceeds Arrow {unit:?} range"),
    })
}

fn inexact_time(value: i32, unit: TimeUnit) -> crate::error::Error {
    IllegalArgument {
        message: format!(
            "time predicate literal {value}ms cannot be represented exactly as Arrow {unit:?}"
        ),
    }
}

fn decimal_to_i128(value: &crate::row::Decimal) -> Result<i128> {
    value
        .to_big_decimal()
        .as_bigint_and_exponent()
        .0
        .try_into()
        .map_err(|_| IllegalArgument {
            message: format!(
                "decimal predicate literal {} exceeds Arrow Decimal128 range",
                value.to_big_decimal()
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{DataField, DataTypes, RowType};
    use crate::predicate::{Literal, Predicate, col, to_pb_predicate};
    use crate::row::TimestampNtz;
    use arrow::array::{ArrayRef, Int32Array, StringArray};

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("name", DataTypes::string(), None),
        ])
    }

    fn batch() -> RecordBatch {
        RecordBatch::try_from_iter(vec![
            (
                "id",
                Arc::new(Int32Array::from(vec![Some(1), None, Some(3)])) as ArrayRef,
            ),
            (
                "name",
                Arc::new(StringArray::from(vec![
                    Some("alpha"),
                    None,
                    Some("alphabet"),
                ])) as ArrayRef,
            ),
        ])
        .unwrap()
    }

    fn evaluate(predicate: Predicate) -> Vec<bool> {
        BoundPredicate::bind(Some(&predicate), &row_type())
            .unwrap()
            .evaluate_batch(&batch())
            .unwrap()
            .values()
            .iter()
            .collect()
    }

    #[test]
    fn evaluates_string_functions_and_null_inputs_exactly() {
        assert_eq!(
            evaluate(col("name").starts_with("alpha")),
            vec![true, false, true]
        );
        assert_eq!(
            evaluate(col("name").ends_with("bet")),
            vec![false, false, true]
        );
        assert_eq!(
            evaluate(col("name").contains("ph")),
            vec![true, false, true]
        );
    }

    #[test]
    fn compound_predicates_treat_null_leaf_results_as_false() {
        assert_eq!(
            evaluate(col("id").eq(1_i32).or(col("name").is_null())),
            vec![true, true, false]
        );
    }

    #[test]
    fn in_and_not_in_match_core_null_semantics() {
        assert_eq!(
            evaluate(col("id").is_in(vec![Some(1_i32), None])),
            vec![true, false, false]
        );
        assert_eq!(
            evaluate(col("id").not_in(vec![Some(1_i32), None])),
            vec![false, false, false]
        );
        assert_eq!(
            evaluate(col("id").not_in(Vec::<i32>::new())),
            vec![true, false, true]
        );
    }

    #[test]
    fn missing_or_changed_physical_columns_are_rejected() {
        let bound = BoundPredicate::bind(Some(&col("id").eq(1_i32)), &row_type()).unwrap();
        let missing = RecordBatch::try_from_iter(vec![(
            "name",
            Arc::new(StringArray::from(vec!["one"])) as ArrayRef,
        )])
        .unwrap();

        assert!(bound.evaluate_batch(&missing).is_err());
    }

    #[test]
    fn wire_literals_are_not_restricted_by_arrow_physical_types() {
        let cases: Vec<(FlussDataType, Literal, ArrayRef)> = vec![
            (
                DataTypes::time_with_precision(0),
                Literal::Time(1),
                Arc::new(Time32SecondArray::from(vec![0])),
            ),
            (
                DataTypes::timestamp_with_precision(3),
                Literal::TimestampNtz(TimestampNtz::from_millis_nanos(1, 1).unwrap()),
                Arc::new(TimestampMillisecondArray::from(vec![0])),
            ),
            (
                DataTypes::timestamp_with_precision(9),
                Literal::TimestampNtz(TimestampNtz::new(i64::MAX)),
                Arc::new(TimestampNanosecondArray::from(vec![0])),
            ),
            (
                DataTypes::binary(4),
                Literal::Bytes(vec![1, 2, 3]),
                Arc::new(
                    FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                        [Some([1, 2, 3, 4].as_slice())].into_iter(),
                        4,
                    )
                    .unwrap(),
                ),
            ),
        ];
        for (data_type, literal, array) in cases {
            let row_type = RowType::new(vec![DataField::with_field_id(
                "c",
                data_type.clone(),
                None,
                7,
            )]);
            let predicate = col("c").eq(literal);
            // Preserve the existing wire encoder's accepted inputs. Exact Arrow
            // evaluation must reject narrowing instead of truncating literals.
            to_pb_predicate(&predicate, &row_type).unwrap();
            let bound = BoundPredicate::bind(Some(&predicate), &row_type).unwrap();
            let batch = RecordBatch::try_from_iter([("c", array)]).unwrap();
            assert!(bound.evaluate_batch(&batch).is_err(), "{data_type}");
        }
    }

    #[test]
    fn fixed_binary_literals_use_the_declared_width_even_without_rows() {
        for len in [0, 1, 3] {
            let array = literal_array(
                &BoundLiteral::Bytes(vec![1, 2, 3, 4]),
                &DataTypes::binary(4),
                len,
            )
            .unwrap();
            assert_eq!(array.data_type(), &ArrowDataType::FixedSizeBinary(4));
            assert_eq!(array.len(), len);
        }
    }

    #[test]
    fn decimal_literals_preserve_signed_unscaled_values() {
        for unscaled in [
            0,
            127,
            128,
            -128,
            -129,
            i128::from(i64::MAX) + 1,
            -(10_i128.pow(38) - 1),
        ] {
            let value =
                crate::row::Decimal::from_unscaled_bytes(&unscaled.to_be_bytes(), 38, 3).unwrap();
            let array =
                literal_array(&Literal::Decimal(value), &DataTypes::decimal(38, 3), 2).unwrap();
            let array = array.as_any().downcast_ref::<Decimal128Array>().unwrap();
            assert_eq!(array.values().as_ref(), &[unscaled, unscaled]);
            assert_eq!(array.data_type(), &ArrowDataType::Decimal128(38, 3));
        }
    }
}
