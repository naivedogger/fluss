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

//! Conservative bucket-key pruning over the engine-supplied filter predicate.
//!
//! Buckets are pruned only when the filter provides equality constraints on
//! every bucket-key column, so the bucket hash can be recomputed exactly. Any
//! missing or non-equality condition on a bucket key keeps all buckets.

use fluss::BucketingFunction;
use fluss::metadata::{DataLakeFormat, RowType};
use fluss::predicate::{BoundLiteral, BoundPredicate, CompoundFunction, LeafFunction};
use fluss::row::GenericRow;
use fluss::row::encode::KeyEncoderFactory;
use std::collections::{HashMap, HashSet};

/// Decides which buckets may contain rows that satisfy the scan filter.
pub(crate) struct BucketPruner {
    matching_buckets: Option<HashSet<i32>>,
}

impl BucketPruner {
    /// Builds a pruner for one planning pass.
    ///
    /// Returns a pruner that keeps every bucket when the filter does not
    /// provably constrain all bucket-key columns to equality values.
    pub(crate) fn new(
        row_type: &RowType,
        bucket_keys: &[String],
        num_buckets: i32,
        data_lake_format: Option<DataLakeFormat>,
        filter: &BoundPredicate,
    ) -> Self {
        if bucket_keys.is_empty() || num_buckets <= 0 {
            return Self {
                matching_buckets: None,
            };
        }

        let constraints = match extract_bucket_constraints(filter, bucket_keys) {
            Some(constraints) => constraints,
            None => {
                return Self {
                    matching_buckets: None,
                };
            }
        };

        match compute_matching_buckets(
            row_type,
            bucket_keys,
            num_buckets,
            data_lake_format,
            &constraints,
        ) {
            Ok(buckets) if buckets.is_empty() => Self {
                matching_buckets: Some(buckets),
            },
            Ok(buckets) => Self {
                matching_buckets: Some(buckets),
            },
            Err(_) => Self {
                matching_buckets: None,
            },
        }
    }

    /// Returns whether a bucket may contain matching rows.
    pub(crate) fn bucket_may_match(&self, bucket_id: i32) -> bool {
        match &self.matching_buckets {
            Some(buckets) => buckets.contains(&bucket_id),
            None => true,
        }
    }
}

/// Equality values extracted for each bucket-key column.
#[derive(Debug, Clone)]
struct BucketConstraints {
    values: Vec<Vec<BoundLiteral>>,
}

impl BucketConstraints {
    fn combinations(&self) -> Vec<Vec<BoundLiteral>> {
        let mut combinations: Vec<Vec<BoundLiteral>> = vec![Vec::new()];
        for column_values in &self.values {
            let mut next = Vec::with_capacity(combinations.len() * column_values.len());
            for combination in &combinations {
                for value in column_values {
                    let mut extended = combination.clone();
                    extended.push(value.clone());
                    next.push(extended);
                }
            }
            combinations = next;
        }
        combinations
    }
}

fn extract_bucket_constraints(
    filter: &BoundPredicate,
    bucket_keys: &[String],
) -> Option<BucketConstraints> {
    let mut per_column: HashMap<&str, Vec<BoundLiteral>> = HashMap::new();

    // Collect equality constraints from the top-level AND structure.
    let mut worklist: Vec<&BoundPredicate> = vec![filter];
    while let Some(predicate) = worklist.pop() {
        match predicate {
            BoundPredicate::Compound {
                function: CompoundFunction::And,
                children,
            } => worklist.extend(children),
            BoundPredicate::Leaf {
                field_name,
                function: LeafFunction::Equal,
                literals,
                ..
            } if bucket_keys.contains(field_name) => {
                per_column
                    .entry(field_name.as_str())
                    .or_default()
                    .extend(literals.iter().cloned());
            }
            BoundPredicate::Leaf {
                field_name,
                function: LeafFunction::In,
                literals,
                ..
            } if bucket_keys.contains(field_name) => {
                per_column
                    .entry(field_name.as_str())
                    .or_default()
                    .extend(literals.iter().cloned());
            }
            _ => {}
        }
    }

    let values: Vec<Vec<BoundLiteral>> = bucket_keys
        .iter()
        .map(|key| per_column.remove(key.as_str()).unwrap_or_default())
        .collect();

    if values.iter().any(Vec::is_empty) {
        return None;
    }
    Some(BucketConstraints { values })
}

