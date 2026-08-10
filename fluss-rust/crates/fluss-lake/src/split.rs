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

//! Versioned, serializable UnionRead split and its scheduling metadata.

use crate::split_descriptor::SplitDescriptor;
use crate::{FlussLakeError, FlussLakeResult};

const UNION_READ_SPLIT_MAGIC: [u8; 4] = *b"FLUR";
const UNION_READ_SPLIT_HEADER_SIZE: usize = 50;
const STATISTICS_ROWS_PRESENT: u8 = 1;
const STATISTICS_BYTES_PRESENT: u8 = 1 << 1;
const PARTITION_PRESENT: u8 = 1 << 2;
const PARTITION_ID_PRESENT: u8 = 1 << 3;
const PARTITION_NAME_PRESENT: u8 = 1 << 4;

/// Current version of the serialized UnionRead split descriptor envelope.
pub const CURRENT_FLUSS_LAKE_SPLIT_VERSION: u32 = 2;

/// Estimated work exposed to an upstream engine for scheduling and costing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlussLakeReadStatistics {
    estimated_rows: Option<u64>,
    estimated_bytes: Option<u64>,
}

impl FlussLakeReadStatistics {
    pub fn new(estimated_rows: Option<u64>, estimated_bytes: Option<u64>) -> Self {
        Self {
            estimated_rows,
            estimated_bytes,
        }
    }

    pub fn estimated_rows(&self) -> Option<u64> {
        self.estimated_rows
    }

    pub fn estimated_bytes(&self) -> Option<u64> {
        self.estimated_bytes
    }
}

/// Identity of a partitioned split.
///
/// Root (unpartitioned) splits carry `None`; partitioned splits carry at
/// least one of the partition id or the partition name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FlussLakePartitionIdentity {
    partition_id: Option<i64>,
    partition_name: Option<String>,
}

impl FlussLakePartitionIdentity {
    /// Creates a root identity representing an unpartitioned split.
    pub fn root() -> Self {
        Self {
            partition_id: None,
            partition_name: None,
        }
    }

    /// Creates a partition identity from the server-returned id and name.
    pub fn new(partition_id: Option<i64>, partition_name: Option<String>) -> Self {
        Self {
            partition_id,
            partition_name,
        }
    }

    /// Returns the partition id when the planner resolved one.
    pub fn partition_id(&self) -> Option<i64> {
        self.partition_id
    }

    /// Returns the partition name when the planner resolved one.
    pub fn partition_name(&self) -> Option<&str> {
        self.partition_name.as_deref()
    }

    /// Returns `true` for root (unpartitioned) splits.
    pub fn is_root(&self) -> bool {
        self.partition_id.is_none() && self.partition_name.is_none()
    }
}

/// An opaque, serializable bounded read split.
///
/// Engines may inspect the scheduling metadata, but must treat the execution
/// descriptor as opaque and consume it through [`crate::FlussLakeReader`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlussLakeReadSplit {
    pub split_id: String,
    pub bucket_id: i32,
    pub partition: Option<FlussLakePartitionIdentity>,
    pub estimated_rows: Option<u64>,
    pub estimated_size: Option<u64>,
    pub descriptor_version: u32,
    execution_descriptor: Vec<u8>,
}

impl FlussLakeReadSplit {
    /// Creates a split from a planner-produced execution descriptor.
    pub(crate) fn try_new(
        split_id: String,
        bucket_id: i32,
        partition: Option<FlussLakePartitionIdentity>,
        descriptor_version: u32,
        execution_descriptor: Vec<u8>,
        statistics: FlussLakeReadStatistics,
    ) -> FlussLakeResult<Self> {
        if descriptor_version != CURRENT_FLUSS_LAKE_SPLIT_VERSION {
            return Err(FlussLakeError::UnsupportedSplitVersion {
                version: descriptor_version,
            });
        }
        if split_id.is_empty() {
            return Err(FlussLakeError::InvalidSplit(
                "split id must not be empty".to_string(),
            ));
        }
        if execution_descriptor.is_empty() {
            return Err(FlussLakeError::InvalidSplit(
                "execution descriptor must not be empty".to_string(),
            ));
        }
        SplitDescriptor::decode(&execution_descriptor)?;
        Ok(Self {
            split_id,
            bucket_id,
            partition,
            estimated_rows: statistics.estimated_rows,
            estimated_size: statistics.estimated_bytes,
            descriptor_version,
            execution_descriptor,
        })
    }

