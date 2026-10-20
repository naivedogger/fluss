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

use super::coercion::validate_leaf_shape;
use crate::error::Error::IllegalArgument;
use crate::error::Result;
use crate::metadata::DataType as FlussDataType;
use crate::predicate::{BoundPredicate, CompoundFunction, LeafFunction, Literal};
use crate::record::to_arrow_type;
use crate::row::Datum;
use crate::util::partition::parse_value_of_type;
use crate::util::{canonicalize_nan, compare_floats};
use arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, Int32Array, Int64Array, Scalar, StringArray,
    make_builder,
};
use arrow::compute::cast;
use arrow::compute::kernels::boolean::{and, is_not_null, is_null, or};
use arrow::compute::kernels::cmp::{eq, gt, gt_eq, lt, lt_eq, neq};
use arrow::datatypes::{DataType as ArrowDataType, Float32Type, Float64Type, TimeUnit};
use arrow::record_batch::RecordBatch;
use arrow::row::{RowConverter, SortField};
use std::cmp::Ordering;
use std::collections::HashSet;

impl BoundPredicate {
    /// Prepares an owned, reusable evaluator, building membership sets once.
    ///
    /// Rejects malformed leaf shapes and errors while preparing membership sets.
    /// Other literal conversions remain evaluation-time checks. The evaluator is
    /// immutable and can be shared across batch readers; its buffers
    /// are released when it is dropped. Batch column names and types are checked
    /// on each evaluation. Prefer this over `evaluate_batch` for a stream of batches.
    #[doc(hidden)]
    pub fn into_evaluator(self) -> Result<PredicateEvaluator> {
        Ok(PredicateEvaluator {
            root: PreparedPredicate::new(self)?,
        })
    }

    /// Evaluates a leaf against a non-null, encoded Fluss partition value.
    ///
    /// Uses the same exact filter semantics as [`Self::evaluate_batch`]. Returns
    /// an error for non-leaves, invalid encodings, unsupported types, or values
    /// that cannot be represented losslessly in the column's Arrow type.
    /// A pruning caller must retain the partition on error.
    #[doc(hidden)]
    pub fn evaluate_partition_value(&self, value: &str) -> Result<bool> {
        let Self::Leaf {
            field_name,
            data_type,
            ..
        } = self
        else {
            return Err(IllegalArgument {
                message: "partition value evaluation requires a leaf predicate".into(),
            });
        };
        let datum = parse_value_of_type(value, data_type)?;
        let temporal = match &datum {
            Datum::TimestampNtz(value) => Some(Literal::TimestampNtz(*value)),
            Datum::TimestampLtz(value) => Some(Literal::TimestampLtz(*value)),
            Datum::Time(value) => Some(Literal::Time(value.get_inner())),
            _ => None,
        };
        let array = if let Some(literal) = temporal {
            membership_scalar(&literal, data_type)?.ok_or_else(|| IllegalArgument {
                message: "partition value cannot be represented exactly in its Arrow type".into(),
            })?
        } else {
            datum_array(&datum, data_type)?
        };
        let batch = RecordBatch::try_from_iter([(field_name.as_str(), array)])?;
        Ok(self.evaluate_batch(&batch)?.value(0))
    }

    /// Evaluates this predicate exactly against an Arrow record batch.
    ///
    /// Returns a non-null filter mask. Except for `IS NULL`, null input values
    /// do not match; `NOT IN` with a null literal matches no rows.
    /// Membership sets are built per call; use [`Self::into_evaluator`]
    /// to reuse them across batches.
    #[doc(hidden)]
    pub fn evaluate_batch(&self, batch: &RecordBatch) -> Result<BooleanArray> {
        match self {
            Self::AlwaysTrue => Ok(BooleanArray::from(vec![true; batch.num_rows()])),
            Self::Compound { function, children } => combine_masks(
                *function,
                batch.num_rows(),
                children.iter().map(|child| child.evaluate_batch(batch)),
            ),
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
    validate_leaf_shape(function, field_name, literals)?;
    let array = predicate_column(batch, field_name, data_type)?;

    match function {
        LeafFunction::IsNull => is_null(array.as_ref()).map_err(Into::into),
        LeafFunction::IsNotNull => is_not_null(array.as_ref()).map_err(Into::into),
        LeafFunction::In | LeafFunction::NotIn => {
            evaluate_membership(array, data_type, function, literals)
        }
        LeafFunction::StartsWith | LeafFunction::EndsWith | LeafFunction::Contains => {
            evaluate_string_function(array, function, &literals[0])
        }
        _ => compare(array, function, &literals[0], data_type),
    }
}

fn predicate_column<'a>(
    batch: &'a RecordBatch,
    field_name: &str,
    data_type: &FlussDataType,
) -> Result<&'a ArrayRef> {
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

    Ok(array)
}

fn evaluate_membership(
    array: &ArrayRef,
    data_type: &FlussDataType,
    function: LeafFunction,
    literals: &[Literal],
) -> Result<BooleanArray> {
    if function == LeafFunction::NotIn && literals.iter().any(Literal::is_null) {
        return constant_filter(array, false);
    }
    if literals.is_empty() {
        return constant_filter(array, function == LeafFunction::NotIn);
    }
    MembershipSet::new(literals, data_type)?.evaluate(array, function)
}

