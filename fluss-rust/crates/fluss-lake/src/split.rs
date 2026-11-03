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

//! Local read tasks for bounded UnionRead.

use crate::split_descriptor::SplitDescriptor;
use crate::{FlussLakeError, Result};
use std::collections::HashSet;

/// Estimated work attached to one logical split during planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SplitStatistics {
    pub(crate) estimated_rows: Option<usize>,
    pub(crate) estimated_size: Option<usize>,
}

impl SplitStatistics {
    pub(crate) fn new(estimated_rows: Option<usize>, estimated_size: Option<usize>) -> Self {
        Self {
            estimated_rows,
            estimated_size,
        }
    }
}

pub use crate::partition::FlussLakePartitionIdentity;

/// One bounded read task with a frozen partition and source bucket identity.
///
/// Append plans have separate tasks for each lake split and each nonempty log
/// tail. Primary-key plans keep a bucket's lake baseline and tail together.
/// Tasks can be cloned and scheduled independently within the process. They
/// retain frozen source metadata and expose no transport format. Lake task
/// payloads remain private inside `execution_descriptor`.
#[derive(Debug, Clone)]
pub struct FlussLakeReadSplit {
    /// Opaque identifier unique within the owning plan, not across replanning.
    pub(crate) split_id: String,
    /// Fluss log bucket, or the lake-native bucket for an append lake task.
    /// Bucket-unaware append lake tasks use -1 and never carry a log range.
    pub(crate) bucket_id: i32,
    /// Partition represented by this logical split.
    pub(crate) partition: FlussLakePartitionIdentity,
    /// Best-effort input row estimate; `None` means unknown.
    /// Not an exact PK result cardinality.
    pub(crate) estimated_rows: Option<usize>,
    /// Best-effort input size in bytes; `None` means unknown.
    pub(crate) estimated_size: Option<usize>,
    execution_descriptor: SplitDescriptor,
}

impl FlussLakeReadSplit {
    /// Identifier unique within the plan that produced this task.
    pub fn split_id(&self) -> &str {
        &self.split_id
    }
    /// Source bucket, or -1 for a bucket-unaware append lake task.
    pub fn bucket_id(&self) -> i32 {
        self.bucket_id
    }
    /// Business partition covered by this task.
    pub fn partition(&self) -> &FlussLakePartitionIdentity {
        &self.partition
    }
    /// Estimated input rows, not the number of reconciled PK results.
    pub fn estimated_rows(&self) -> Option<usize> {
        self.estimated_rows
    }
    /// Estimated input bytes, if available.
    pub fn estimated_size(&self) -> Option<usize> {
        self.estimated_size
    }

    pub(crate) fn try_new(
        split_id: String,
        bucket_id: i32,
        partition: FlussLakePartitionIdentity,
        execution_descriptor: SplitDescriptor,
        statistics: SplitStatistics,
    ) -> Result<Self> {
        let split = Self {
            split_id,
            bucket_id,
            partition,
            estimated_rows: statistics.estimated_rows,
            estimated_size: statistics.estimated_size,
            execution_descriptor,
        };
        split.validate()?;
        Ok(split)
    }

    pub(crate) fn execution_descriptor(&self) -> &SplitDescriptor {
        &self.execution_descriptor
    }

    fn validate(&self) -> Result<()> {
        if self.split_id.is_empty() {
            return Err(FlussLakeError::Internal(
                "split id must not be empty".to_string(),
            ));
        }
        let descriptor = &self.execution_descriptor;
        if self.bucket_id < 0 && !(descriptor.is_append_lake() && self.bucket_id == -1) {
            return Err(FlussLakeError::Internal(format!(
                "split bucket id must be non-negative, got {}",
                self.bucket_id
            )));
        }
        validate_partition_identity(&self.partition)?;

        if descriptor.table_bucket().bucket_id() != self.bucket_id {
            return Err(FlussLakeError::Internal(format!(
                "split bucket id {} does not match execution descriptor bucket id {}",
                self.bucket_id,
                descriptor.table_bucket().bucket_id()
            )));
        }
        let descriptor_is_partitioned = descriptor.is_partitioned();
        let task_is_partitioned =
            matches!(self.partition, FlussLakePartitionIdentity::KeyValues(_));
        if descriptor_is_partitioned != task_is_partitioned {
            return Err(FlussLakeError::Internal(
                "split partition identity does not match the execution descriptor".to_string(),
            ));
        }
        if descriptor
            .lake_splits()
            .iter()
            .any(|task| task.partition != self.partition)
        {
            return Err(FlussLakeError::Internal(
                "lake task partition does not match the logical split".to_string(),
            ));
        }
        Ok(())
    }
}

fn validate_partition_identity(partition: &FlussLakePartitionIdentity) -> Result<()> {
    let FlussLakePartitionIdentity::KeyValues(key_values) = partition else {
        return Ok(());
    };
    if key_values.is_empty() {
        return Err(FlussLakeError::Internal(
            "partitioned split identity must contain at least one key/value pair".to_string(),
        ));
    }
    let mut keys = HashSet::with_capacity(key_values.len());
    for (key, _) in key_values {
        if key.is_empty() {
            return Err(FlussLakeError::Internal(
                "partition key name must not be empty".to_string(),
            ));
        }
        if !keys.insert(key) {
            return Err(FlussLakeError::Internal(format!(
                "partition identity contains duplicate key '{key}'"
            )));
        }
    }
    Ok(())
}
