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

//! Engine-neutral predicate DSL for UnionRead.
//!
//! Predicates are column-name-based and remain unresolved until planning
//! binds them to the table schema. The DSL is intentionally smaller than a
//! full SQL expression language: it supports the comparisons, null checks,
//! and in-list filters that UnionRead can evaluate exactly as a residual
//! filter and/or use for partition/bucket pruning.

use crate::FlussLakeError;
use arrow::array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Date32Array, Float64Array, Int32Array, Int64Array,
    StringArray, TimestampMicrosecondArray, new_null_array,
};
use arrow::compute::kernels::boolean::{and, is_null, not, or};
use arrow::compute::kernels::cmp::{eq, gt, gt_eq, lt, lt_eq, neq};
use arrow::datatypes::{DataType, TimeUnit};
use arrow::record_batch::RecordBatch;
use fluss::metadata::RowType;
use fluss::predicate::{
    ComparisonOperator as PruningComparisonOp, FieldRef, NullCheckOperator, PruningLiteral,
    PruningPredicate,
};
use std::collections::HashSet;
use std::sync::Arc;

/// Comparison operators supported by UnionRead predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlussLakeComparisonOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

/// A literal value used in a UnionRead predicate.
#[derive(Debug, Clone, PartialEq)]
pub enum FlussLakeLiteral {
    Null,
    Boolean(bool),
    Int32(i32),
    Int64(i64),
    Float64(f64),
    String(String),
    Binary(Vec<u8>),
    Timestamp(i64),
    Date(i32),
}

impl From<bool> for FlussLakeLiteral {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i32> for FlussLakeLiteral {
    fn from(value: i32) -> Self {
        Self::Int32(value)
    }
}

impl From<i64> for FlussLakeLiteral {
    fn from(value: i64) -> Self {
        Self::Int64(value)
    }
}

impl From<f64> for FlussLakeLiteral {
    fn from(value: f64) -> Self {
        Self::Float64(value)
    }
}

impl From<String> for FlussLakeLiteral {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for FlussLakeLiteral {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<Vec<u8>> for FlussLakeLiteral {
    fn from(value: Vec<u8>) -> Self {
        Self::Binary(value)
    }
}

/// Engine-neutral predicate that can be pushed into UnionRead planning.
#[derive(Debug, Clone, PartialEq)]
pub enum FlussLakePredicate {
    AlwaysTrue,
    AlwaysFalse,
    And(Vec<FlussLakePredicate>),
    Or(Vec<FlussLakePredicate>),
    Not(Box<FlussLakePredicate>),
    IsNull {
        column: String,
        negated: bool,
    },
    In {
        column: String,
        values: Vec<FlussLakeLiteral>,
        negated: bool,
    },
    Comparison {
        column: String,
        op: FlussLakeComparisonOp,
        value: FlussLakeLiteral,
    },
}

impl FlussLakePredicate {
    /// A predicate that is always true.
    pub fn always_true() -> Self {
        Self::AlwaysTrue
    }

    /// A predicate that is always false.
    pub fn always_false() -> Self {
        Self::AlwaysFalse
    }

    /// `column IS NULL`.
    pub fn is_null(column: impl Into<String>) -> Self {
        Self::IsNull {
            column: column.into(),
            negated: false,
        }
    }

    /// `column IS NOT NULL`.
    pub fn is_not_null(column: impl Into<String>) -> Self {
        Self::IsNull {
            column: column.into(),
            negated: true,
        }
    }

    /// `column = value`.
    pub fn eq(column: impl Into<String>, value: impl Into<FlussLakeLiteral>) -> Self {
        Self::Comparison {
            column: column.into(),
            op: FlussLakeComparisonOp::Eq,
            value: value.into(),
        }
    }

    /// `column != value`.
    pub fn not_eq(column: impl Into<String>, value: impl Into<FlussLakeLiteral>) -> Self {
        Self::Comparison {
            column: column.into(),
            op: FlussLakeComparisonOp::NotEq,
            value: value.into(),
        }
    }

