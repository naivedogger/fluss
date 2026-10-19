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

//! Schema-bound predicates for local evaluation and conservative pruning.

use super::coercion::{
    compound_name, float32, float64, integer_in_range, integer_value, mismatch, rescale,
    resolve_field, unsupported_column, validate_leaf,
};
use crate::error::Error::IllegalArgument;
use crate::error::Result;
use crate::metadata::{DataField, DataType, RowType};
use crate::predicate::{CompoundFunction, LeafFunction, Literal, Predicate};
use std::collections::HashSet;

/// A core predicate resolved against a concrete Fluss row type.
///
/// This type is public only so sibling workspace crates can share binding,
/// coercion, pruning, and exact evaluation semantics.
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
        literals: Vec<Literal>,
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

fn bind_literal(field: &DataField, literal: &Literal) -> Result<Literal> {
    if matches!(literal, Literal::Null) {
        return Ok(Literal::Null);
    }

    // The coercion rules are shared with the existing protobuf encoder.
    // Arrow physical-width checks belong to the evaluator, not the wire format.
    Ok(match (field.data_type(), literal) {
        (DataType::TinyInt(_), _) => {
            Literal::Int8(integer_in_range(field, literal, i8::MAX as i64)? as i8)
        }
        (DataType::SmallInt(_), _) => {
            Literal::Int16(integer_in_range(field, literal, i16::MAX as i64)? as i16)
        }
        (DataType::Int(_), _) => {
            Literal::Int32(integer_in_range(field, literal, i32::MAX as i64)? as i32)
        }
        (DataType::BigInt(_), _) => Literal::Int64(integer_value(field, literal)?),
        (DataType::Float(_), _) => Literal::Float32(float32(field, literal)?),
        (DataType::Double(_), _) => Literal::Float64(float64(field, literal)?),
        (DataType::Decimal(decimal_type), Literal::Decimal(value)) => Literal::Decimal(rescale(
            field,
            value,
            decimal_type.precision(),
            decimal_type.scale(),
        )?),
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
                literals: vec![Literal::Int32(1)],
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
    fn local_binding_does_not_require_protocol_field_ids() {
        let row_type = RowType::new(vec![DataField::new("id", DataTypes::int(), None)]);
        let bound = BoundPredicate::bind(Some(&col("id").eq(1_i8)), &row_type).unwrap();

        assert!(matches!(
            bound,
            BoundPredicate::Leaf { field_index: 0, ref literals, .. }
                if literals == &[Literal::Int32(1)]
        ));
    }

    #[test]
    fn rejects_integer_literals_that_round_when_bound_to_double() {
        let row_type = RowType::new(vec![DataField::new("value", DataTypes::double(), None)]);

        assert!(BoundPredicate::bind(Some(&col("value").eq(i64::MAX)), &row_type).is_err());
        BoundPredicate::bind(Some(&col("value").eq(9_007_199_254_740_994_i64)), &row_type).unwrap();
    }
}
