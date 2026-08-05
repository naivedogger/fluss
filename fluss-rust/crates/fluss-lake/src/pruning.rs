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

//! Conservative partition pruning over engine-supplied pruning predicates.
//!
//! Partitions are pruned only when a predicate provably evaluates to false
//! for the partition's key values. Any unsupported operator, literal kind or
//! non-partition field keeps the partition, so pruning can never remove
//! matching rows. The engine always re-evaluates its original predicates as
//! residual filters regardless of the pruning outcome.

use crate::{
    FlussLakePredicateInput, FlussLakePredicatePushdownDecision, FlussLakePredicatePushdownLevel,
};
use fluss::metadata::{ResolvedPartitionSpec, RowType};
use fluss::predicate::{ComparisonOperator, FieldRef, PruningLiteral, PruningPredicate};
use std::collections::HashMap;

/// Three-valued outcome of evaluating a predicate against partition values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruthValue {
    True,
    False,
    Unknown,
}

/// Evaluates request predicates against a table's partition key values.
pub(crate) struct PartitionPruner {
    /// Table field index of each partition key, keyed by partition key name.
    partition_fields: HashMap<usize, String>,
    predicates: Vec<PruningPredicate>,
}

impl PartitionPruner {
    /// Builds a pruner for one planning pass.
    ///
    /// `partition_keys` must reference existing fields of `row_type`; the
    /// planner validates predicate field identity separately.
    pub(crate) fn new(
        row_type: &RowType,
        partition_keys: &[String],
        predicates: &[FlussLakePredicateInput],
    ) -> Self {
        let mut partition_fields = HashMap::with_capacity(partition_keys.len());
        for partition_key in partition_keys {
            if let Some(field_index) = row_type
                .fields()
                .iter()
                .position(|field| field.name() == partition_key)
            {
                partition_fields.insert(field_index, partition_key.clone());
            }
        }
        Self {
            partition_fields,
            predicates: predicates
                .iter()
                .map(|input| input.predicate().clone())
                .collect(),
        }
    }

    /// Reports the pushdown level of every request predicate.
    ///
    /// A predicate is `PruningOnly` when this pruner could evaluate it to a
    /// definitive false for some partition; every level keeps the engine's
    /// residual evaluation obligation.
    pub(crate) fn decisions(
        &self,
        predicates: &[FlussLakePredicateInput],
    ) -> Vec<FlussLakePredicatePushdownDecision> {
        predicates
            .iter()
            .map(|input| {
                let level = if self.can_prune(input.predicate()) {
                    FlussLakePredicatePushdownLevel::PruningOnly
                } else {
                    FlussLakePredicatePushdownLevel::Unsupported
                };
                FlussLakePredicatePushdownDecision::new(input.id(), level)
            })
            .collect()
    }

    /// Returns whether a partition may contain matching rows.
    ///
    /// Request predicates are conjunctive: the partition is pruned as soon as
    /// any predicate is provably false for its key values.
    pub(crate) fn partition_may_match(&self, partition_spec: &ResolvedPartitionSpec) -> bool {
        let partition_values: HashMap<&str, &str> = partition_spec
            .get_partition_keys()
            .iter()
            .zip(partition_spec.get_partition_values())
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        self.predicates
            .iter()
            .all(|predicate| self.evaluate(predicate, &partition_values) != TruthValue::False)
    }

    fn can_prune(&self, predicate: &PruningPredicate) -> bool {
        match predicate {
            PruningPredicate::Comparison {
                operator,
                field,
                literal,
            } => {
                self.partition_fields.contains_key(&field.index())
                    && comparison_is_evaluable(*operator, literal)
            }
            PruningPredicate::In { field, literals } => {
                self.partition_fields.contains_key(&field.index())
                    && !literals.is_empty()
                    && literals.iter().all(literal_is_evaluable)
            }
            PruningPredicate::NullCheck { .. } => false,
            // A conjunction prunes when any child prunes.
            PruningPredicate::And(children) => children.iter().any(|child| self.can_prune(child)),
            // A disjunction only prunes when every branch can be proven false.
            PruningPredicate::Or(children) => {
                !children.is_empty() && children.iter().all(|child| self.can_prune(child))
            }
        }
    }