    /// `column < value`.
    pub fn lt(column: impl Into<String>, value: impl Into<FlussLakeLiteral>) -> Self {
        Self::Comparison {
            column: column.into(),
            op: FlussLakeComparisonOp::Lt,
            value: value.into(),
        }
    }

    /// `column <= value`.
    pub fn le(column: impl Into<String>, value: impl Into<FlussLakeLiteral>) -> Self {
        Self::Comparison {
            column: column.into(),
            op: FlussLakeComparisonOp::LtEq,
            value: value.into(),
        }
    }

    /// `column > value`.
    pub fn gt(column: impl Into<String>, value: impl Into<FlussLakeLiteral>) -> Self {
        Self::Comparison {
            column: column.into(),
            op: FlussLakeComparisonOp::Gt,
            value: value.into(),
        }
    }

    /// `column >= value`.
    pub fn ge(column: impl Into<String>, value: impl Into<FlussLakeLiteral>) -> Self {
        Self::Comparison {
            column: column.into(),
            op: FlussLakeComparisonOp::GtEq,
            value: value.into(),
        }
    }

    /// `column IN (values...)`.
    pub fn in_list(
        column: impl Into<String>,
        values: impl IntoIterator<Item = impl Into<FlussLakeLiteral>>,
    ) -> Self {
        Self::In {
            column: column.into(),
            values: values.into_iter().map(Into::into).collect(),
            negated: false,
        }
    }

    /// `column NOT IN (values...)`.
    pub fn not_in_list(
        column: impl Into<String>,
        values: impl IntoIterator<Item = impl Into<FlussLakeLiteral>>,
    ) -> Self {
        Self::In {
            column: column.into(),
            values: values.into_iter().map(Into::into).collect(),
            negated: true,
        }
    }

    /// Combines predicates with AND, flattening nested ANDs and discarding
    /// `AlwaysTrue`.
    pub fn and(predicates: impl IntoIterator<Item = Self>) -> Self {
        let mut flattened = Vec::new();
        for predicate in predicates {
            match predicate {
                Self::AlwaysTrue => {}
                Self::And(children) => flattened.extend(children),
                other => flattened.push(other),
            }
        }
        match flattened.len() {
            0 => Self::AlwaysTrue,
            1 => flattened.into_iter().next().expect("len is 1"),
            _ => Self::And(flattened),
        }
    }

    /// Combines predicates with OR, flattening nested ORs and discarding
    /// `AlwaysFalse`.
    pub fn or(predicates: impl IntoIterator<Item = Self>) -> Self {
        let mut flattened = Vec::new();
        for predicate in predicates {
            match predicate {
                Self::AlwaysFalse => {}
                Self::Or(children) => flattened.extend(children),
                other => flattened.push(other),
            }
        }
        match flattened.len() {
            0 => Self::AlwaysFalse,
            1 => flattened.into_iter().next().expect("len is 1"),
            _ => Self::Or(flattened),
        }
    }

    /// Negates this predicate, applying De Morgan's laws where possible.
    pub fn negate(self) -> Self {
        match self {
            Self::AlwaysTrue => Self::AlwaysFalse,
            Self::AlwaysFalse => Self::AlwaysTrue,
            Self::Not(inner) => *inner,
            Self::And(children) => Self::Or(children.into_iter().map(Self::negate).collect()),
            Self::Or(children) => Self::And(children.into_iter().map(Self::negate).collect()),
            Self::IsNull { column, negated } => Self::IsNull {
                column,
                negated: !negated,
            },
            Self::In {
                column,
                values,
                negated,
            } => Self::In {
                column,
                values,
                negated: !negated,
            },
            Self::Comparison { column, op, value } => Self::Comparison {
                column,
                op: op.negate(),
                value,
            },
        }
    }