fn compare(
    array: &ArrayRef,
    function: LeafFunction,
    literal: &Literal,
    data_type: &FlussDataType,
) -> Result<BooleanArray> {
    let nanos = temporal_nanos(literal, data_type);
    if let Some(nanos) = nanos {
        match array.data_type() {
            ArrowDataType::Timestamp(unit, _)
            | ArrowDataType::Time32(unit)
            | ArrowDataType::Time64(unit) => {
                return compare_temporal(array, function, nanos, *unit);
            }
            _ => {}
        }
    }
    compare_scalar(array, function, literal_array(literal, data_type)?)
}

fn temporal_nanos(literal: &Literal, data_type: &FlussDataType) -> Option<i128> {
    match (data_type, literal) {
        (FlussDataType::Time(_), Literal::Time(millis)) => Some(i128::from(*millis) * 1_000_000),
        (FlussDataType::Timestamp(_), Literal::TimestampNtz(value)) => Some(
            i128::from(value.get_millisecond()) * 1_000_000
                + i128::from(value.get_nano_of_millisecond()),
        ),
        (FlussDataType::TimestampLTz(_), Literal::TimestampLtz(value)) => Some(
            i128::from(value.get_epoch_millisecond()) * 1_000_000
                + i128::from(value.get_nano_of_millisecond()),
        ),
        _ => None,
    }
}