    /// Returns the opaque execution descriptor for transport to an executor.
    pub(crate) fn execution_descriptor(&self) -> &[u8] {
        &self.execution_descriptor
    }

    /// Returns the split-level statistics as a convenience object.
    pub fn statistics(&self) -> FlussLakeReadStatistics {
        FlussLakeReadStatistics::new(self.estimated_rows, self.estimated_size)
    }

    /// Encodes this split for distribution to an execution worker.
    ///
    /// The wire envelope is versioned independently of the internal split
    /// implementation. Engines must treat the returned bytes as opaque.
    pub fn encode(&self) -> FlussLakeResult<Vec<u8>> {
        let split_id = self.split_id.as_bytes();
        let split_id_len = u32::try_from(split_id.len()).map_err(|_| {
            FlussLakeError::InvalidSplit("split id exceeds the wire format limit".to_string())
        })?;
        let descriptor_len = u32::try_from(self.execution_descriptor.len()).map_err(|_| {
            FlussLakeError::InvalidSplit(
                "execution descriptor exceeds the wire format limit".to_string(),
            )
        })?;

        let (partition_flags, partition_id_bytes, partition_name_bytes) =
            encode_partition(&self.partition);

        let mut statistics_flags = 0;
        if self.estimated_rows.is_some() {
            statistics_flags |= STATISTICS_ROWS_PRESENT;
        }
        if self.estimated_size.is_some() {
            statistics_flags |= STATISTICS_BYTES_PRESENT;
        }

        let capacity = UNION_READ_SPLIT_HEADER_SIZE
            .checked_add(split_id.len())
            .and_then(|size| size.checked_add(partition_name_bytes.len()))
            .and_then(|size| size.checked_add(self.execution_descriptor.len()))
            .ok_or_else(|| {
                FlussLakeError::InvalidSplit("encoded split size overflows usize".to_string())
            })?;
        let mut encoded = Vec::with_capacity(capacity);
        encoded.extend_from_slice(&UNION_READ_SPLIT_MAGIC);
        encoded.extend_from_slice(&self.descriptor_version.to_le_bytes());
        encoded.extend_from_slice(&split_id_len.to_le_bytes());
        encoded.extend_from_slice(&self.bucket_id.to_le_bytes());
        encoded.push(partition_flags);
        encoded.extend_from_slice(&partition_id_bytes);
        encoded.extend_from_slice(&(partition_name_bytes.len() as u32).to_le_bytes());
        encoded.extend_from_slice(&descriptor_len.to_le_bytes());
        encoded.push(statistics_flags);
        encoded.extend_from_slice(&self.estimated_rows.unwrap_or_default().to_le_bytes());
        encoded.extend_from_slice(&self.estimated_size.unwrap_or_default().to_le_bytes());
        encoded.extend_from_slice(split_id);
        encoded.extend_from_slice(&partition_name_bytes);
        encoded.extend_from_slice(&self.execution_descriptor);
        Ok(encoded)
    }

