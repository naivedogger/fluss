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

//! Serializable FIP-48 logical read split.

use crate::split_descriptor::SplitDescriptor;
use crate::{FlussLakeError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Maximum split descriptor version supported by this reader.
pub(crate) const CURRENT_FLUSS_LAKE_SPLIT_VERSION: u32 = 1;

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

/// Stable partition identity exposed for scheduling and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FlussLakePartitionIdentity {
    Unpartitioned,
    KeyValues(Vec<(String, String)>),
}

/// One logical `(partition, bucket)` bounded read unit.
///
/// Physical lake splits remain private inside `execution_descriptor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlussLakeReadSplit {
    /// Opaque identifier for logging and diagnostics.
    pub split_id: String,
    /// Fluss bucket represented by this logical split.
    pub bucket_id: i32,
    /// Partition represented by this logical split.
    pub partition: FlussLakePartitionIdentity,
    /// Best-effort row estimate for this split.
    pub estimated_rows: Option<usize>,
    /// Best-effort byte-size estimate for this split.
    pub estimated_size: Option<usize>,
    /// Version of the private execution descriptor.
    pub descriptor_version: u32,
    execution_descriptor: Vec<u8>,
}

impl FlussLakeReadSplit {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_new(
        split_id: String,
        bucket_id: i32,
        partition: FlussLakePartitionIdentity,
        descriptor_version: u32,
        execution_descriptor: Vec<u8>,
        statistics: SplitStatistics,
    ) -> Result<Self> {
        if descriptor_version > CURRENT_FLUSS_LAKE_SPLIT_VERSION {
            return Err(incompatible_split_version(descriptor_version));
        }
        if descriptor_version == 0 {
            return Err(FlussLakeError::Internal(
                "split descriptor version must be greater than zero".to_string(),
            ));
        }
        if split_id.is_empty() {
            return Err(FlussLakeError::Internal(
                "split id must not be empty".to_string(),
            ));
        }
        if execution_descriptor.is_empty() {
            return Err(FlussLakeError::Internal(
                "execution descriptor must not be empty".to_string(),
            ));
        }
        let split = Self {
            split_id,
            bucket_id,
            partition,
            estimated_rows: statistics.estimated_rows,
            estimated_size: statistics.estimated_size,
            descriptor_version,
            execution_descriptor,
        };
        split.decode_execution_descriptor()?;
        Ok(split)
    }