    /// Converts this predicate into Fluss's internal pruning model using the
    /// resolved table schema.
    ///
    /// Returns `None` when the predicate contains constructs that cannot be
    /// expressed as a pruning predicate. The caller must still evaluate the
    /// original predicate as a residual filter regardless of the return value.
    pub(crate) fn to_pruning_predicate(&self, row_type: &RowType) -> Option<PruningPredicate> {
        match self {
            Self::AlwaysTrue | Self::IsNull { negated: true, .. } => None,
            // Empty disjunction is logically false, which lets the pruner
            // drop every partition when the engine filter is unsatisfiable.
            Self::AlwaysFalse => Some(PruningPredicate::Or(Vec::new())),
            Self::And(children) => {
                let pruned: Vec<PruningPredicate> = children
                    .iter()
                    .filter_map(|child| child.to_pruning_predicate(row_type))
                    .collect();
                if pruned.is_empty() {
                    None
                } else {
                    Some(PruningPredicate::And(pruned))
                }
            }
            Self::Or(children) => {
                let mut pruned = Vec::with_capacity(children.len());
                for child in children {
                    // Dropping one unsupported OR branch is unsafe: that
                    // branch may be true for a partition rejected by the
                    // remaining branches. Disable pruning for the whole OR.
                    pruned.push(child.to_pruning_predicate(row_type)?);
                }
                Some(PruningPredicate::Or(pruned))
            }
            Self::Not(inner) => inner
                .as_ref()
                .clone()
                .negate()
                .to_pruning_predicate(row_type),
            Self::In { negated: true, .. } => None,
            Self::IsNull {
                column,
                negated: false,
            } => {
                let field = field_ref_by_name(row_type, column)?;
                Some(PruningPredicate::null_check(
                    NullCheckOperator::IsNull,
                    field,
                ))
            }
            Self::In {
                column,
                values,
                negated: false,
            } => {
                let field = field_ref_by_name(row_type, column)?;
                let pruning_literals: Vec<PruningLiteral> = values
                    .iter()
                    .filter_map(FlussLakeLiteral::to_pruning)
                    .collect();
                if pruning_literals.is_empty() {
                    None
                } else {
                    Some(PruningPredicate::in_list(field, pruning_literals))
                }
            }
            Self::Comparison { column, op, value } => {
                let field = field_ref_by_name(row_type, column)?;
                let pruning_literal = value.to_pruning()?;
                Some(PruningPredicate::comparison(
                    op.to_pruning(),
                    field,
                    pruning_literal,
                ))
            }
        }
    }

