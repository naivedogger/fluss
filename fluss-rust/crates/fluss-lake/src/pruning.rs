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

//! Conservative partition pruning over a schema-bound core predicate.

use crate::FlussLakePartitionIdentity;
use fluss::metadata::{ResolvedPartitionSpec, RowType};
use fluss::predicate::{BoundLiteral, BoundPredicate, CompoundFunction, LeafFunction};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruthValue {
    True,
    False,
    Unknown,
}

/// Evaluates predicates only against fields represented by partition values.
pub(crate) struct PartitionPruner {
    partition_fields: HashMap<usize, String>,
    predicate: BoundPredicate,
}

impl PartitionPruner {
    pub(crate) fn new(
        row_type: &RowType,
        partition_keys: &[String],
        predicate: &BoundPredicate,
    ) -> Self {
        let partition_fields = partition_keys
            .iter()
            .filter_map(|partition_key| {
                row_type
                    .fields()
                    .iter()
                    .position(|field| field.name() == partition_key)
                    .map(|field_index| (field_index, partition_key.clone()))
            })
            .collect();
        Self {
            partition_fields,
            predicate: predicate.clone(),
        }
    }

    pub(crate) fn partition_may_match(&self, partition_spec: &ResolvedPartitionSpec) -> bool {
        let values: HashMap<&str, &str> = partition_spec
            .get_partition_keys()
            .iter()
            .zip(partition_spec.get_partition_values())
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        self.partition_values_may_match(&values)
    }

    pub(crate) fn partition_identity_may_match(
        &self,
        partition: &FlussLakePartitionIdentity,
    ) -> bool {
        let FlussLakePartitionIdentity::KeyValues(key_values) = partition else {
            return true;
        };
        let values: HashMap<&str, &str> = key_values
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        self.partition_values_may_match(&values)
    }

    fn partition_values_may_match(&self, values: &HashMap<&str, &str>) -> bool {
        self.evaluate(&self.predicate, values) != TruthValue::False
    }

