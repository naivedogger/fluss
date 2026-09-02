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

//! UnionRead planning result.

use crate::split::FlussLakeReadSplit;
use arrow::datatypes::SchemaRef;

/// Aggregated statistics about a planned UnionRead job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlussLakePlanStatistics {
    pub split_count: usize,
    pub estimated_rows: Option<usize>,
    pub estimated_size: Option<usize>,
}

impl FlussLakePlanStatistics {
    pub(crate) fn from_splits(splits: &[FlussLakeReadSplit]) -> Self {
        Self {
            split_count: splits.len(),
            estimated_rows: sum_estimates(splits.iter().map(|split| split.estimated_rows)),
            estimated_size: sum_estimates(splits.iter().map(|split| split.estimated_size)),
        }
    }
}

/// Result of planning a bounded read.
#[derive(Debug, Clone)]
pub struct FlussLakeReadPlan {
    schema: SchemaRef,
    splits: Vec<FlussLakeReadSplit>,
    statistics: FlussLakePlanStatistics,
}

impl FlussLakeReadPlan {
    pub(crate) fn new(
        schema: SchemaRef,
        splits: Vec<FlussLakeReadSplit>,
        statistics: FlussLakePlanStatistics,
    ) -> Self {
        Self {
            schema,
            splits,
            statistics,
        }
    }

    /// Schema that every split will produce when read.
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// Number of logical splits in this plan.
    pub fn split_count(&self) -> usize {
        self.splits.len()
    }

    /// All splits to read.
    pub fn splits(&self) -> &[FlussLakeReadSplit] {
        &self.splits
    }

    /// Plan-level statistics.
    pub fn statistics(&self) -> FlussLakePlanStatistics {
        self.statistics
    }
}

fn sum_estimates(mut estimates: impl Iterator<Item = Option<usize>>) -> Option<usize> {
    estimates.try_fold(0usize, |total, estimate| total.checked_add(estimate?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split::{FlussLakePartitionIdentity, SplitStatistics};
    use crate::split_descriptor::SplitDescriptor;
    use fluss::metadata::{TableBucket, TablePath};

    fn split(rows: Option<usize>, size: Option<usize>) -> FlussLakeReadSplit {
        let descriptor = SplitDescriptor::try_new(
            TablePath::new("fluss", "orders"),
            1,
            false,
            TableBucket::new(5, 0),
            0,
            10,
            None,
            Vec::new(),
            Vec::new(),
        )
        .unwrap()
        .encode()
        .unwrap();
        FlussLakeReadSplit::try_new(
            "fluss.orders:root:0".to_string(),
            0,
            FlussLakePartitionIdentity::Unpartitioned,
            crate::CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor,
            SplitStatistics::new(rows, size),
        )
        .unwrap()
    }

    #[test]
    fn plan_statistics_sum_only_complete_estimates() {
        let complete = vec![split(Some(2), Some(10)), split(Some(3), Some(20))];
        assert_eq!(
            FlussLakePlanStatistics::from_splits(&complete),
            FlussLakePlanStatistics {
                split_count: 2,
                estimated_rows: Some(5),
                estimated_size: Some(30),
            }
        );

        let incomplete = vec![split(Some(2), None), split(None, Some(20))];
        assert_eq!(
            FlussLakePlanStatistics::from_splits(&incomplete),
            FlussLakePlanStatistics {
                split_count: 2,
                estimated_rows: None,
                estimated_size: None,
            }
        );
    }

    #[test]
    fn empty_plan_reports_zero_work() {
        assert_eq!(
            FlussLakePlanStatistics::from_splits(&[]),
            FlussLakePlanStatistics {
                split_count: 0,
                estimated_rows: Some(0),
                estimated_size: Some(0),
            }
        );
    }
}