    /// Evaluates this predicate against an Arrow record batch.
    ///
    /// A predicate accepted by planning must remain exactly executable.
    /// Missing physical columns and incompatible literal types are therefore
    /// execution errors rather than a reason to pass rows through.
    pub(crate) fn evaluate_batch(
        &self,
        batch: &RecordBatch,
    ) -> Result<BooleanArray, FlussLakeError> {
        let num_rows = batch.num_rows();
        match self {
            Self::AlwaysTrue => Ok(BooleanArray::from(vec![true; num_rows])),
            Self::AlwaysFalse => Ok(BooleanArray::from(vec![false; num_rows])),
            Self::And(children) => {
                let mut result = BooleanArray::from(vec![true; num_rows]);
                for child in children {
                    let child_mask = child.evaluate_batch(batch)?;
                    result = and(&result, &child_mask).map_err(predicate_arrow_error)?;
                }
                Ok(result)
            }
            Self::Or(children) => {
                let mut result = BooleanArray::from(vec![false; num_rows]);
                for child in children {
                    let child_mask = child.evaluate_batch(batch)?;
                    result = or(&result, &child_mask).map_err(predicate_arrow_error)?;
                }
                Ok(result)
            }
            Self::Not(inner) => not(&inner.evaluate_batch(batch)?).map_err(predicate_arrow_error),
            Self::IsNull { column, negated } => {
                let array = required_column(batch, column)?;
                let mask = is_null(array.as_ref()).map_err(predicate_arrow_error)?;
                if *negated {
                    not(&mask).map_err(predicate_arrow_error)
                } else {
                    Ok(mask)
                }
            }
            Self::In {
                column,
                values,
                negated,
            } => {
                if values.is_empty() {
                    return Ok(BooleanArray::from(vec![*negated; num_rows]));
                }
                let mut result = BooleanArray::from(vec![false; num_rows]);
                for value in values {
                    let eq_mask =
                        compare_column_literal(batch, column, FlussLakeComparisonOp::Eq, value)?;
                    result = or(&result, &eq_mask).map_err(predicate_arrow_error)?;
                }
                if *negated {
                    not(&result).map_err(predicate_arrow_error)
                } else {
                    Ok(result)
                }
            }
            Self::Comparison { column, op, value } => {
                compare_column_literal(batch, column, *op, value)
            }
        }
    }
}

fn compare_column_literal(
    batch: &RecordBatch,
    column: &str,
    op: FlussLakeComparisonOp,
    value: &FlussLakeLiteral,
) -> Result<BooleanArray, FlussLakeError> {
    let array = required_column(batch, column)?;
    let constant =
        literal_to_constant_array(value, array.data_type().clone(), batch.num_rows()).ok_or_else(
            || {
                FlussLakeError::SchemaIncompatible(format!(
                    "predicate literal {value:?} cannot be evaluated against column '{column}' of Arrow type {}",
                    array.data_type()
                ))
            },
        )?;
    let array_datum: &dyn Array = array.as_ref();
    let constant_datum: &dyn Array = constant.as_ref();
    let result = match op {
        FlussLakeComparisonOp::Eq => eq(&array_datum, &constant_datum),
        FlussLakeComparisonOp::NotEq => neq(&array_datum, &constant_datum),
        FlussLakeComparisonOp::Lt => lt(&array_datum, &constant_datum),
        FlussLakeComparisonOp::LtEq => lt_eq(&array_datum, &constant_datum),
        FlussLakeComparisonOp::Gt => gt(&array_datum, &constant_datum),
        FlussLakeComparisonOp::GtEq => gt_eq(&array_datum, &constant_datum),
    };
    result.map_err(predicate_arrow_error)
}

fn literal_to_constant_array(
    literal: &FlussLakeLiteral,
    data_type: DataType,
    len: usize,
) -> Option<ArrayRef> {
    match (literal, data_type) {
        (FlussLakeLiteral::Null, data_type) => Some(new_null_array(&data_type, len)),
        (FlussLakeLiteral::Boolean(value), DataType::Boolean) => {
            Some(Arc::new(BooleanArray::from(vec![*value; len])))
        }
        (FlussLakeLiteral::Int32(value), DataType::Int32) => {
            Some(Arc::new(Int32Array::from(vec![*value; len])))
        }
        (FlussLakeLiteral::Int32(value), DataType::Int64) => {
            Some(Arc::new(Int64Array::from(vec![i64::from(*value); len])))
        }
        (FlussLakeLiteral::Int64(value), DataType::Int64) => {
            Some(Arc::new(Int64Array::from(vec![*value; len])))
        }
        (FlussLakeLiteral::Float64(value), DataType::Float64) => {
            Some(Arc::new(Float64Array::from(vec![*value; len])))
        }
        (FlussLakeLiteral::String(value), DataType::Utf8) => {
            Some(Arc::new(StringArray::from(vec![value.as_str(); len])))
        }
        (FlussLakeLiteral::Binary(value), DataType::Binary) => {
            Some(Arc::new(BinaryArray::from(vec![value.as_slice(); len])))
        }
        (
            FlussLakeLiteral::Timestamp(value),
            DataType::Timestamp(TimeUnit::Microsecond, timezone),
        ) => Some(Arc::new(
            TimestampMicrosecondArray::from(vec![*value; len]).with_timezone_opt(timezone),
        )),
        (FlussLakeLiteral::Date(value), DataType::Date32) => {
            Some(Arc::new(Date32Array::from(vec![*value; len])))
        }
        _ => None,
    }
}

impl FlussLakePredicate {
    /// Returns every referenced field index once, in expression order.
    pub(crate) fn referenced_field_indexes(
        &self,
        row_type: &RowType,
    ) -> Result<Vec<usize>, FlussLakeError> {
        let mut indexes = Vec::new();
        let mut seen = HashSet::new();
        self.collect_referenced_field_indexes(row_type, &mut indexes, &mut seen)?;
        Ok(indexes)
    }