fn compute_matching_buckets(
    row_type: &RowType,
    bucket_keys: &[String],
    num_buckets: i32,
    data_lake_format: Option<DataLakeFormat>,
    constraints: &BucketConstraints,
) -> crate::Result<HashSet<i32>> {
    let mut encoder =
        KeyEncoderFactory::of_bucket_key_encoder(row_type, bucket_keys, &data_lake_format)
            .map_err(|error| {
                crate::FlussLakeError::PlanningFailed(format!(
                    "failed to create bucket-key encoder for pruning: {error}"
                ))
            })?;
    let bucketing = <dyn BucketingFunction>::of(data_lake_format.as_ref());
    let key_positions: Vec<usize> = bucket_keys
        .iter()
        .map(|key| {
            row_type
                .fields()
                .iter()
                .position(|field| field.name() == key)
                .expect("bucket keys were validated against the row type")
        })
        .collect();

    let mut buckets = HashSet::new();
    for combination in constraints.combinations() {
        let mut row = GenericRow::new(row_type.fields().len());
        for (position, value) in key_positions.iter().zip(combination.iter()) {
            row.set_field(*position, value.to_datum());
        }
        let key_bytes = encoder.encode_key(&row).map_err(|error| {
            crate::FlussLakeError::PlanningFailed(format!(
                "failed to encode bucket-key value for pruning: {error}"
            ))
        })?;
        let bucket_id = bucketing
            .bucketing(&key_bytes, num_buckets)
            .map_err(|error| {
                crate::FlussLakeError::PlanningFailed(format!(
                    "failed to compute bucket for pruning: {error}"
                ))
            })?;
        buckets.insert(bucket_id);
    }
    Ok(buckets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluss::metadata::{DataField, DataTypes};
    use fluss::predicate::{Predicate, col};

    fn row_type() -> RowType {
        RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("region", DataTypes::string(), None),
        ])
    }

    fn bound(predicate: Predicate) -> BoundPredicate {
        BoundPredicate::bind(Some(&predicate), &row_type()).unwrap()
    }

    #[test]
    fn no_bucket_keys_keeps_every_bucket() {
        let pruner = BucketPruner::new(&row_type(), &[], 4, None, &bound(col("id").eq(1_i32)));
        assert!(pruner.bucket_may_match(0));
        assert!(pruner.bucket_may_match(3));
    }

    #[test]
    fn equality_on_single_bucket_key_prunes_others() {
        let pruner = BucketPruner::new(
            &row_type(),
            &["id".to_string()],
            4,
            None,
            &bound(col("id").eq(1_i32)),
        );
        // Hash of 1 with Fluss bucketing lands in one specific bucket.
        let matching: Vec<i32> = (0..4).filter(|id| pruner.bucket_may_match(*id)).collect();
        assert_eq!(matching.len(), 1);
    }

    #[test]
    fn missing_bucket_key_constraint_keeps_every_bucket() {
        let pruner = BucketPruner::new(
            &row_type(),
            &["id".to_string()],
            4,
            None,
            &bound(col("region").eq("US")),
        );
        assert!(pruner.bucket_may_match(0));
        assert!(pruner.bucket_may_match(3));
    }

    #[test]
    fn in_list_on_bucket_key_prunes_to_matching_buckets() {
        let pruner = BucketPruner::new(
            &row_type(),
            &["id".to_string()],
            4,
            None,
            &bound(col("id").is_in([1_i32, 2_i32])),
        );
        let matching: Vec<i32> = (0..4).filter(|id| pruner.bucket_may_match(*id)).collect();
        assert!(!matching.is_empty());
        assert!(matching.len() <= 2);
    }
}
