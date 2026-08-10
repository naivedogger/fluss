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

use crate::split::{FlussLakeReadSplit, FlussLakeReadStatistics};
use arrow::datatypes::SchemaRef;

/// Aggregated statistics about a planned UnionRead job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlussLakePlanStatistics {
    split_count: usize,
}

impl FlussLakePlanStatistics {
    pub(crate) fn new(split_count: usize) -> Self {
        Self { split_count }
    }

    /// Number of splits the plan contains.
    pub fn split_count(&self) -> usize {
        self.split_count
    }
}

/// Result of planning a bounded read.
#[derive(Debug, Clone)]
pub struct FlussLakeReadPlan {
    output_schema: SchemaRef,
    splits: Vec<FlussLakeReadSplit>,
    statistics: FlussLakePlanStatistics,
}

impl FlussLakeReadPlan {
    pub(crate) fn new(
        output_schema: SchemaRef,
        splits: Vec<FlussLakeReadSplit>,
        statistics: FlussLakePlanStatistics,
    ) -> Self {
        Self {
            output_schema,
            splits,
            statistics,
        }
    }

    /// Schema that every split will produce when read.
    pub fn output_schema(&self) -> SchemaRef {
        self.output_schema.clone()
    }

    /// All splits to read.
    pub fn splits(&self) -> &[FlussLakeReadSplit] {
        &self.splits
    }

    /// Plan-level statistics.
    pub fn statistics(&self) -> FlussLakePlanStatistics {
        self.statistics
    }

    /// Estimated total rows across all splits, if every split reports one.
    pub fn estimated_total_rows(&self) -> Option<u64> {
        self.splits
            .iter()
            .map(|split| split.estimated_rows)
            .collect::<Option<Vec<_>>>()
            .map(|rows| rows.iter().sum())
    }

    /// Estimated total bytes across all splits, if every split reports one.
    pub fn estimated_total_size(&self) -> Option<u64> {
        self.splits
            .iter()
            .map(|split| split.estimated_size)
            .collect::<Option<Vec<_>>>()
            .map(|sizes| sizes.iter().sum())
    }

    /// Convenience accessor for the split-level statistics vector.
    pub fn split_statistics(&self) -> Vec<FlussLakeReadStatistics> {
        self.splits
            .iter()
            .map(FlussLakeReadSplit::statistics)
            .collect()
    }
}
