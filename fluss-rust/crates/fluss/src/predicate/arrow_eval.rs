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
use crate::predicate::{BoundPredicate, CompoundFunction, LeafFunction, Literal};
use crate::record::to_arrow_type;
use arrow::array::{Array, ArrayRef, BooleanArray, Int64Array, Scalar, StringArray, make_builder};
use arrow::compute::cast;
use arrow::compute::kernels::boolean::{and, is_not_null, is_null, not, or};
use arrow::compute::kernels::cmp::{eq, gt, gt_eq, lt, lt_eq, neq};
use arrow::datatypes::{DataType as ArrowDataType, TimeUnit};
use arrow::record_batch::RecordBatch;

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
    literals: &[Literal],
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
        _ => compare(array, function, &literals[0], data_type),
    }
}

fn evaluate_membership(
    array: &ArrayRef,
    data_type: &FlussDataType,
    function: LeafFunction,
    literals: &[Literal],
    len: usize,
) -> Result<BooleanArray> {
    if function == LeafFunction::NotIn && literals.iter().any(Literal::is_null) {
        return Ok(BooleanArray::from(vec![false; len]));
    }

    let mut result = BooleanArray::from(vec![false; len]);
    for literal in literals.iter().filter(|literal| !literal.is_null()) {
        let mask = compare(array, LeafFunction::Equal, literal, data_type)?;
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
    literal: &Literal,
    data_type: &FlussDataType,
) -> Result<BooleanArray> {
    let constant = literal_array(literal, data_type)?;
    let left: &dyn Array = array.as_ref();
    let right = Scalar::new(constant);
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
    literal: &Literal,
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

/// Builds one scalar value, rather than repeating the literal for every input row.
fn literal_array(literal: &Literal, data_type: &FlussDataType) -> Result<ArrayRef> {
    let arrow_type = to_arrow_type(data_type)?;
    let timestamp = match literal {
        Literal::TimestampNtz(value) => {
            Some((value.get_millisecond(), value.get_nano_of_millisecond()))
        }
        Literal::TimestampLtz(value) => Some((
            value.get_epoch_millisecond(),
            value.get_nano_of_millisecond(),
        )),
        _ => None,
    };
    if let (Some((millis, nanos)), ArrowDataType::Timestamp(unit, _)) = (timestamp, &arrow_type) {
        // Row conversion permits truncation. Predicate bounds must remain exact,
        // including values whose intermediate millisecond scaling exceeds i64.
        let value = timestamp_to_arrow_value(millis, nanos, *unit)?;
        return cast(&Int64Array::from(vec![value]), &arrow_type).map_err(Into::into);
    }

    // Binding already normalizes literal types and decimal scale. The core row
    // converter also validates time precision and fixed-size binary widths.
    let mut builder = make_builder(&arrow_type, 1);
    literal
        .to_datum()
        .append_to(builder.as_mut(), data_type, &arrow_type)?;
    Ok(builder.finish())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{DataField, DataTypes, RowType};
    use crate::predicate::{Literal, Predicate, col, to_pb_predicate};
    use crate::row::{TimestampLtz, TimestampNtz};
    use arrow::array::{
        ArrayRef, Decimal128Array, FixedSizeBinaryArray, Int32Array, StringArray, new_empty_array,
    };
    use std::sync::Arc;

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
    fn missing_predicate_columns_are_rejected() {
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
        let cases = [
            (DataTypes::time_with_precision(0), Literal::Time(1)),
            (
                DataTypes::timestamp_with_precision(3),
                Literal::TimestampNtz(TimestampNtz::from_millis_nanos(1, 1).unwrap()),
            ),
            (
                DataTypes::timestamp_with_precision(6),
                Literal::TimestampNtz(TimestampNtz::from_millis_nanos(-1, 999_999).unwrap()),
            ),
            (
                DataTypes::timestamp_with_precision(9),
                Literal::TimestampNtz(TimestampNtz::new(i64::MAX)),
            ),
            (DataTypes::binary(4), Literal::Bytes(vec![1, 2, 3])),
        ];
        for (data_type, literal) in cases {
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
            let array = new_empty_array(&to_arrow_type(&data_type).unwrap());
            let batch = RecordBatch::try_from_iter([("c", array)]).unwrap();
            assert!(bound.evaluate_batch(&batch).is_err(), "{data_type}");
        }
    }

    #[test]
    fn scalar_comparisons_preserve_nulls_and_operators() {
        let array: ArrayRef = Arc::new(Int32Array::from(vec![Some(1), None, Some(2), Some(3)]));
        for (function, expected) in [
            (LeafFunction::Equal, [false, false, true, false]),
            (LeafFunction::NotEqual, [true, false, false, true]),
            (LeafFunction::LessThan, [true, false, false, false]),
            (LeafFunction::LessOrEqual, [true, false, true, false]),
            (LeafFunction::GreaterThan, [false, false, false, true]),
            (LeafFunction::GreaterOrEqual, [false, false, true, true]),
        ] {
            let mask = compare(&array, function, &Literal::Int32(2), &DataTypes::int()).unwrap();
            assert_eq!(mask.iter().collect::<Vec<_>>(), expected.map(Some));
        }
    }

    #[test]
    fn core_converted_scalars_support_arrow_comparison() {
        for (data_type, literal) in [
            (DataTypes::boolean(), Literal::Bool(true)),
            (DataTypes::int(), Literal::Int32(-3)),
            (DataTypes::double(), Literal::Float64(-2.5)),
            (DataTypes::string(), Literal::String("hello".into())),
            (DataTypes::bytes(), Literal::Bytes(vec![0, 255])),
            (DataTypes::date(), Literal::Date(-1)),
            (DataTypes::time_with_precision(0), Literal::Time(1_000)),
            (DataTypes::time_with_precision(9), Literal::Time(1)),
        ] {
            let scalar = literal_array(&literal, &data_type).unwrap();
            assert_eq!(scalar.len(), 1);
            assert_eq!(scalar.data_type(), &to_arrow_type(&data_type).unwrap());
            let indices = arrow::array::UInt32Array::from(vec![Some(0), None, Some(0)]);
            let array = arrow::compute::take(scalar.as_ref(), &indices, None).unwrap();
            let mask = compare(&array, LeafFunction::Equal, &literal, &data_type).unwrap();
            assert_eq!(
                mask.iter().collect::<Vec<_>>(),
                [Some(true), Some(false), Some(true)]
            );
        }
    }

    #[test]
    fn timestamp_scalars_preserve_units_timezones_and_range_boundaries() {
        for (precision, millis, nanos, expected) in [
            (0, -1_000, 0, -1),
            (0, 0, 0, 0),
            (3, i64::MIN, 0, i64::MIN),
            (3, i64::MAX, 0, i64::MAX),
            (6, -1, 999_000, -1),
            (6, -9_223_372_036_854_776, 192_000, i64::MIN),
            (6, 9_223_372_036_854_775, 807_000, i64::MAX),
            (9, -1, 999_999, -1),
            (9, -9_223_372_036_855, 224_192, i64::MIN),
            (9, 9_223_372_036_854, 775_807, i64::MAX),
        ] {
            for (data_type, literal) in [
                (
                    DataTypes::timestamp_with_precision(precision),
                    Literal::TimestampNtz(TimestampNtz::from_millis_nanos(millis, nanos).unwrap()),
                ),
                (
                    DataTypes::timestamp_ltz_with_precision(precision),
                    Literal::TimestampLtz(TimestampLtz::from_millis_nanos(millis, nanos).unwrap()),
                ),
            ] {
                let scalar = literal_array(&literal, &data_type).unwrap();
                assert_eq!(scalar.data_type(), &to_arrow_type(&data_type).unwrap());
                let integers = cast(scalar.as_ref(), &ArrowDataType::Int64).unwrap();
                assert_eq!(
                    integers
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap()
                        .value(0),
                    expected
                );
            }
        }
    }

    #[test]
    fn fixed_binary_scalar_filters_empty_and_nonempty_batches() {
        let data_type = DataTypes::binary(4);
        let literal = Literal::Bytes(vec![1, 2, 3, 4]);
        let scalar = literal_array(&literal, &data_type).unwrap();
        assert_eq!(scalar.data_type(), &ArrowDataType::FixedSizeBinary(4));
        assert_eq!(scalar.len(), 1);
        for len in [0, 3] {
            let array: ArrayRef = Arc::new(
                FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                    std::iter::repeat_n(Some([1, 2, 3, 4].as_slice()), len),
                    4,
                )
                .unwrap(),
            );
            let mask = compare(&array, LeafFunction::Equal, &literal, &data_type).unwrap();
            assert_eq!(mask.len(), len);
            assert_eq!(mask.true_count(), len);
        }
    }

    #[test]
    fn decimal_literals_preserve_signed_unscaled_values() {
        for unscaled in [0, i128::from(i64::MAX) + 1, -(10_i128.pow(38) - 1)] {
            let value =
                crate::row::Decimal::from_unscaled_bytes(&unscaled.to_be_bytes(), 38, 3).unwrap();
            let array =
                literal_array(&Literal::Decimal(value), &DataTypes::decimal(38, 3)).unwrap();
            let array = array.as_any().downcast_ref::<Decimal128Array>().unwrap();
            assert_eq!(array.values().as_ref(), &[unscaled]);
            assert_eq!(array.data_type(), &ArrowDataType::Decimal128(38, 3));
        }
    }
}