fn compare_scalar(
    array: &ArrayRef,
    function: LeafFunction,
    constant: ArrayRef,
) -> Result<BooleanArray> {
    let result = match array.data_type() {
        ArrowDataType::Float32 => Some(compare_values(
            array.as_primitive::<Float32Type>().iter(),
            constant.as_primitive::<Float32Type>().value(0),
            function,
            |a, b| compare_floats(f64::from(a), f64::from(b)),
        )),
        ArrowDataType::Float64 => Some(compare_values(
            array.as_primitive::<Float64Type>().iter(),
            constant.as_primitive::<Float64Type>().value(0),
            function,
            compare_floats,
        )),
        ArrowDataType::Binary
            if !matches!(function, LeafFunction::Equal | LeafFunction::NotEqual) =>
        {
            Some(compare_values(
                array.as_binary::<i32>().iter(),
                constant.as_binary::<i32>().value(0),
                function,
                compare_binary,
            ))
        }
        ArrowDataType::FixedSizeBinary(_)
            if !matches!(function, LeafFunction::Equal | LeafFunction::NotEqual) =>
        {
            Some(compare_values(
                array.as_fixed_size_binary().iter(),
                constant.as_fixed_size_binary().value(0),
                function,
                compare_binary,
            ))
        }
        _ => None,
    };
    if let Some(result) = result {
        return Ok(result);
    }
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

fn compare_values<T: Copy>(
    values: impl Iterator<Item = Option<T>>,
    constant: T,
    function: LeafFunction,
    compare: impl Fn(T, T) -> Ordering,
) -> BooleanArray {
    values
        .map(|value| {
            value.is_some_and(|value| {
                let ordering = compare(value, constant);
                match function {
                    LeafFunction::Equal => ordering.is_eq(),
                    LeafFunction::NotEqual => !ordering.is_eq(),
                    LeafFunction::LessThan => ordering.is_lt(),
                    LeafFunction::LessOrEqual => !ordering.is_gt(),
                    LeafFunction::GreaterThan => ordering.is_gt(),
                    LeafFunction::GreaterOrEqual => !ordering.is_lt(),
                    _ => unreachable!("non-comparison leaf reached compare"),
                }
            })
        })
        .collect()
}

fn compare_binary(left: &[u8], right: &[u8]) -> Ordering {
    // Java byte[] comparison is signed, unlike Arrow's binary ordering.
    left.iter()
        .map(|b| *b as i8)
        .cmp(right.iter().map(|b| *b as i8))
}

fn evaluate_string_function(
    array: &ArrayRef,
    function: LeafFunction,
    literal: &Literal,
) -> Result<BooleanArray> {
    let Literal::String(expected) = literal else {
        return Err(IllegalArgument {
            message: "string predicate was not bound to a string literal".to_string(),
        });
    };
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
    datum_array(&literal.to_datum(), data_type)
}

fn datum_array(datum: &Datum<'_>, data_type: &FlussDataType) -> Result<ArrayRef> {
    let arrow_type = to_arrow_type(data_type)?;
    // Temporal bounds and partition values use the lossless path instead of
    // the row writer's narrowing conversions.
    let mut builder = make_builder(&arrow_type, 1);
    datum.append_to(builder.as_mut(), data_type, &arrow_type)?;
    Ok(builder.finish())
}

fn nanos_per_unit(unit: TimeUnit) -> i128 {
    match unit {
        TimeUnit::Second => 1_000_000_000,
        TimeUnit::Millisecond => 1_000_000,
        TimeUnit::Microsecond => 1_000,
        TimeUnit::Nanosecond => 1,
    }
}

fn compare_temporal(
    array: &ArrayRef,
    mut function: LeafFunction,
    nanos: i128,
    unit: TimeUnit,
) -> Result<BooleanArray> {
    let divisor = nanos_per_unit(unit);
    // Floor, rather than truncate toward zero, so negative bounds obey the
    // same rewrite. Only the scalar changes; the column stays in its native unit.
    let bound = nanos.div_euclid(divisor);
    if nanos.rem_euclid(divisor) != 0 {
        function = match function {
            LeafFunction::Equal => return constant_filter(array, false),
            LeafFunction::NotEqual => return constant_filter(array, true),
            LeafFunction::LessThan | LeafFunction::LessOrEqual => LeafFunction::LessOrEqual,
            LeafFunction::GreaterThan | LeafFunction::GreaterOrEqual => LeafFunction::GreaterThan,
            _ => unreachable!("non-comparison leaf reached temporal evaluator"),
        };
    }
    let Some(constant) = temporal_scalar(bound, array.data_type())? else {
        // An out-of-range signed integer is below the minimum iff it is negative.
        let matches = match function {
            LeafFunction::Equal => false,
            LeafFunction::NotEqual => true,
            LeafFunction::LessThan | LeafFunction::LessOrEqual => bound > 0,
            LeafFunction::GreaterThan | LeafFunction::GreaterOrEqual => bound < 0,
            _ => unreachable!("non-comparison leaf reached temporal evaluator"),
        };
        return constant_filter(array, matches);
    };
    compare_scalar(array, function, constant)
}

// Constant folding here is for a filter mask, not a nullable SQL expression.
fn constant_filter(array: &ArrayRef, matches: bool) -> Result<BooleanArray> {
    if matches {
        is_not_null(array.as_ref()).map_err(Into::into)
    } else {
        Ok(BooleanArray::from(vec![false; array.len()]))
    }
}

/// An immutable evaluator with membership sets prepared for repeated batches.
#[doc(hidden)]
#[derive(Debug)]
pub struct PredicateEvaluator {
    root: PreparedPredicate,
}

impl PredicateEvaluator {
    /// Returns a non-null filter mask, using the same semantics and batch schema
    /// checks as [`BoundPredicate::evaluate_batch`].
    pub fn evaluate_batch(&self, batch: &RecordBatch) -> Result<BooleanArray> {
        self.root.evaluate_batch(batch)
    }
}

#[derive(Debug)]
enum PreparedPredicate {
    Direct(BoundPredicate),
    Membership {
        field_name: String,
        data_type: FlussDataType,
        function: LeafFunction,
        set: MembershipSet,
    },
    Compound {
        function: CompoundFunction,
        children: Vec<PreparedPredicate>,
    },
}

impl PreparedPredicate {
    fn new(predicate: BoundPredicate) -> Result<Self> {
        match predicate {
            BoundPredicate::Compound { function, children } => Ok(Self::Compound {
                function,
                children: children.into_iter().map(Self::new).collect::<Result<_>>()?,
            }),
            BoundPredicate::Leaf {
                ref field_name,
                ref data_type,
                function,
                ref literals,
                ..
            } => {
                validate_leaf_shape(function, field_name, literals)?;
                if matches!(function, LeafFunction::In | LeafFunction::NotIn)
                    && !literals.is_empty()
                    && !(function == LeafFunction::NotIn && literals.iter().any(Literal::is_null))
                {
                    Ok(Self::Membership {
                        field_name: field_name.clone(),
                        data_type: data_type.clone(),
                        function,
                        set: MembershipSet::new(literals, data_type)?,
                    })
                } else {
                    Ok(Self::Direct(predicate))
                }
            }
            other => Ok(Self::Direct(other)),
        }
    }

    fn evaluate_batch(&self, batch: &RecordBatch) -> Result<BooleanArray> {
        match self {
            Self::Direct(predicate) => predicate.evaluate_batch(batch),
            Self::Membership {
                field_name,
                data_type,
                function,
                set,
            } => set.evaluate(predicate_column(batch, field_name, data_type)?, *function),
            Self::Compound { function, children } => combine_masks(
                *function,
                batch.num_rows(),
                children.iter().map(|child| child.evaluate_batch(batch)),
            ),
        }
    }
}

fn combine_masks(
    function: CompoundFunction,
    len: usize,
    masks: impl Iterator<Item = Result<BooleanArray>>,
) -> Result<BooleanArray> {
    let mut result = BooleanArray::from(vec![function == CompoundFunction::And; len]);
    for mask in masks {
        let mask = mask?;
        result = match function {
            CompoundFunction::And => and(&result, &mask),
            CompoundFunction::Or => or(&result, &mask),
        }?;
    }
    Ok(result)
}

#[derive(Debug)]
enum MembershipSet {
    Floats(HashSet<u64>),
    Rows {
        converter: RowConverter,
        values: HashSet<Box<[u8]>>,
    },
}

impl MembershipSet {
    fn new(literals: &[Literal], data_type: &FlussDataType) -> Result<Self> {
        let arrow_type = to_arrow_type(data_type)?;
        if matches!(arrow_type, ArrowDataType::Float32 | ArrowDataType::Float64) {
            let mut values = HashSet::with_capacity(literals.len());
            for literal in literals.iter().filter(|literal| !literal.is_null()) {
                let scalar = literal_array(literal, data_type)?;
                let value = match arrow_type {
                    ArrowDataType::Float32 => {
                        f64::from(scalar.as_primitive::<Float32Type>().value(0))
                    }
                    _ => scalar.as_primitive::<Float64Type>().value(0),
                };
                values.insert(canonicalize_nan(value).to_bits());
            }
            return Ok(Self::Floats(values));
        }
        let converter = RowConverter::new(vec![SortField::new(arrow_type)])?;
        let mut values = HashSet::with_capacity(literals.len());
        for literal in literals.iter().filter(|literal| !literal.is_null()) {
            if let Some(scalar) = membership_scalar(literal, data_type)? {
                let rows = converter.convert_columns(&[scalar])?;
                values.insert(rows.row(0).as_ref().into());
            }
        }
        Ok(Self::Rows { converter, values })
    }

    fn evaluate(&self, array: &ArrayRef, function: LeafFunction) -> Result<BooleanArray> {
        let negate = function == LeafFunction::NotIn;
        let (converter, values) = match self {
            Self::Floats(values) => {
                // Canonicalize only NaNs; signed-zero keys remain distinct.
                // Probe fixed-size keys without allocating per row or copying a batch.
                let matches = |value: Option<f64>| {
                    value.is_some_and(|value| {
                        values.contains(&canonicalize_nan(value).to_bits()) != negate
                    })
                };
                return Ok(match array.data_type() {
                    ArrowDataType::Float32 => array
                        .as_primitive::<Float32Type>()
                        .iter()
                        .map(|value| matches(value.map(f64::from)))
                        .collect(),
                    ArrowDataType::Float64 => array
                        .as_primitive::<Float64Type>()
                        .iter()
                        .map(matches)
                        .collect(),
                    _ => unreachable!("membership column type was validated"),
                });
            }
            Self::Rows { converter, values } => (converter, values),
        };
        if values.is_empty() {
            return constant_filter(array, negate);
        }
        // For non-floats Arrow row encoding has the same equality relation as
        // Fluss, including byte arrays whose ordering differs from Arrow's.
        let rows = converter.convert_columns(std::slice::from_ref(array))?;
        Ok(rows
            .iter()
            .enumerate()
            .map(|(index, row)| array.is_valid(index) && (values.contains(row.as_ref()) != negate))
            .collect())
    }
}

fn membership_scalar(literal: &Literal, data_type: &FlussDataType) -> Result<Option<ArrayRef>> {
    let arrow_type = to_arrow_type(data_type)?;
    if let Some(nanos) = temporal_nanos(literal, data_type) {
        let unit = match arrow_type {
            ArrowDataType::Timestamp(unit, _)
            | ArrowDataType::Time32(unit)
            | ArrowDataType::Time64(unit) => unit,
            _ => unreachable!("temporal literal without a temporal Arrow type"),
        };
        let divisor = nanos_per_unit(unit);
        if nanos % divisor != 0 {
            return Ok(None);
        }
        return temporal_scalar(nanos / divisor, &arrow_type);
    }
    literal_array(literal, data_type).map(Some)
}

fn temporal_scalar(value: i128, arrow_type: &ArrowDataType) -> Result<Option<ArrayRef>> {
    let scalar = if matches!(arrow_type, ArrowDataType::Time32(_)) {
        let Ok(value) = i32::try_from(value) else {
            return Ok(None);
        };
        cast(&Int32Array::from(vec![value]), arrow_type)?
    } else {
        let Ok(value) = i64::try_from(value) else {
            return Ok(None);
        };
        cast(&Int64Array::from(vec![value]), arrow_type)?
    };
    Ok(Some(scalar))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{DataField, DataTypes, RowType};
    use crate::predicate::tests::assert_comparisons;
    use crate::predicate::{Literal, Predicate, col, to_pb_predicate};
    use crate::row::{TimestampLtz, TimestampNtz};
    use arrow::array::{
        ArrayRef, Decimal128Array, FixedSizeBinaryArray, Int32Array, StringArray, new_empty_array,
    };
    use arrow::compute::kernels::boolean::not;
    use std::sync::Arc;

    #[test]
    fn partition_values_use_typed_parsing_and_exact_filters() {
        use crate::util::partition::convert_value_of_type;
        let cases = [
            (DataTypes::tinyint(), Literal::Int8(-1), "-1"),
            (DataTypes::smallint(), Literal::Int16(256), "256"),
            (DataTypes::int(), Literal::Int32(-1000), "-1000"),
            (
                DataTypes::bigint(),
                Literal::Int64(i64::MAX),
                "9223372036854775807",
            ),
            (DataTypes::boolean(), Literal::Bool(true), "true"),
            (DataTypes::float(), Literal::Float32(1.5), "1_5"),
            (DataTypes::double(), Literal::Float64(-1.5), "-1_5"),
            (DataTypes::double(), Literal::Float64(-0.0), "-0_0"),
            (DataTypes::float(), Literal::Float32(f32::INFINITY), "Inf"),
            (DataTypes::float(), Literal::Float32(-f32::NAN), "NaN"),
            (DataTypes::double(), Literal::Float64(-f64::NAN), "NaN"),
            (
                DataTypes::double(),
                Literal::Float64(f64::NEG_INFINITY),
                "-Inf",
            ),
            (DataTypes::string(), Literal::String("US".into()), "US"),
            (DataTypes::char(2), Literal::String("US".into()), "US"),
            (DataTypes::bytes(), Literal::Bytes(vec![0x00, 0xff]), "00ff"),
            (
                DataTypes::binary(2),
                Literal::Bytes(vec![0x00, 0xff]),
                "00ff",
            ),
            (DataTypes::date(), Literal::Date(-1), "1969-12-31"),
            (DataTypes::date(), Literal::Date(19_782), "2024-02-29"),
            (
                DataTypes::time_with_precision(3),
                Literal::Time(3_723_004),
                "01-02-03_004",
            ),
            (
                DataTypes::timestamp_with_precision(9),
                Literal::TimestampNtz(TimestampNtz::from_millis_nanos(-1, 900_000).unwrap()),
                "1969-12-31-23-59-59_9999",
            ),
            (
                DataTypes::timestamp_ltz_with_precision(9),
                Literal::TimestampLtz(TimestampLtz::from_millis_nanos(1, 900_000).unwrap()),
                "1970-01-01-00-00-00_0019",
            ),
        ];
        for (data_type, literal, encoded) in cases {
            let datum = literal.to_datum();
            assert_eq!(
                parse_value_of_type(encoded, &data_type).unwrap(),
                datum,
                "{encoded}"
            );
            let round_trip = convert_value_of_type(&datum, &data_type).unwrap();
            let row_type = RowType::new(vec![DataField::new("p", data_type, None)]);
            for (predicate, expected) in [
                (col("p").eq(literal.clone()), true),
                (col("p").ne(literal.clone()), false),
                (col("p").is_in([literal.clone()]), true),
                (col("p").not_in([literal.clone(), Literal::Null]), false),
                (col("p").is_null(), false),
                (col("p").is_not_null(), true),
            ] {
                let bound = BoundPredicate::bind(Some(&predicate), &row_type).unwrap();
                for value in [encoded, round_trip.as_str()] {
                    assert_eq!(
                        bound.evaluate_partition_value(value).unwrap(),
                        expected,
                        "{bound:?}: {value}"
                    );
                }
            }
        }
    }

    #[test]
    fn partition_evaluation_rejects_lossy_values_and_preserves_temporal_bounds() {
        for (data_type, value) in [
            (DataTypes::tinyint(), "128"),
            (DataTypes::boolean(), "invalid"),
            (DataTypes::bytes(), "0ff"),
            (DataTypes::binary(2), "ff"),
            (DataTypes::bytes(), "é"),
            (DataTypes::date(), "2026-02-30"),
            (DataTypes::time(), "24-00-00_000"),
            (DataTypes::timestamp(), "1970-01-01-00-00-00_1234567890"),
            (
                DataTypes::timestamp_with_precision(0),
                "1970-01-01-00-00-00_0019",
            ),
            (DataTypes::timestamp_with_precision(9), "3000-01-01"),
        ] {
            let row_type = RowType::new(vec![DataField::new("p", data_type, None)]);
            let bound = BoundPredicate::bind(Some(&col("p").is_not_null()), &row_type).unwrap();
            assert!(bound.evaluate_partition_value(value).is_err(), "{value}");
        }
        for precision in [0, 3, 6, 9] {
            let row_type = RowType::new(vec![DataField::new(
                "p",
                DataTypes::timestamp_with_precision(precision),
                None,
            )]);
            let bound = BoundPredicate::bind(
                Some(&col("p").lt(TimestampNtz::from_millis_nanos(1, 900_000).unwrap())),
                &row_type,
            )
            .unwrap();
            assert!(bound.evaluate_partition_value("1970-01-01").unwrap());
            if precision > 0 {
                assert!(
                    bound
                        .evaluate_partition_value("1970-01-01-00-00-00_001")
                        .unwrap()
                );
                assert!(
                    !bound
                        .evaluate_partition_value("1970-01-01-00-00-00_002")
                        .unwrap()
                );
            }
        }
    }

    #[test]
    fn binary_comparisons_use_signed_bytes() {
        use Ordering::{Equal, Greater, Less};
        use arrow::array::BinaryArray;
        let bytes: &[Option<&[u8]>] = &[
            Some(&[0x80]),
            Some(&[0xff]),
            Some(&[0]),
            Some(&[0x7f]),
            None,
        ];
        for (data_type, array) in [
            (
                DataTypes::bytes(),
                std::sync::Arc::new(BinaryArray::from(bytes.to_vec())) as ArrayRef,
            ),
            (
                DataTypes::binary(1),
                std::sync::Arc::new(
                    FixedSizeBinaryArray::try_from_sparse_iter_with_size(bytes.iter().copied(), 1)
                        .unwrap(),
                ) as ArrayRef,
            ),
        ] {
            assert_comparisons(
                data_type,
                array,
                Literal::Bytes(vec![0]),
                vec![Some(Less), Some(Less), Some(Equal), Some(Greater), None],
            );
        }
        assert_comparisons(
            DataTypes::bytes(),
            std::sync::Arc::new(BinaryArray::from(vec![
                Some(&[][..]),
                Some(&[0][..]),
                Some(&[0, 0x80][..]),
                Some(&[0, 0][..]),
                None,
            ])),
            Literal::Bytes(vec![0, 0]),
            vec![Some(Less), Some(Less), Some(Less), Some(Equal), None],
        );
    }

    #[test]
    fn nan_signs_and_payloads_share_one_equality_class() {
        use arrow::array::{Float32Array, Float64Array};
        // Explicit Java expectations, independent of Arrow/total_cmp ordering.
        let bits = [
            0x7ff8_0000_0000_0000,
            0xfff8_0000_0000_0000,
            0x7ff8_0000_2000_0000,
            0xfff8_0000_2000_0000,
            0x7ff0_0000_2000_0000,
        ];
        for single in [false, true] {
            let (data_type, array): (_, ArrayRef) = if single {
                (
                    DataTypes::float(),
                    std::sync::Arc::new(Float32Array::from(
                        bits.iter()
                            .map(|bits| Some(f64::from_bits(*bits) as f32))
                            .chain([None])
                            .collect::<Vec<_>>(),
                    )),
                )
            } else {
                (
                    DataTypes::double(),
                    std::sync::Arc::new(Float64Array::from(
                        bits.iter()
                            .map(|bits| Some(f64::from_bits(*bits)))
                            .chain([None])
                            .collect::<Vec<_>>(),
                    )),
                )
            };
            for literal in [f64::NAN, f64::from_bits(bits[3])] {
                assert_comparisons(
                    data_type.clone(),
                    array.clone(),
                    Literal::Float64(literal),
                    vec![
                        Some(Ordering::Equal),
                        Some(Ordering::Equal),
                        Some(Ordering::Equal),
                        Some(Ordering::Equal),
                        Some(Ordering::Equal),
                        None,
                    ],
                );
            }
        }
    }

    // Independent comparison path for membership differential tests.
    fn evaluate_membership_linear(
        array: &ArrayRef,
        data_type: &FlussDataType,
        function: LeafFunction,
        literals: &[Literal],
        len: usize,
    ) -> Result<BooleanArray> {
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
        for (predicate, expected) in [
            (
                col("id").is_in(vec![Some(1_i32), None]),
                [true, false, false],
            ),
            (
                col("id").not_in(vec![Some(1_i32), None]),
                [false, false, false],
            ),
            (col("id").is_in(Vec::<i32>::new()), [false, false, false]),
            (col("id").not_in(Vec::<i32>::new()), [true, false, true]),
        ] {
            assert_eq!(evaluate(predicate), expected);
        }
    }

    #[test]
    fn manually_constructed_leaves_reject_invalid_literal_counts() {
        let batch = RecordBatch::try_from_iter([(
            "c",
            Arc::new(StringArray::from(vec![Some("a"), None])) as ArrayRef,
        )])
        .unwrap();
        let leaf = |function, literals| BoundPredicate::Leaf {
            field_index: 0,
            field_id: 0,
            field_name: "c".into(),
            data_type: DataTypes::string(),
            function,
            literals,
        };
        for function in [
            LeafFunction::Equal,
            LeafFunction::NotEqual,
            LeafFunction::LessThan,
            LeafFunction::LessOrEqual,
            LeafFunction::GreaterThan,
            LeafFunction::GreaterOrEqual,
            LeafFunction::StartsWith,
            LeafFunction::EndsWith,
            LeafFunction::Contains,
        ] {
            for literals in [
                vec![],
                vec![Literal::String("a".into()); 2],
                vec![Literal::Null],
            ] {
                let predicate = leaf(function, literals);
                assert!(matches!(
                    predicate.evaluate_batch(&batch),
                    Err(IllegalArgument { .. })
                ));
                assert!(matches!(
                    predicate.into_evaluator(),
                    Err(IllegalArgument { .. })
                ));
            }
        }
        for function in [LeafFunction::IsNull, LeafFunction::IsNotNull] {
            assert!(
                leaf(function, vec![Literal::Null])
                    .evaluate_batch(&batch)
                    .is_err()
            );
        }
    }

    fn assert_membership_strategies(
        data_type: FlussDataType,
        array: ArrayRef,
        literals: Vec<Literal>,
    ) {
        let row_type = RowType::new(vec![DataField::new("c", data_type.clone(), None)]);
        let batch = RecordBatch::try_from_iter([("c", array.clone())]).unwrap();
        for function in [LeafFunction::In, LeafFunction::NotIn] {
            for with_null in [false, true] {
                let mut literals = literals.clone();
                literals.extend(literals.clone());
                if with_null {
                    literals.push(Literal::Null);
                }
                let predicate = Predicate::Leaf {
                    field: "c".into(),
                    function,
                    literals,
                };
                let bound = BoundPredicate::bind(Some(&predicate), &row_type).unwrap();
                let BoundPredicate::Leaf { literals, .. } = &bound else {
                    unreachable!()
                };
                let expected = if function == LeafFunction::NotIn && with_null {
                    BooleanArray::from(vec![false; array.len()])
                } else {
                    evaluate_membership_linear(&array, &data_type, function, literals, array.len())
                        .unwrap()
                };
                assert_eq!(bound.evaluate_batch(&batch).unwrap(), expected);
                let evaluator = bound.into_evaluator().unwrap();
                assert_eq!(evaluator.evaluate_batch(&batch).unwrap(), expected);
                assert_eq!(
                    evaluator.evaluate_batch(&batch.slice(0, 0)).unwrap().len(),
                    0
                );
            }
        }
    }

    #[test]
    fn hashed_membership_matches_linear_comparison_for_scalar_types() {
        use arrow::array::{BinaryArray, Float64Array};
        let nan = f64::from_bits(f64::NAN.to_bits() + (1 << 30));
        for single in [false, true] {
            let values = vec![
                Some(-0.0),
                Some(0.0),
                Some(f64::NAN),
                Some(-f64::NAN),
                Some(nan),
                Some(1.0),
                Some(2.0),
                None,
            ];
            let data_type = if single {
                DataTypes::float()
            } else {
                DataTypes::double()
            };
            let array = cast(
                &Float64Array::from(values),
                &to_arrow_type(&data_type).unwrap(),
            )
            .unwrap();
            let literals = [-0.0, nan, 1.0]
                .into_iter()
                .map(|v| {
                    if single {
                        Literal::Float32(v as f32)
                    } else {
                        Literal::Float64(v)
                    }
                })
                .collect();
            assert_membership_strategies(data_type, array, literals);
        }
        assert_membership_strategies(
            DataTypes::int(),
            Arc::new(Int32Array::from(vec![Some(-1), Some(0), Some(1), None])),
            vec![Literal::Int32(-1), Literal::Int32(0)],
        );
        assert_membership_strategies(
            DataTypes::string(),
            Arc::new(StringArray::from(vec![
                Some(""),
                Some("a"),
                Some("ab"),
                Some("你好"),
                None,
            ])),
            vec![Literal::String("a".into()), Literal::String("你好".into())],
        );
        assert_membership_strategies(
            DataTypes::bytes(),
            Arc::new(BinaryArray::from(vec![
                Some(b"a".as_slice()),
                Some(b"ab".as_slice()),
                None,
            ])),
            vec![Literal::Bytes(b"a".to_vec())],
        );
        assert_membership_strategies(
            DataTypes::binary(2),
            Arc::new(
                FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                    [Some(b"ab".as_slice()), Some(b"cd".as_slice()), None].into_iter(),
                    2,
                )
                .unwrap(),
            ),
            vec![Literal::Bytes(b"ab".to_vec())],
        );
        let values = vec![Some(-(10_i128.pow(38) - 1)), Some(123), Some(124), None];
        assert_membership_strategies(
            DataTypes::decimal(38, 2),
            Arc::new(
                Decimal128Array::from(values)
                    .with_precision_and_scale(38, 2)
                    .unwrap(),
            ),
            vec![Literal::Decimal(
                crate::row::Decimal::from_unscaled_bytes(&123_i128.to_be_bytes(), 38, 2).unwrap(),
            )],
        );
    }

    #[test]
    fn prepared_compound_reuses_membership_and_validates_each_batch() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PredicateEvaluator>();
        let predicate = col("id").is_in(0..128).and(col("name").is_not_null());
        let bound = BoundPredicate::bind(Some(&predicate), &row_type()).unwrap();
        let evaluator = bound.clone().into_evaluator().unwrap();
        for input in [batch(), batch().slice(1, 2), batch().slice(0, 0)] {
            assert_eq!(
                evaluator.evaluate_batch(&input).unwrap(),
                bound.evaluate_batch(&input).unwrap()
            );
        }
        let missing = RecordBatch::try_from_iter([("name", batch().column(1).clone())]).unwrap();
        let wrong = RecordBatch::try_from_iter([
            ("id", Arc::new(Int64Array::from(vec![1])) as ArrayRef),
            ("name", Arc::new(StringArray::from(vec!["a"])) as ArrayRef),
        ])
        .unwrap();
        for invalid in [missing, wrong] {
            assert!(bound.evaluate_batch(&invalid).is_err());
            assert!(evaluator.evaluate_batch(&invalid).is_err());
        }
    }

    #[test]
    fn wire_binary_literals_do_not_require_arrow_width() {
        let data_type = DataTypes::binary(4);
        let row_type = RowType::new(vec![DataField::with_field_id(
            "c",
            data_type.clone(),
            None,
            7,
        )]);
        let predicate = col("c").eq(Literal::Bytes(vec![1, 2, 3]));
        // The wire encoder accepts variable-width literals; Arrow evaluation
        // must still validate the fixed-size binary width, even on empty input.
        to_pb_predicate(&predicate, &row_type).unwrap();
        let bound = BoundPredicate::bind(Some(&predicate), &row_type).unwrap();
        let batch = RecordBatch::try_from_iter([(
            "c",
            new_empty_array(&to_arrow_type(&data_type).unwrap()),
        )])
        .unwrap();
        assert!(bound.evaluate_batch(&batch).is_err());
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
            (DataTypes::binary(4), Literal::Bytes(vec![1, 2, 3, 4])),
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
            assert_eq!(
                compare(
                    &array.slice(0, 0),
                    LeafFunction::Equal,
                    &literal,
                    &data_type
                )
                .unwrap()
                .len(),
                0
            );
        }
    }

    #[test]
    fn temporal_comparisons_match_exact_nanosecond_ordering() {
        for (precision, millis, nanos) in [
            (0_u32, 1_500, 0),
            (0, -1_500, 0),
            (1, 1, 1),
            (3, -1, 999_999),
            (3, i64::from(i32::MIN), 0),
            (6, 1, 1),
            (6, -9_223_372_036_854_776, 192_000),
            (6, i64::MAX, 0),
            (9, 1, 1),
            (9, i64::from(i32::MAX), 0),
            (9, -9_223_372_036_855, 224_191),
            (9, -9_223_372_036_855, 224_192),
            (9, 9_223_372_036_854, 775_807),
            (9, 9_223_372_036_854, 775_808),
        ] {
            let tick_nanos = 10_i128.pow(9 - precision.div_ceil(3) * 3);
            let mut cases = vec![
                (
                    DataTypes::timestamp_with_precision(precision),
                    Literal::TimestampNtz(TimestampNtz::from_millis_nanos(millis, nanos).unwrap()),
                ),
                (
                    DataTypes::timestamp_ltz_with_precision(precision),
                    Literal::TimestampLtz(TimestampLtz::from_millis_nanos(millis, nanos).unwrap()),
                ),
            ];
            if nanos == 0 {
                if let Ok(millis) = i32::try_from(millis) {
                    cases.push((
                        DataTypes::time_with_precision(precision),
                        Literal::Time(millis),
                    ));
                }
            }
            let literal_nanos = i128::from(millis) * 1_000_000 + i128::from(nanos);
            for (data_type, literal) in cases {
                let arrow_type = to_arrow_type(&data_type).unwrap();
                let time32 = matches!(arrow_type, ArrowDataType::Time32(_));
                let (min, max) = if time32 {
                    (i128::from(i32::MIN), i128::from(i32::MAX))
                } else {
                    (i128::from(i64::MIN), i128::from(i64::MAX))
                };
                let near = literal_nanos / tick_nanos;
                let values: Vec<_> = [min, max, -2, -1, 0, 1, 2, near - 1, near, near + 1]
                    .into_iter()
                    .filter(|v| (min..=max).contains(v))
                    .map(|v| Some(v as i64))
                    .chain([None])
                    .collect();
                let integers: ArrayRef = if time32 {
                    Arc::new(Int32Array::from(
                        values
                            .iter()
                            .map(|v| v.map(|v| v as i32))
                            .collect::<Vec<_>>(),
                    ))
                } else {
                    Arc::new(Int64Array::from(values.clone()))
                };
                let array = cast(integers.as_ref(), &arrow_type).unwrap();
                // Compare in the exact nanosecond domain, independently of the rewrite.
                let orderings = values
                    .iter()
                    .map(|v| v.map(|v| (i128::from(v) * tick_nanos).cmp(&literal_nanos)))
                    .collect();
                assert_comparisons(data_type, array, literal, orderings);
            }
        }
    }

    #[test]
    fn temporal_membership_ignores_unrepresentable_literals() {
        let data_type = DataTypes::timestamp_with_precision(0);
        let row_type = RowType::new(vec![DataField::new("c", data_type.clone(), None)]);
        let array = cast(
            &Int64Array::from(vec![Some(1), None, Some(2)]),
            &to_arrow_type(&data_type).unwrap(),
        )
        .unwrap();
        let batch = RecordBatch::try_from_iter([("c", array)]).unwrap();
        let literals = vec![
            Literal::TimestampNtz(TimestampNtz::new(1_500)),
            Literal::TimestampNtz(TimestampNtz::new(2_000)),
        ];
        let mut with_null = literals.clone();
        with_null.push(Literal::Null);
        for (predicate, expected) in [
            (col("c").is_in(literals.clone()), [false, false, true]),
            (col("c").not_in(literals), [true, false, false]),
            (col("c").is_in(with_null.clone()), [false, false, true]),
            (col("c").not_in(with_null), [false, false, false]),
        ] {
            let mask = BoundPredicate::bind(Some(&predicate), &row_type)
                .unwrap()
                .evaluate_batch(&batch)
                .unwrap();
            assert_eq!(mask.iter().collect::<Vec<_>>(), expected.map(Some));
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