    fn collect_referenced_field_indexes(
        &self,
        row_type: &RowType,
        indexes: &mut Vec<usize>,
        seen: &mut HashSet<usize>,
    ) -> Result<(), FlussLakeError> {
        match self {
            Self::AlwaysTrue | Self::AlwaysFalse => Ok(()),
            Self::And(children) | Self::Or(children) => {
                for child in children {
                    child.collect_referenced_field_indexes(row_type, indexes, seen)?;
                }
                Ok(())
            }
            Self::Not(inner) => inner.collect_referenced_field_indexes(row_type, indexes, seen),
            Self::IsNull { column, .. }
            | Self::In { column, .. }
            | Self::Comparison { column, .. } => {
                let field = field_ref_by_name(row_type, column).ok_or_else(|| {
                    FlussLakeError::InvalidRequest(format!(
                        "predicate references unknown column '{column}'"
                    ))
                })?;
                if seen.insert(field.index()) {
                    indexes.push(field.index());
                }
                Ok(())
            }
        }
    }

    /// Validates referenced columns and exact Arrow literal execution.
    pub(crate) fn validate_columns(&self, row_type: &RowType) -> Result<(), FlussLakeError> {
        let arrow_schema = fluss::record::to_arrow_schema(row_type).map_err(|error| {
            FlussLakeError::InvalidRequest(format!(
                "failed to resolve predicate column types: {error}"
            ))
        })?;
        match self {
            Self::AlwaysTrue | Self::AlwaysFalse => Ok(()),
            Self::And(children) | Self::Or(children) => {
                for child in children {
                    child.validate_columns(row_type)?;
                }
                Ok(())
            }
            Self::Not(inner) => inner.validate_columns(row_type),
            Self::IsNull { column, .. } => {
                field_ref_by_name(row_type, column).ok_or_else(|| {
                    FlussLakeError::InvalidRequest(format!(
                        "predicate references unknown column '{column}'"
                    ))
                })?;
                Ok(())
            }
            Self::In { column, values, .. } => {
                let field = arrow_schema.field_with_name(column).map_err(|_| {
                    FlussLakeError::InvalidRequest(format!(
                        "predicate references unknown column '{column}'"
                    ))
                })?;
                for value in values {
                    validate_literal_type(column, field.data_type(), value)?;
                }
                Ok(())
            }
            Self::Comparison { column, value, .. } => {
                let field = arrow_schema.field_with_name(column).map_err(|_| {
                    FlussLakeError::InvalidRequest(format!(
                        "predicate references unknown column '{column}'"
                    ))
                })?;
                validate_literal_type(column, field.data_type(), value)
            }
        }
    }
}

fn required_column<'a>(
    batch: &'a RecordBatch,
    column: &str,
) -> Result<&'a ArrayRef, FlussLakeError> {
    batch.column_by_name(column).ok_or_else(|| {
        FlussLakeError::SchemaIncompatible(format!(
            "physical UnionRead batch is missing predicate column '{column}'"
        ))
    })
}

fn validate_literal_type(
    column: &str,
    data_type: &DataType,
    literal: &FlussLakeLiteral,
) -> Result<(), FlussLakeError> {
    let supported = matches!(
        (literal, data_type),
        (FlussLakeLiteral::Null, _)
            | (FlussLakeLiteral::Boolean(_), DataType::Boolean)
            | (
                FlussLakeLiteral::Int32(_),
                DataType::Int32 | DataType::Int64
            )
            | (FlussLakeLiteral::Int64(_), DataType::Int64)
            | (FlussLakeLiteral::Float64(_), DataType::Float64)
            | (FlussLakeLiteral::String(_), DataType::Utf8)
            | (FlussLakeLiteral::Binary(_), DataType::Binary)
            | (
                FlussLakeLiteral::Timestamp(_),
                DataType::Timestamp(TimeUnit::Microsecond, _)
            )
            | (FlussLakeLiteral::Date(_), DataType::Date32)
    );
    if supported {
        Ok(())
    } else {
        Err(FlussLakeError::InvalidRequest(format!(
            "predicate literal {literal:?} is not compatible with column '{column}' of Arrow type {data_type}"
        )))
    }
}

fn predicate_arrow_error(error: arrow::error::ArrowError) -> FlussLakeError {
    FlussLakeError::Execution(format!("failed to evaluate UnionRead predicate: {error}"))
}

impl FlussLakeComparisonOp {
    fn negate(self) -> Self {
        match self {
            Self::Eq => Self::NotEq,
            Self::NotEq => Self::Eq,
            Self::Lt => Self::GtEq,
            Self::LtEq => Self::Gt,
            Self::Gt => Self::LtEq,
            Self::GtEq => Self::Lt,
        }
    }

