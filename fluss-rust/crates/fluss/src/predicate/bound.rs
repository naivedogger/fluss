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

//! Schema binding and literal coercion shared by predicate consumers.

use crate::error::Error::IllegalArgument;
use crate::error::Result;
use crate::metadata::{DataField, DataType, RowType};
use crate::predicate::{CompoundFunction, LeafFunction, Literal, Predicate};
use crate::row::{Date, Datum, Decimal, Time};
use std::collections::HashSet;

/// A core literal normalized for a bound predicate.
///
/// Binding checks literal kinds and exact numeric conversions against the
/// Fluss column type. Consumers validate their physical representations.
/// Reuse the core literal enum instead of defining another value model.
#[doc(hidden)]
pub type BoundLiteral = Literal;

impl Literal {
    /// Returns whether this is a null literal.
    #[doc(hidden)]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Returns an integer representation useful for conservative pruning.
    #[doc(hidden)]
    pub fn as_integer(&self) -> Option<i128> {
        match self {
            Self::Int8(value) => Some(i128::from(*value)),
            Self::Int16(value) => Some(i128::from(*value)),
            Self::Int32(value) | Self::Date(value) | Self::Time(value) => Some(i128::from(*value)),
            Self::Int64(value) => Some(i128::from(*value)),
            Self::TimestampNtz(value) => Some(i128::from(value.get_millisecond())),
            Self::TimestampLtz(value) => Some(i128::from(value.get_epoch_millisecond())),
            _ => None,
        }
    }

    /// Returns the string value useful for conservative pruning.
    #[doc(hidden)]
    pub fn as_string(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// Converts this literal into a Fluss row datum.
    #[doc(hidden)]
    pub fn to_datum(&self) -> Datum<'static> {
        match self {
            Self::Null => Datum::Null,
            Self::Bool(value) => (*value).into(),
            Self::Int8(value) => (*value).into(),
            Self::Int16(value) => (*value).into(),
            Self::Int32(value) => (*value).into(),
            Self::Int64(value) => (*value).into(),
            Self::Float32(value) => (*value).into(),
            Self::Float64(value) => (*value).into(),
            Self::String(value) => value.clone().into(),
            Self::Bytes(value) => value.clone().into(),
            Self::Decimal(value) => value.clone().into(),
            Self::Date(value) => Date::new(*value).into(),
            Self::Time(value) => Time::new(*value).into(),
            Self::TimestampNtz(value) => (*value).into(),
            Self::TimestampLtz(value) => (*value).into(),
        }
    }
}

/// A core predicate resolved against a concrete Fluss row type.
///
/// This type is public only so sibling workspace crates can share binding,
/// coercion, protocol encoding, pruning, and exact evaluation semantics.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq)]
pub enum BoundPredicate {
    AlwaysTrue,
    Leaf {
        field_index: usize,
        field_name: String,
        field_id: i32,
        data_type: DataType,
        function: LeafFunction,
        literals: Vec<BoundLiteral>,
    },
    Compound {
        function: CompoundFunction,
        children: Vec<BoundPredicate>,
    },
}

impl BoundPredicate {
    /// Binds an optional predicate to `row_type` using Fluss literal coercion rules.
    /// Consumers validate backend-specific physical representations separately.
    #[doc(hidden)]
    pub fn bind(predicate: Option<&Predicate>, row_type: &RowType) -> Result<Self> {
        match predicate {
            Some(predicate) => Self::bind_predicate(predicate, row_type),
            None => Ok(Self::AlwaysTrue),
        }
    }

    fn bind_predicate(predicate: &Predicate, row_type: &RowType) -> Result<Self> {
        match predicate {
            Predicate::Leaf {
                field,
                function,
                literals,
            } => {
                let (field_index, data_field) = resolve_field(row_type, field)?;
                validate_leaf(*function, data_field, literals)?;
                Ok(Self::Leaf {
                    field_index,
                    field_name: field.clone(),
                    field_id: data_field.field_id(),
                    data_type: data_field.data_type().clone(),
                    function: *function,
                    literals: literals
                        .iter()
                        .map(|literal| bind_literal(data_field, literal))
                        .collect::<Result<Vec<_>>>()?,
                })
            }
            Predicate::Compound { function, children } => {
                if children.is_empty() {
                    return Err(IllegalArgument {
                        message: format!("{} predicate has no children", compound_name(*function)),
                    });
                }
                Ok(Self::Compound {
                    function: *function,
                    children: children
                        .iter()
                        .map(|child| Self::bind_predicate(child, row_type))
                        .collect::<Result<Vec<_>>>()?,
                })
            }
        }
    }