    fn evaluate(
        &self,
        predicate: &PruningPredicate,
        partition_values: &HashMap<&str, &str>,
    ) -> TruthValue {
        match predicate {
            PruningPredicate::Comparison {
                operator,
                field,
                literal,
            } => match self.partition_value(field, partition_values) {
                Some(value) => evaluate_comparison(*operator, value, literal),
                None => TruthValue::Unknown,
            },
            PruningPredicate::In { field, literals } => {
                match self.partition_value(field, partition_values) {
                    Some(value) => evaluate_in_list(value, literals),
                    None => TruthValue::Unknown,
                }
            }
            // Fluss partition key values are not modeled as nullable here, so
            // null checks conservatively keep the partition.
            PruningPredicate::NullCheck { .. } => TruthValue::Unknown,
            PruningPredicate::And(children) => {
                let mut result = TruthValue::True;
                for child in children {
                    match self.evaluate(child, partition_values) {
                        TruthValue::False => return TruthValue::False,
                        TruthValue::Unknown => result = TruthValue::Unknown,
                        TruthValue::True => {}
                    }
                }
                result
            }
            PruningPredicate::Or(children) => {
                if children.is_empty() {
                    return TruthValue::Unknown;
                }
                let mut result = TruthValue::False;
                for child in children {
                    match self.evaluate(child, partition_values) {
                        TruthValue::True => return TruthValue::True,
                        TruthValue::Unknown => result = TruthValue::Unknown,
                        TruthValue::False => {}
                    }
                }
                result
            }
        }
    }

    fn partition_value<'a>(
        &self,
        field: &FieldRef,
        partition_values: &HashMap<&str, &'a str>,
    ) -> Option<&'a str> {
        let partition_key = self.partition_fields.get(&field.index())?;
        // Field name mismatches are rejected during request validation; an
        // unknown key here only means the server sent unexpected metadata.
        if partition_key != field.name() {
            return None;
        }
        partition_values.get(partition_key.as_str()).copied()
    }
}

fn comparison_is_evaluable(operator: ComparisonOperator, literal: &PruningLiteral) -> bool {
    match operator {
        ComparisonOperator::Equal | ComparisonOperator::NotEqual => literal_is_evaluable(literal),
        ComparisonOperator::LessThan
        | ComparisonOperator::LessThanOrEqual
        | ComparisonOperator::GreaterThan
        | ComparisonOperator::GreaterThanOrEqual => literal_as_integer(literal).is_some(),
    }
}

/// Literal kinds whose equality against a partition value string is exact.
fn literal_is_evaluable(literal: &PruningLiteral) -> bool {
    matches!(literal, PruningLiteral::String(_)) || literal_as_integer(literal).is_some()
}

fn literal_as_integer(literal: &PruningLiteral) -> Option<i128> {
    match literal {
        PruningLiteral::Int8(value) => Some(i128::from(*value)),
        PruningLiteral::Int16(value) => Some(i128::from(*value)),
        PruningLiteral::Int32(value) => Some(i128::from(*value)),
        PruningLiteral::Int64(value) => Some(i128::from(*value)),
        _ => None,
    }
}