    fn evaluate(
        &self,
        predicate: &BoundPredicate,
        partition_values: &HashMap<&str, &str>,
    ) -> TruthValue {
        match predicate {
            BoundPredicate::AlwaysTrue => TruthValue::True,
            BoundPredicate::Compound { function, children } => match function {
                CompoundFunction::And => {
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
                CompoundFunction::Or => {
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
            },
            BoundPredicate::Leaf {
                field_index,
                field_name,
                function,
                literals,
                ..
            } => {
                let Some(partition_key) = self.partition_fields.get(field_index) else {
                    return TruthValue::Unknown;
                };
                if partition_key != field_name {
                    return TruthValue::Unknown;
                }
                let Some(value) = partition_values.get(partition_key.as_str()).copied() else {
                    return TruthValue::Unknown;
                };
                evaluate_leaf(value, *function, literals)
            }
        }
    }
}

fn evaluate_leaf(
    partition_value: &str,
    function: LeafFunction,
    literals: &[BoundLiteral],
) -> TruthValue {
    match function {
        LeafFunction::IsNull | LeafFunction::IsNotNull => TruthValue::Unknown,
        LeafFunction::In | LeafFunction::NotIn => {
            if literals.is_empty() {
                return if function == LeafFunction::In {
                    TruthValue::False
                } else {
                    TruthValue::True
                };
            }
            let mut result = TruthValue::False;
            for literal in literals {
                match literal_equals(partition_value, literal) {
                    Some(true) => {
                        return if function == LeafFunction::In {
                            TruthValue::True
                        } else {
                            TruthValue::False
                        };
                    }
                    Some(false) => {}
                    None => result = TruthValue::Unknown,
                }
            }
            if function == LeafFunction::NotIn {
                match result {
                    TruthValue::False => TruthValue::True,
                    other => other,
                }
            } else {
                result
            }
        }
        LeafFunction::StartsWith | LeafFunction::EndsWith | LeafFunction::Contains => {
            let Some(expected) = literals.first().and_then(BoundLiteral::as_string) else {
                return TruthValue::Unknown;
            };
            let matches = match function {
                LeafFunction::StartsWith => partition_value.starts_with(expected),
                LeafFunction::EndsWith => partition_value.ends_with(expected),
                LeafFunction::Contains => partition_value.contains(expected),
                _ => unreachable!(),
            };
            truth(matches)
        }
        _ => evaluate_comparison(partition_value, function, &literals[0]),
    }
}

fn evaluate_comparison(
    partition_value: &str,
    function: LeafFunction,
    literal: &BoundLiteral,
) -> TruthValue {
    if let Some(expected) = literal.as_string() {
        return truth(match function {
            LeafFunction::Equal => partition_value == expected,
            LeafFunction::NotEqual => partition_value != expected,
            LeafFunction::LessThan => partition_value < expected,
            LeafFunction::LessOrEqual => partition_value <= expected,
            LeafFunction::GreaterThan => partition_value > expected,
            LeafFunction::GreaterOrEqual => partition_value >= expected,
            _ => return TruthValue::Unknown,
        });
    }
    let Some(expected) = literal.as_integer() else {
        return TruthValue::Unknown;
    };
    let Ok(actual) = partition_value.parse::<i128>() else {
        return TruthValue::Unknown;
    };
    truth(match function {
        LeafFunction::Equal => actual == expected,
        LeafFunction::NotEqual => actual != expected,
        LeafFunction::LessThan => actual < expected,
        LeafFunction::LessOrEqual => actual <= expected,
        LeafFunction::GreaterThan => actual > expected,
        LeafFunction::GreaterOrEqual => actual >= expected,
        _ => return TruthValue::Unknown,
    })
}

fn literal_equals(partition_value: &str, literal: &BoundLiteral) -> Option<bool> {
    if let Some(expected) = literal.as_string() {
        return Some(partition_value == expected);
    }
    let expected = literal.as_integer()?;
    partition_value
        .parse::<i128>()
        .ok()
        .map(|actual| actual == expected)
}

fn truth(value: bool) -> TruthValue {
    if value {
        TruthValue::True
    } else {
        TruthValue::False
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluss::metadata::{DataField, DataTypes};
    use fluss::predicate::{Predicate, col};
    use std::sync::Arc;

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("region", DataTypes::string(), None),
            DataField::new("day", DataTypes::int(), None),
        ])
    }

    fn pruner(predicate: Predicate) -> PartitionPruner {
        let row_type = row_type();
        let bound = BoundPredicate::bind(Some(&predicate), &row_type).unwrap();
        PartitionPruner::new(
            &row_type,
            &["region".to_string(), "day".to_string()],
            &bound,
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
    fn prunes_equal_range_and_in_predicates() {
        assert!(!pruner(col("region").eq("US")).partition_may_match(&partition("EU", "2")));
        assert!(!pruner(col("day").ge(10_i32)).partition_may_match(&partition("US", "9")));
        assert!(
            !pruner(col("region").is_in(["US", "CN"])).partition_may_match(&partition("EU", "2"))
        );
    }

    #[test]
    fn non_partition_fields_and_unparsable_values_are_kept() {
        assert!(pruner(col("id").eq(1_i32)).partition_may_match(&partition("EU", "2")));
        assert!(pruner(col("day").eq(1_i32)).partition_may_match(&partition("EU", "not-a-number")));
    }

    #[test]
    fn mixed_boolean_predicates_remain_conservative() {
        let mixed = pruner(col("region").eq("US").or(col("id").eq(1_i32)));
        assert!(mixed.partition_may_match(&partition("EU", "2")));
        assert!(
            !pruner(col("region").eq("US").and(col("id").eq(1_i32)))
                .partition_may_match(&partition("EU", "2"))
        );
    }

    #[test]
    fn lake_only_partition_identity_uses_the_same_pruning_semantics() {
        let identity = FlussLakePartitionIdentity::KeyValues(vec![
            ("region".to_string(), "EU".to_string()),
            ("day".to_string(), "2".to_string()),
        ]);

        assert!(!pruner(col("region").eq("US")).partition_identity_may_match(&identity));
        assert!(pruner(col("region").eq("EU")).partition_identity_may_match(&identity));
    }
}
