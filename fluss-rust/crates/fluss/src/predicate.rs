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

//! Engine-neutral predicates that can be used for conservative data pruning.
//!
//! These predicates are not an engine expression tree and do not imply exact
//! row-level evaluation. A reader may use them only when it can prove that
//! pruning cannot remove matching rows. The originating engine remains
//! responsible for evaluating its original predicate as a residual filter.

use crate::metadata::DataType;
use crate::row::{Date, Decimal, F32, F64, Time, TimestampLtz, TimestampNtz};

/// A stable reference to a field in the resolved Fluss table schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldRef {
    index: usize,
    name: String,
    data_type: DataType,
}

impl FieldRef {
    pub fn new(index: usize, name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            index,
            name: name.into(),
            data_type,
        }
    }

    /// Returns the field position in the resolved Fluss table schema.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Returns the field name retained for validation and diagnostics.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn data_type(&self) -> &DataType {
        &self.data_type
    }
}

/// An owned scalar literal accepted by the pruning predicate model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PruningLiteral {
    Null,
    Boolean(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Float32(F32),
    Float64(F64),
    String(String),
    Bytes(Vec<u8>),
    Decimal(Decimal),
    Date(Date),
    Time(Time),
    TimestampNtz(TimestampNtz),
    TimestampLtz(TimestampLtz),
}

impl From<bool> for PruningLiteral {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i8> for PruningLiteral {
    fn from(value: i8) -> Self {
        Self::Int8(value)
    }
}

impl From<i16> for PruningLiteral {
    fn from(value: i16) -> Self {
        Self::Int16(value)
    }
}

impl From<i32> for PruningLiteral {
    fn from(value: i32) -> Self {
        Self::Int32(value)
    }
}

impl From<i64> for PruningLiteral {
    fn from(value: i64) -> Self {
        Self::Int64(value)
    }
}

impl From<f32> for PruningLiteral {
    fn from(value: f32) -> Self {
        Self::Float32(value.into())
    }
}

impl From<f64> for PruningLiteral {
    fn from(value: f64) -> Self {
        Self::Float64(value.into())
    }
}

impl From<String> for PruningLiteral {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for PruningLiteral {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<Vec<u8>> for PruningLiteral {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

impl From<&[u8]> for PruningLiteral {
    fn from(value: &[u8]) -> Self {
        Self::Bytes(value.to_vec())
    }
}

impl From<Decimal> for PruningLiteral {
    fn from(value: Decimal) -> Self {
        Self::Decimal(value)
    }
}

impl From<Date> for PruningLiteral {
    fn from(value: Date) -> Self {
        Self::Date(value)
    }
}

impl From<Time> for PruningLiteral {
    fn from(value: Time) -> Self {
        Self::Time(value)
    }
}

impl From<TimestampNtz> for PruningLiteral {
    fn from(value: TimestampNtz) -> Self {
        Self::TimestampNtz(value)
    }
}

impl From<TimestampLtz> for PruningLiteral {
    fn from(value: TimestampLtz) -> Self {
        Self::TimestampLtz(value)
    }
}

/// Comparison operations understood by the pruning predicate model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComparisonOperator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

/// Null-check operations understood by the pruning predicate model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NullCheckOperator {
    IsNull,
    IsNotNull,
}

/// A conservative predicate supplied by an upstream engine for data pruning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PruningPredicate {
    Comparison {
        operator: ComparisonOperator,
        field: FieldRef,
        literal: PruningLiteral,
    },
    NullCheck {
        operator: NullCheckOperator,
        field: FieldRef,
    },
    In {
        field: FieldRef,
        literals: Vec<PruningLiteral>,
    },
    And(Vec<PruningPredicate>),
    Or(Vec<PruningPredicate>),
}

impl PruningPredicate {
    pub fn comparison(
        operator: ComparisonOperator,
        field: FieldRef,
        literal: impl Into<PruningLiteral>,
    ) -> Self {
        Self::Comparison {
            operator,
            field,
            literal: literal.into(),
        }
    }

    pub fn null_check(operator: NullCheckOperator, field: FieldRef) -> Self {
        Self::NullCheck { operator, field }
    }

    pub fn in_list(
        field: FieldRef,
        literals: impl IntoIterator<Item = impl Into<PruningLiteral>>,
    ) -> Self {
        Self::In {
            field,
            literals: literals.into_iter().map(Into::into).collect(),
        }
    }

    pub fn and(children: impl IntoIterator<Item = PruningPredicate>) -> Self {
        Self::And(children.into_iter().collect())
    }

    pub fn or(children: impl IntoIterator<Item = PruningPredicate>) -> Self {
        Self::Or(children.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::DataTypes;

    #[test]
    fn field_reference_keeps_resolved_schema_identity() {
        let field = FieldRef::new(2, "region", DataTypes::string());

        assert_eq!(field.index(), 2);
        assert_eq!(field.name(), "region");
        assert_eq!(field.data_type(), &DataTypes::string());
    }

    #[test]
    fn predicate_tree_is_owned_and_engine_neutral() {
        let id = FieldRef::new(0, "id", DataTypes::bigint());
        let region = FieldRef::new(1, "region", DataTypes::string());
        let predicate = PruningPredicate::and([
            PruningPredicate::comparison(ComparisonOperator::GreaterThan, id, 100_i64),
            PruningPredicate::in_list(region, ["US", "CN"]),
        ]);

        assert!(matches!(predicate, PruningPredicate::And(children) if children.len() == 2));
    }

    #[test]
    fn primitive_values_convert_to_owned_literals() {
        assert_eq!(
            PruningLiteral::from("fluss"),
            PruningLiteral::String("fluss".to_string())
        );
        assert_eq!(
            PruningLiteral::from(&[1_u8, 2_u8][..]),
            PruningLiteral::Bytes(vec![1, 2])
        );
        assert_eq!(
            PruningLiteral::from(1.5_f32),
            PruningLiteral::Float32(1.5.into())
        );
    }
}