    /// Returns referenced table field indexes in first-seen order.
    #[doc(hidden)]
    pub fn referenced_field_indexes(&self) -> Vec<usize> {
        let mut indexes = Vec::new();
        let mut seen = HashSet::new();
        self.collect_referenced_field_indexes(&mut indexes, &mut seen);
        indexes
    }

    fn collect_referenced_field_indexes(
        &self,
        indexes: &mut Vec<usize>,
        seen: &mut HashSet<usize>,
    ) {
        match self {
            Self::AlwaysTrue => {}
            Self::Leaf { field_index, .. } => {
                if seen.insert(*field_index) {
                    indexes.push(*field_index);
                }
            }
            Self::Compound { children, .. } => {
                for child in children {
                    child.collect_referenced_field_indexes(indexes, seen);
                }
            }
        }
    }
}

fn resolve_field<'a>(row_type: &'a RowType, name: &str) -> Result<(usize, &'a DataField)> {
    row_type
        .get_field_index(name)
        .map(|index| (index, &row_type.fields()[index]))
        .ok_or_else(|| IllegalArgument {
            message: format!(
                "filter column '{}' does not exist in the table schema, available columns: {:?}",
                name,
                row_type.get_field_names()
            ),
        })
}

fn validate_leaf(function: LeafFunction, field: &DataField, literals: &[Literal]) -> Result<()> {
    let expected = match function {
        LeafFunction::In | LeafFunction::NotIn => return Ok(()),
        LeafFunction::IsNull | LeafFunction::IsNotNull => 0,
        _ => 1,
    };
    if literals.len() != expected {
        return Err(IllegalArgument {
            message: format!(
                "{function:?} on column '{}' expects {expected} literal(s), got {}",
                field.name(),
                literals.len()
            ),
        });
    }

    if literals.first() == Some(&Literal::Null) {
        return Err(IllegalArgument {
            message: format!(
                "{function:?} on column '{}' cannot take a null literal, use is_null()/is_not_null()",
                field.name()
            ),
        });
    }

    if matches!(
        function,
        LeafFunction::StartsWith | LeafFunction::EndsWith | LeafFunction::Contains
    ) && !matches!(field.data_type(), DataType::Char(_) | DataType::String(_))
    {
        return Err(IllegalArgument {
            message: format!(
                "{function:?} requires a character string column, but column '{}' is {}",
                field.name(),
                field.data_type()
            ),
        });
    }
    Ok(())
}

fn bind_literal(field: &DataField, literal: &Literal) -> Result<BoundLiteral> {
    if matches!(literal, Literal::Null) {
        return Ok(BoundLiteral::Null);
    }

    // The coercion rules are shared with the existing protobuf encoder.
    // Arrow physical-width checks belong to the evaluator, not the wire format.
    Ok(match (field.data_type(), literal) {
        (DataType::TinyInt(_), _) => {
            BoundLiteral::Int8(integer_in_range(field, literal, i8::MAX as i64)? as i8)
        }
        (DataType::SmallInt(_), _) => {
            BoundLiteral::Int16(integer_in_range(field, literal, i16::MAX as i64)? as i16)
        }
        (DataType::Int(_), _) => {
            BoundLiteral::Int32(integer_in_range(field, literal, i32::MAX as i64)? as i32)
        }
        (DataType::BigInt(_), _) => BoundLiteral::Int64(integer_value(field, literal)?),
        (DataType::Float(_), _) => BoundLiteral::Float32(float32(field, literal)?),
        (DataType::Double(_), _) => BoundLiteral::Float64(float64(field, literal)?),
        (DataType::Decimal(decimal_type), Literal::Decimal(value)) => BoundLiteral::Decimal(
            rescale(field, value, decimal_type.precision(), decimal_type.scale())?,
        ),
        (DataType::Boolean(_), Literal::Bool(_))
        | (DataType::Char(_) | DataType::String(_), Literal::String(_))
        | (DataType::Binary(_) | DataType::Bytes(_), Literal::Bytes(_))
        | (DataType::Date(_), Literal::Date(_))
        | (DataType::Time(_), Literal::Time(_))
        | (DataType::Timestamp(_), Literal::TimestampNtz(_))
        | (DataType::TimestampLTz(_), Literal::TimestampLtz(_)) => literal.clone(),
        (DataType::Array(_) | DataType::Map(_) | DataType::Row(_), _) => {
            return Err(unsupported_column(field));
        }
        _ => return Err(mismatch(field, literal)),
    })
}

fn integer_value(field: &DataField, literal: &Literal) -> Result<i64> {
    match literal {
        Literal::Int8(value) => Ok(i64::from(*value)),
        Literal::Int16(value) => Ok(i64::from(*value)),
        Literal::Int32(value) => Ok(i64::from(*value)),
        Literal::Int64(value) => Ok(*value),
        _ => Err(mismatch(field, literal)),
    }
}