    /// Decodes an opaque split produced by [`FlussLakeReadSplit::encode`].
    pub fn decode(encoded: &[u8]) -> FlussLakeResult<Self> {
        if encoded.len() < UNION_READ_SPLIT_HEADER_SIZE {
            return Err(FlussLakeError::InvalidSplit(format!(
                "split envelope is truncated: expected at least {UNION_READ_SPLIT_HEADER_SIZE} bytes, got {}",
                encoded.len()
            )));
        }
        if encoded[..UNION_READ_SPLIT_MAGIC.len()] != UNION_READ_SPLIT_MAGIC {
            return Err(FlussLakeError::InvalidSplit(
                "split envelope has an invalid magic header".to_string(),
            ));
        }

        let descriptor_version = read_u32(encoded, 4)?;
        if descriptor_version != CURRENT_FLUSS_LAKE_SPLIT_VERSION {
            return Err(FlussLakeError::UnsupportedSplitVersion {
                version: descriptor_version,
            });
        }

        let split_id_len = read_u32(encoded, 8)? as usize;
        let bucket_id = read_i32(encoded, 12)?;
        let partition_flags = encoded[16];
        let known_partition_flags =
            PARTITION_PRESENT | PARTITION_ID_PRESENT | PARTITION_NAME_PRESENT;
        if partition_flags & !known_partition_flags != 0 {
            return Err(FlussLakeError::InvalidSplit(format!(
                "split envelope contains unknown partition flags 0x{partition_flags:02x}"
            )));
        }

        let partition_id = if partition_flags & PARTITION_ID_PRESENT != 0 {
            Some(read_i64(encoded, 17)?)
        } else {
            None
        };
        let partition_name_len = read_u32(encoded, 25)? as usize;
        let descriptor_len = read_u32(encoded, 29)? as usize;
        let statistics_flags = encoded[33];
        let known_statistics_flags = STATISTICS_ROWS_PRESENT | STATISTICS_BYTES_PRESENT;
        if statistics_flags & !known_statistics_flags != 0 {
            return Err(FlussLakeError::InvalidSplit(format!(
                "split envelope contains unknown statistics flags 0x{statistics_flags:02x}"
            )));
        }

        let estimated_rows = (statistics_flags & STATISTICS_ROWS_PRESENT != 0)
            .then(|| read_u64(encoded, 34))
            .transpose()?;
        let estimated_size = (statistics_flags & STATISTICS_BYTES_PRESENT != 0)
            .then(|| read_u64(encoded, 42))
            .transpose()?;
        let payload_start = UNION_READ_SPLIT_HEADER_SIZE;
        let split_id_end = payload_start + split_id_len;
        let partition_name_end = split_id_end + partition_name_len;
        let expected_len = partition_name_end
            .checked_add(descriptor_len)
            .ok_or_else(|| {
                FlussLakeError::InvalidSplit("decoded split size overflows usize".to_string())
            })?;
        if encoded.len() != expected_len {
            return Err(FlussLakeError::InvalidSplit(format!(
                "split envelope length mismatch: expected {expected_len} bytes, got {}",
                encoded.len()
            )));
        }

        let split_id = std::str::from_utf8(&encoded[payload_start..split_id_end])
            .map_err(|error| {
                FlussLakeError::InvalidSplit(format!("split id is not valid UTF-8: {error}"))
            })?
            .to_string();
        let partition_name = if partition_flags & PARTITION_NAME_PRESENT != 0 {
            Some(
                std::str::from_utf8(&encoded[split_id_end..partition_name_end])
                    .map_err(|error| {
                        FlussLakeError::InvalidSplit(format!(
                            "partition name is not valid UTF-8: {error}"
                        ))
                    })?
                    .to_string(),
            )
        } else {
            None
        };
        let partition = if partition_flags & PARTITION_PRESENT != 0 {
            Some(FlussLakePartitionIdentity::new(
                partition_id,
                partition_name,
            ))
        } else {
            None
        };
        let execution_descriptor = encoded[partition_name_end..expected_len].to_vec();
        Self::try_new(
            split_id,
            bucket_id,
            partition,
            descriptor_version,
            execution_descriptor,
            FlussLakeReadStatistics::new(estimated_rows, estimated_size),
        )
    }
}

fn encode_partition(partition: &Option<FlussLakePartitionIdentity>) -> (u8, [u8; 8], Vec<u8>) {
    let mut flags = 0;
    let mut id_bytes = [0u8; 8];
    let mut name_bytes = Vec::new();
    let Some(partition) = partition else {
        return (flags, id_bytes, name_bytes);
    };
    flags |= PARTITION_PRESENT;
    if let Some(partition_id) = partition.partition_id() {
        flags |= PARTITION_ID_PRESENT;
        id_bytes = partition_id.to_le_bytes();
    }
    if let Some(name) = partition.partition_name() {
        flags |= PARTITION_NAME_PRESENT;
        name_bytes = name.as_bytes().to_vec();
    }
    (flags, id_bytes, name_bytes)
}

fn read_u32(encoded: &[u8], offset: usize) -> FlussLakeResult<u32> {
    let bytes = encoded
        .get(offset..offset + size_of::<u32>())
        .ok_or_else(|| FlussLakeError::InvalidSplit("split envelope is truncated".to_string()))?;
    Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| {
        FlussLakeError::InvalidSplit("invalid u32 in split envelope".to_string())
    })?))
}

fn read_i32(encoded: &[u8], offset: usize) -> FlussLakeResult<i32> {
    let bytes = encoded
        .get(offset..offset + size_of::<i32>())
        .ok_or_else(|| FlussLakeError::InvalidSplit("split envelope is truncated".to_string()))?;
    Ok(i32::from_le_bytes(bytes.try_into().map_err(|_| {
        FlussLakeError::InvalidSplit("invalid i32 in split envelope".to_string())
    })?))
}