fn evaluate_comparison(
    operator: ComparisonOperator,
    partition_value: &str,
    literal: &PruningLiteral,
) -> TruthValue {
    match operator {
        ComparisonOperator::Equal => match literal_equals(partition_value, literal) {
            Some(true) => TruthValue::True,
            Some(false) => TruthValue::False,
            None => TruthValue::Unknown,
        },
        ComparisonOperator::NotEqual => match literal_equals(partition_value, literal) {
            Some(true) => TruthValue::False,
            Some(false) => TruthValue::True,
            None => TruthValue::Unknown,
        },
        ComparisonOperator::LessThan
        | ComparisonOperator::LessThanOrEqual
        | ComparisonOperator::GreaterThan
        | ComparisonOperator::GreaterThanOrEqual => {
            let (Some(literal_value), Ok(partition_value)) =
                (literal_as_integer(literal), partition_value.parse::<i128>())
            else {
                return TruthValue::Unknown;
            };
            let holds = match operator {
                ComparisonOperator::LessThan => partition_value < literal_value,
                ComparisonOperator::LessThanOrEqual => partition_value <= literal_value,
                ComparisonOperator::GreaterThan => partition_value > literal_value,
                ComparisonOperator::GreaterThanOrEqual => partition_value >= literal_value,
                _ => unreachable!("outer match narrowed to range operators"),
            };
            if holds {
                TruthValue::True
            } else {
                TruthValue::False
            }
        }
    }
}

fn evaluate_in_list(partition_value: &str, literals: &[PruningLiteral]) -> TruthValue {
    if literals.is_empty() {
        return TruthValue::Unknown;
    }
    let mut result = TruthValue::False;
    for literal in literals {
        match literal_equals(partition_value, literal) {
            Some(true) => return TruthValue::True,
            Some(false) => {}
            None => result = TruthValue::Unknown,
        }
    }
    result
}