    pub(crate) fn decode_execution_descriptor(&self) -> Result<SplitDescriptor> {
        if self.descriptor_version == 0 {
            return Err(FlussLakeError::Internal(
                "split descriptor version must be greater than zero".to_string(),
            ));
        }
        if self.execution_descriptor.is_empty() {
            return Err(FlussLakeError::Internal(
                "execution descriptor must not be empty".to_string(),
            ));
        }
        if self.split_id.is_empty() {
            return Err(FlussLakeError::Internal(
                "split id must not be empty".to_string(),
            ));
        }
        if self.bucket_id < 0 {
            return Err(FlussLakeError::Internal(format!(
                "split bucket id must be non-negative, got {}",
                self.bucket_id
            )));
        }
        validate_partition_identity(&self.partition)?;

        let descriptor = decode_descriptor(self.descriptor_version, &self.execution_descriptor)?;
        if descriptor.table_bucket().bucket_id() != self.bucket_id {
            return Err(FlussLakeError::Internal(format!(
                "public split bucket id {} does not match execution descriptor bucket id {}",
                self.bucket_id,
                descriptor.table_bucket().bucket_id()
            )));
        }
        let descriptor_is_partitioned = descriptor.is_partitioned();
        let public_is_partitioned =
            matches!(self.partition, FlussLakePartitionIdentity::KeyValues(_));
        if descriptor_is_partitioned != public_is_partitioned {
            return Err(FlussLakeError::Internal(
                "public split partition identity does not match the execution descriptor"
                    .to_string(),
            ));
        }
        Ok(descriptor)
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

fn decode_descriptor(version: u32, encoded: &[u8]) -> Result<SplitDescriptor> {
    match version {
        1 => SplitDescriptor::decode(encoded),
        version => Err(incompatible_split_version(version)),
    }
}

fn incompatible_split_version(split_version: u32) -> FlussLakeError {
    FlussLakeError::IncompatibleSplitVersion(format!(
        "split descriptor version {split_version} is newer than reader maximum supported version {CURRENT_FLUSS_LAKE_SPLIT_VERSION}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split_descriptor::SplitDescriptor;
    use fluss::metadata::{TableBucket, TablePath};

    fn descriptor() -> Vec<u8> {
        SplitDescriptor::try_new(
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
        .unwrap()
    }

    #[test]
    fn serde_round_trip_preserves_fip_shape() {
        let split = FlussLakeReadSplit::try_new(
            "orders/root/0".to_string(),
            0,
            FlussLakePartitionIdentity::Unpartitioned,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor(),
            SplitStatistics::new(Some(42), Some(1024)),
        )
        .unwrap();

        let encoded = serde_json::to_vec(&split).unwrap();
        let decoded: FlussLakeReadSplit = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded, split);
        assert_eq!(decoded.estimated_rows, Some(42));
        assert_eq!(decoded.estimated_size, Some(1024));
    }

    #[test]
    fn partition_identity_uses_key_values() {
        let identity = FlussLakePartitionIdentity::KeyValues(vec![
            ("region".to_string(), "US".to_string()),
            ("day".to_string(), "2026-09-02".to_string()),
        ]);
        let encoded = serde_json::to_vec(&identity).unwrap();
        assert_eq!(
            serde_json::from_slice::<FlussLakePartitionIdentity>(&encoded).unwrap(),
            identity
        );
    }

    #[test]
    fn newer_version_reports_split_and_reader_versions() {
        let error = FlussLakeReadSplit::try_new(
            "orders/root/0".to_string(),
            0,
            FlussLakePartitionIdentity::Unpartitioned,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION + 1,
            descriptor(),
            SplitStatistics::default(),
        )
        .unwrap_err();

        match error {
            FlussLakeError::IncompatibleSplitVersion(message) => {
                assert!(message.contains("version 2"));
                assert!(message.contains("version 1"));
            }
            other => panic!("expected incompatible split version, got {other}"),
        }
    }

    #[test]
    fn reader_validation_catches_a_public_version_field_mutation() {
        let mut split = FlussLakeReadSplit::try_new(
            "orders/root/0".to_string(),
            0,
            FlussLakePartitionIdentity::Unpartitioned,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor(),
            SplitStatistics::default(),
        )
        .unwrap();
        split.descriptor_version = CURRENT_FLUSS_LAKE_SPLIT_VERSION + 1;

        let error = split.decode_execution_descriptor().unwrap_err();
        match error {
            FlussLakeError::IncompatibleSplitVersion(message) => {
                assert!(message.contains("version 2"));
                assert!(message.contains("version 1"));
            }
            other => panic!("expected incompatible split version, got {other}"),
        }
    }

    #[test]
    fn reader_validation_catches_public_bucket_mutation_after_deserialization() {
        let split = FlussLakeReadSplit::try_new(
            "orders/root/0".to_string(),
            0,
            FlussLakePartitionIdentity::Unpartitioned,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor(),
            SplitStatistics::default(),
        )
        .unwrap();
        let mut value = serde_json::to_value(&split).unwrap();
        value["bucket_id"] = serde_json::json!(1);
        let mutated: FlussLakeReadSplit = serde_json::from_value(value).unwrap();

        assert!(matches!(
            mutated.decode_execution_descriptor(),
            Err(FlussLakeError::Internal(_))
        ));
    }

    #[test]
    fn reader_validation_rejects_invalid_partition_identity() {
        let split = FlussLakeReadSplit::try_new(
            "orders/root/0".to_string(),
            0,
            FlussLakePartitionIdentity::Unpartitioned,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor(),
            SplitStatistics::default(),
        )
        .unwrap();
        let mut value = serde_json::to_value(&split).unwrap();
        value["partition"] = serde_json::json!({"KeyValues": []});
        let mutated: FlussLakeReadSplit = serde_json::from_value(value).unwrap();

        assert!(matches!(
            mutated.decode_execution_descriptor(),
            Err(FlussLakeError::Internal(_))
        ));
    }
}