    fn to_pruning(self) -> PruningComparisonOp {
        match self {
            Self::Eq => PruningComparisonOp::Equal,
            Self::NotEq => PruningComparisonOp::NotEqual,
            Self::Lt => PruningComparisonOp::LessThan,
            Self::LtEq => PruningComparisonOp::LessThanOrEqual,
            Self::Gt => PruningComparisonOp::GreaterThan,
            Self::GtEq => PruningComparisonOp::GreaterThanOrEqual,
        }
    }
}

impl FlussLakeLiteral {
    fn to_pruning(&self) -> Option<PruningLiteral> {
        Some(match self {
            Self::Null => return None,
            Self::Boolean(value) => PruningLiteral::Boolean(*value),
            Self::Int32(value) => PruningLiteral::Int32(*value),
            Self::Int64(value) => PruningLiteral::Int64(*value),
            Self::Float64(value) => PruningLiteral::Float64((*value).into()),
            Self::String(value) => PruningLiteral::String(value.clone()),
            Self::Binary(value) => PruningLiteral::Bytes(value.clone()),
            Self::Timestamp(value) => PruningLiteral::Int64(*value),
            Self::Date(value) => PruningLiteral::Int32(*value),
        })
    }
}

fn field_ref_by_name(row_type: &RowType, column: &str) -> Option<FieldRef> {
    let (field_index, field) = row_type
        .fields()
        .iter()
        .enumerate()
        .find(|(_, field)| field.name() == column)?;
    Some(FieldRef::new(
        field_index,
        field.name(),
        field.data_type().clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluss::metadata::{DataField, DataTypes};

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("name", DataTypes::string(), None),
        ])
    }

    #[test]
    fn predicate_combinators_flatten_and_negate() {
        let p = FlussLakePredicate::and([
            FlussLakePredicate::eq("id", 1_i32),
            FlussLakePredicate::or([
                FlussLakePredicate::eq("name", "a"),
                FlussLakePredicate::eq("name", "b"),
            ]),
        ]);
        let not_p = p.clone().negate();
        assert!(matches!(not_p, FlussLakePredicate::Or(_)));
        assert_eq!(not_p.negate(), p);
    }

    #[test]
    fn to_pruning_ignores_unsupported_constructs() {
        let row_type = row_type();
        assert!(
            FlussLakePredicate::always_true()
                .to_pruning_predicate(&row_type)
                .is_none()
        );
        assert!(
            FlussLakePredicate::is_not_null("id")
                .to_pruning_predicate(&row_type)
                .is_none()
        );
        assert!(
            FlussLakePredicate::eq("missing", 1_i32)
                .to_pruning_predicate(&row_type)
                .is_none()
        );
        assert!(
            FlussLakePredicate::eq("id", 1_i32)
                .to_pruning_predicate(&row_type)
                .is_some()
        );
        assert!(
            FlussLakePredicate::or([
                FlussLakePredicate::eq("name", "US"),
                FlussLakePredicate::is_not_null("id"),
            ])
            .to_pruning_predicate(&row_type)
            .is_none(),
            "an unsupported OR branch must disable pruning for the entire disjunction"
        );
    }