fn read_i64(encoded: &[u8], offset: usize) -> FlussLakeResult<i64> {
    let bytes = encoded
        .get(offset..offset + size_of::<i64>())
        .ok_or_else(|| FlussLakeError::InvalidSplit("split envelope is truncated".to_string()))?;
    Ok(i64::from_le_bytes(bytes.try_into().map_err(|_| {
        FlussLakeError::InvalidSplit("invalid i64 in split envelope".to_string())
    })?))
}

fn read_u64(encoded: &[u8], offset: usize) -> FlussLakeResult<u64> {
    let bytes = encoded
        .get(offset..offset + size_of::<u64>())
        .ok_or_else(|| FlussLakeError::InvalidSplit("split envelope is truncated".to_string()))?;
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| {
        FlussLakeError::InvalidSplit("invalid u64 in split envelope".to_string())
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split_descriptor::{AppendLogSplitDescriptor, SplitDescriptor};
    use fluss::metadata::TablePath;

    fn testing_execution_descriptor() -> Vec<u8> {
        SplitDescriptor::AppendLog(
            AppendLogSplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                1,
                fluss::metadata::TableBucket::new(5, 0),
                0,
                1,
                None,
            )
            .unwrap(),
        )
        .encode()
        .unwrap()
    }

    #[test]
    fn rejects_empty_split_identity_and_descriptor() {
        assert!(matches!(
            FlussLakeReadSplit::try_new(
                String::new(),
                0,
                None,
                CURRENT_FLUSS_LAKE_SPLIT_VERSION,
                vec![1],
                FlussLakeReadStatistics::default()
            ),
            Err(FlussLakeError::InvalidSplit(_))
        ));
        assert!(matches!(
            FlussLakeReadSplit::try_new(
                "split".to_string(),
                0,
                None,
                CURRENT_FLUSS_LAKE_SPLIT_VERSION,
                Vec::new(),
                FlussLakeReadStatistics::default()
            ),
            Err(FlussLakeError::InvalidSplit(_))
        ));
    }

    #[test]
    fn split_encoding_round_trips_opaque_descriptor() {
        let split = FlussLakeReadSplit::try_new(
            "partition=dt%3D2026-07-28/bucket=3".to_string(),
            3,
            Some(FlussLakePartitionIdentity::new(
                Some(7),
                Some("dt=2026-07-28".to_string()),
            )),
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            testing_execution_descriptor(),
            FlussLakeReadStatistics::new(Some(42), None),
        )
        .unwrap();

        let decoded = FlussLakeReadSplit::decode(&split.encode().unwrap()).unwrap();

        assert_eq!(decoded, split);
    }

    #[test]
    fn rejects_unknown_split_version() {
        let split = FlussLakeReadSplit::try_new(
            "bucket-0".to_string(),
            0,
            None,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            testing_execution_descriptor(),
            FlussLakeReadStatistics::default(),
        )
        .unwrap();
        let mut encoded = split.encode().unwrap();
        encoded[4..8].copy_from_slice(&(CURRENT_FLUSS_LAKE_SPLIT_VERSION + 1).to_le_bytes());

        assert!(matches!(
            FlussLakeReadSplit::decode(&encoded),
            Err(FlussLakeError::UnsupportedSplitVersion { version: 3 })
        ));
    }

    #[test]
    fn rejects_malformed_split_envelopes() {
        let split = FlussLakeReadSplit::try_new(
            "bucket-0".to_string(),
            0,
            None,
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            testing_execution_descriptor(),
            FlussLakeReadStatistics::new(None, Some(20)),
        )
        .unwrap();
        let encoded = split.encode().unwrap();

        assert!(matches!(
            FlussLakeReadSplit::decode(&encoded[..encoded.len() - 1]),
            Err(FlussLakeError::InvalidSplit(_))
        ));

        let mut invalid_magic = encoded.clone();
        invalid_magic[0] = b'X';
        assert!(matches!(
            FlussLakeReadSplit::decode(&invalid_magic),
            Err(FlussLakeError::InvalidSplit(_))
        ));

        let mut unknown_partition_flags = encoded.clone();
        unknown_partition_flags[16] = 1 << 7;
        assert!(matches!(
            FlussLakeReadSplit::decode(&unknown_partition_flags),
            Err(FlussLakeError::InvalidSplit(_))
        ));

        let mut unknown_statistics_flags = encoded;
        unknown_statistics_flags[33] = 1 << 7;
        assert!(matches!(
            FlussLakeReadSplit::decode(&unknown_statistics_flags),
            Err(FlussLakeError::InvalidSplit(_))
        ));
    }
}