/// Compares one partition value string against a literal.
///
/// Only exact-equality encodings are supported: strings compare verbatim and
/// integers compare after a strict numeric parse. Everything else is unknown.
fn literal_equals(partition_value: &str, literal: &PruningLiteral) -> Option<bool> {
    if let PruningLiteral::String(expected) = literal {
        return Some(partition_value == expected);
    }
    let literal_value = literal_as_integer(literal)?;
    match partition_value.parse::<i128>() {
        Ok(value) => Some(value == literal_value),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlussLakePredicateId;
    use fluss::metadata::{DataField, DataTypes};
    use fluss::predicate::NullCheckOperator;
    use std::sync::Arc;

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("region", DataTypes::string(), None),
            DataField::new("day", DataTypes::int(), None),
        ])
    }

    fn region_field() -> FieldRef {
        FieldRef::new(1, "region", DataTypes::string())
    }

    fn day_field() -> FieldRef {
        FieldRef::new(2, "day", DataTypes::int())
    }

    fn pruner(predicates: Vec<PruningPredicate>) -> PartitionPruner {
        let inputs: Vec<FlussLakePredicateInput> = predicates
            .into_iter()
            .enumerate()
            .map(|(index, predicate)| {
                FlussLakePredicateInput::new(FlussLakePredicateId::new(index as u32), predicate)
            })
            .collect();
        PartitionPruner::new(
            &row_type(),
            &["region".to_string(), "day".to_string()],
            &inputs,
        )
    }

    fn partition(region: &str, day: &str) -> ResolvedPartitionSpec {
        ResolvedPartitionSpec::new(
            Arc::from(["region".to_string(), "day".to_string()]),
            vec![region.to_string(), day.to_string()],
        )
        .unwrap()
    }

    #[test]
    fn equal_predicate_prunes_non_matching_partition() {
        let pruner = pruner(vec![PruningPredicate::comparison(
            ComparisonOperator::Equal,
            region_field(),
            "US",
        )]);

        assert!(pruner.partition_may_match(&partition("US", "20260728")));
        assert!(!pruner.partition_may_match(&partition("EU", "20260728")));
    }

    #[test]
    fn integer_range_predicate_prunes_by_numeric_order() {
        let pruner = pruner(vec![PruningPredicate::comparison(
            ComparisonOperator::GreaterThanOrEqual,
            day_field(),
            20260728_i32,
        )]);

        assert!(pruner.partition_may_match(&partition("US", "20260728")));
        // Lexicographic order would keep "9" here; numeric order prunes it.
        assert!(!pruner.partition_may_match(&partition("US", "9")));
    }

    #[test]
    fn unparsable_partition_value_is_never_pruned() {
        let pruner = pruner(vec![PruningPredicate::comparison(
            ComparisonOperator::Equal,
            day_field(),
            20260728_i32,
        )]);

        assert!(pruner.partition_may_match(&partition("US", "not-a-number")));
    }

    #[test]
    fn in_list_predicate_prunes_partitions_outside_the_list() {
        let pruner = pruner(vec![PruningPredicate::in_list(
            region_field(),
            ["US", "CN"],
        )]);

        assert!(pruner.partition_may_match(&partition("CN", "20260728")));
        assert!(!pruner.partition_may_match(&partition("EU", "20260728")));
    }

    #[test]
    fn non_partition_field_predicate_keeps_every_partition() {
        let pruner = pruner(vec![PruningPredicate::comparison(
            ComparisonOperator::Equal,
            FieldRef::new(0, "id", DataTypes::int()),
            42_i32,
        )]);

        assert!(pruner.partition_may_match(&partition("EU", "20260728")));
    }

    #[test]
    fn disjunction_prunes_only_when_every_branch_is_false() {
        let or_pruner = pruner(vec![PruningPredicate::or([
            PruningPredicate::comparison(ComparisonOperator::Equal, region_field(), "US"),
            PruningPredicate::comparison(ComparisonOperator::Equal, region_field(), "CN"),
        ])]);

        assert!(or_pruner.partition_may_match(&partition("CN", "20260728")));
        assert!(!or_pruner.partition_may_match(&partition("EU", "20260728")));

        let mixed = pruner(vec![PruningPredicate::or([
            PruningPredicate::comparison(ComparisonOperator::Equal, region_field(), "US"),
            PruningPredicate::comparison(
                ComparisonOperator::Equal,
                FieldRef::new(0, "id", DataTypes::int()),
                42_i32,
            ),
        ])]);
        assert!(mixed.partition_may_match(&partition("EU", "20260728")));
    }

    #[test]
    fn conjunction_prunes_when_any_child_is_false() {
        let pruner = pruner(vec![PruningPredicate::and([
            PruningPredicate::comparison(ComparisonOperator::Equal, region_field(), "US"),
            PruningPredicate::comparison(
                ComparisonOperator::Equal,
                FieldRef::new(0, "id", DataTypes::int()),
                42_i32,
            ),
        ])]);

        assert!(pruner.partition_may_match(&partition("US", "20260728")));
        assert!(!pruner.partition_may_match(&partition("EU", "20260728")));
    }

    #[test]
    fn decisions_report_pruning_only_for_evaluable_partition_predicates() {
        let inputs = vec![
            FlussLakePredicateInput::new(
                FlussLakePredicateId::new(1),
                PruningPredicate::comparison(ComparisonOperator::Equal, region_field(), "US"),
            ),
            FlussLakePredicateInput::new(
                FlussLakePredicateId::new(2),
                PruningPredicate::comparison(
                    ComparisonOperator::Equal,
                    FieldRef::new(0, "id", DataTypes::int()),
                    42_i32,
                ),
            ),
            FlussLakePredicateInput::new(
                FlussLakePredicateId::new(3),
                PruningPredicate::null_check(NullCheckOperator::IsNull, region_field()),
            ),
        ];
        let pruner = PartitionPruner::new(&row_type(), &["region".to_string()], &inputs);

        let decisions = pruner.decisions(&inputs);

        assert_eq!(
            decisions[0].level(),
            FlussLakePredicatePushdownLevel::PruningOnly
        );
        assert_eq!(
            decisions[1].level(),
            FlussLakePredicatePushdownLevel::Unsupported
        );
        assert_eq!(
            decisions[2].level(),
            FlussLakePredicatePushdownLevel::Unsupported
        );
        assert!(
            decisions
                .iter()
                .all(|decision| decision.level().requires_residual_evaluation())
        );
    }
}