    #[test]
    fn validates_referenced_columns() {
        let row_type = row_type();
        FlussLakePredicate::eq("id", 1_i32)
            .validate_columns(&row_type)
            .unwrap();
        assert!(
            FlussLakePredicate::eq("missing", 1_i32)
                .validate_columns(&row_type)
                .is_err()
        );
    }

    fn test_batch() -> RecordBatch {
        RecordBatch::try_from_iter(vec![
            (
                "id",
                Arc::new(Int32Array::from(vec![1, 2, 3, 4])) as ArrayRef,
            ),
            (
                "name",
                Arc::new(StringArray::from(vec!["a", "b", "c", "d"])) as ArrayRef,
            ),
        ])
        .unwrap()
    }

    fn mask_values(mask: &BooleanArray) -> Vec<bool> {
        (0..mask.len()).map(|index| mask.value(index)).collect()
    }

    #[test]
    fn evaluate_comparison_on_int_column() {
        let batch = test_batch();
        let mask = FlussLakePredicate::gt("id", 2_i32)
            .evaluate_batch(&batch)
            .unwrap();
        assert_eq!(mask_values(&mask), vec![false, false, true, true]);
    }

    #[test]
    fn evaluate_in_list_on_string_column() {
        let batch = test_batch();
        let mask = FlussLakePredicate::in_list("name", ["a", "c"])
            .evaluate_batch(&batch)
            .unwrap();
        assert_eq!(mask_values(&mask), vec![true, false, true, false]);
    }

    #[test]
    fn evaluate_and_combines_child_masks() {
        let batch = test_batch();
        let mask = FlussLakePredicate::and([
            FlussLakePredicate::gt("id", 1_i32),
            FlussLakePredicate::in_list("name", ["b", "c"]),
        ])
        .evaluate_batch(&batch)
        .unwrap();
        assert_eq!(mask_values(&mask), vec![false, true, true, false]);
    }

    #[test]
    fn evaluate_not_negates_mask() {
        let batch = test_batch();
        let mask = FlussLakePredicate::eq("id", 1_i32)
            .negate()
            .evaluate_batch(&batch)
            .unwrap();
        assert_eq!(mask_values(&mask), vec![false, true, true, true]);
    }

    #[test]
    fn evaluate_is_null_on_non_nullable_column() {
        let batch = test_batch();
        let mask = FlussLakePredicate::is_null("id")
            .evaluate_batch(&batch)
            .unwrap();
        assert_eq!(mask_values(&mask), vec![false, false, false, false]);
    }

    #[test]
    fn evaluate_always_false_drops_all_rows() {
        let batch = test_batch();
        let mask = FlussLakePredicate::always_false()
            .evaluate_batch(&batch)
            .unwrap();
        assert_eq!(mask_values(&mask), vec![false, false, false, false]);
    }

    #[test]
    fn evaluate_missing_column_returns_schema_error() {
        let batch = test_batch();
        assert!(
            FlussLakePredicate::eq("missing", 1_i32)
                .evaluate_batch(&batch)
                .is_err()
        );
    }

    #[test]
    fn evaluates_binary_date_and_timestamp_literals_exactly() {
        let batch = RecordBatch::try_from_iter(vec![
            (
                "payload",
                Arc::new(BinaryArray::from(vec![b"a".as_slice(), b"b".as_slice()])) as ArrayRef,
            ),
            ("day", Arc::new(Date32Array::from(vec![10, 11])) as ArrayRef),
            (
                "event_time",
                Arc::new(TimestampMicrosecondArray::from(vec![100, 200])) as ArrayRef,
            ),
        ])
        .unwrap();
        let predicate = FlussLakePredicate::and([
            FlussLakePredicate::eq("payload", vec![b'b']),
            FlussLakePredicate::eq("day", FlussLakeLiteral::Date(11)),
            FlussLakePredicate::eq("event_time", FlussLakeLiteral::Timestamp(200)),
        ]);

        let mask = predicate.evaluate_batch(&batch).unwrap();

        assert_eq!(mask_values(&mask), vec![false, true]);
    }
}