fn integer_in_range(field: &DataField, literal: &Literal, max: i64) -> Result<i64> {
    let value = integer_value(field, literal)?;
    if value > max || value < -max - 1 {
        return Err(IllegalArgument {
            message: format!(
                "filter literal {value} is out of range for column '{}' of type {}",
                field.name(),
                field.data_type()
            ),
        });
    }
    Ok(value)
}

fn float32(field: &DataField, literal: &Literal) -> Result<f32> {
    if let Literal::Float32(value) = literal {
        return Ok(*value);
    }
    let value = float64(field, literal)?;
    let narrowed = value as f32;
    if f64::from(narrowed) != value && !value.is_nan() {
        return Err(inexact(field, value));
    }
    Ok(narrowed)
}

fn float64(field: &DataField, literal: &Literal) -> Result<f64> {
    match literal {
        Literal::Float64(value) => Ok(*value),
        Literal::Float32(value) => Ok(f64::from(*value)),
        Literal::Int8(_) | Literal::Int16(_) | Literal::Int32(_) | Literal::Int64(_) => {
            let value = integer_value(field, literal)?;
            let widened = value as f64;
            if widened as i128 != i128::from(value) {
                return Err(inexact(field, widened));
            }
            Ok(widened)
        }
        _ => Err(mismatch(field, literal)),
    }
}

fn inexact(field: &DataField, value: f64) -> crate::error::Error {
    IllegalArgument {
        message: format!(
            "filter literal {value} cannot be represented exactly by column '{}' of type {}",
            field.name(),
            field.data_type()
        ),
    }
}

fn rescale(field: &DataField, value: &Decimal, precision: u32, scale: u32) -> Result<Decimal> {
    let big_decimal = value.to_big_decimal();
    let rescaled = Decimal::from_big_decimal(big_decimal.clone(), precision, scale)?;
    if rescaled.to_big_decimal() != big_decimal {
        return Err(IllegalArgument {
            message: format!(
                "filter literal {big_decimal} does not fit column '{}' of type {}",
                field.name(),
                field.data_type()
            ),
        });
    }
    Ok(rescaled)
}

fn mismatch(field: &DataField, literal: &Literal) -> crate::error::Error {
    IllegalArgument {
        message: format!(
            "filter literal {literal:?} does not match column '{}' of type {}",
            field.name(),
            field.data_type()
        ),
    }
}

pub(crate) fn unsupported_column(field: &DataField) -> crate::error::Error {
    IllegalArgument {
        message: format!(
            "filter on column '{}' of type {} is not supported",
            field.name(),
            field.data_type()
        ),
    }
}

fn compound_name(function: CompoundFunction) -> &'static str {
    match function {
        CompoundFunction::And => "AND",
        CompoundFunction::Or => "OR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{DataField, DataTypes};
    use crate::predicate::col;

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::with_field_id("id", DataTypes::int(), None, 3),
            DataField::with_field_id("name", DataTypes::string(), None, 5),
        ])
    }

    #[test]
    fn binds_field_identity_and_coerced_literals_once() {
        let bound = BoundPredicate::bind(Some(&col("id").eq(1_i8)), &row_type()).unwrap();

        assert_eq!(
            bound,
            BoundPredicate::Leaf {
                field_index: 0,
                field_name: "id".to_string(),
                field_id: 3,
                data_type: DataTypes::int(),
                function: LeafFunction::Equal,
                literals: vec![BoundLiteral::Int32(1)],
            }
        );
    }

    #[test]
    fn discovers_referenced_fields_in_first_seen_order() {
        let predicate = col("name").starts_with("A").and(col("id").gt(1_i32));
        let bound = BoundPredicate::bind(Some(&predicate), &row_type()).unwrap();

        assert_eq!(bound.referenced_field_indexes(), vec![1, 0]);
    }

    #[test]
    fn rejects_invalid_shape_before_any_consumer_uses_the_predicate() {
        let predicate = Predicate::Leaf {
            field: "id".to_string(),
            function: LeafFunction::IsNull,
            literals: vec![Literal::Int32(1)],
        };

        let error = BoundPredicate::bind(Some(&predicate), &row_type()).unwrap_err();

        assert!(error.to_string().contains("expects 0 literal(s), got 1"));
    }

    #[test]
    fn rejects_integer_literals_that_round_when_bound_to_double() {
        let row_type = RowType::new(vec![DataField::new("value", DataTypes::double(), None)]);

        assert!(BoundPredicate::bind(Some(&col("value").eq(i64::MAX)), &row_type).is_err());
        BoundPredicate::bind(Some(&col("value").eq(9_007_199_254_740_994_i64)), &row_type).unwrap();
    }
}
