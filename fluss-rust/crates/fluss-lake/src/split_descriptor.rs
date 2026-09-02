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

use crate::{FlussLakeError, Result};
use fluss::metadata::{TableBucket, TablePath};
use std::collections::HashSet;

const SPLIT_DESCRIPTOR_MAGIC: [u8; 4] = *b"URD1";
const PARTITIONED: u8 = 1;
const SNAPSHOT_PRESENT: u8 = 1 << 1;
const PRIMARY_KEY_TABLE: u8 = 1 << 2;
const LIVE_PARTITION_ID_PRESENT: u8 = 1 << 3;
const SPLIT_DESCRIPTOR_HEADER_SIZE: usize = 69;

/// Frozen execution state for one logical `(partition, bucket)` split.
///
/// Projection, filtering, batching, and lake-only mode remain properties of
/// [`crate::FlussLakeScan`]. Physical lake splits are opaque payloads grouped
/// into this logical unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitDescriptor {
    table_path: TablePath,
    schema_id: i32,
    partitioned: bool,
    table_bucket: TableBucket,
    start_offset: i64,
    stop_offset: i64,
    snapshot_id: Option<i64>,
    lake_splits: Vec<String>,
    primary_key_indexes: Vec<usize>,
}

#[allow(dead_code)]
impl SplitDescriptor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_new(
        table_path: TablePath,
        schema_id: i32,
        partitioned: bool,
        table_bucket: TableBucket,
        start_offset: i64,
        stop_offset: i64,
        snapshot_id: Option<i64>,
        lake_splits: Vec<String>,
        primary_key_indexes: Vec<usize>,
    ) -> Result<Self> {
        if table_path.database().is_empty() || table_path.table().is_empty() {
            return Err(invalid_descriptor(
                "database and table names must not be empty",
            ));
        }
        if schema_id < 0 {
            return Err(invalid_descriptor(format!(
                "schema id must be non-negative, got {schema_id}"
            )));
        }
        if table_bucket.table_id() < 0 {
            return Err(invalid_descriptor(format!(
                "table id must be non-negative, got {}",
                table_bucket.table_id()
            )));
        }
        if let Some(partition_id) = table_bucket.partition_id() {
            if !partitioned {
                return Err(invalid_descriptor(
                    "an unpartitioned split must not carry a live partition id",
                ));
            }
            if partition_id < 0 {
                return Err(invalid_descriptor(format!(
                    "partition id must be non-negative, got {partition_id}"
                )));
            }
        }
        if table_bucket.bucket_id() < 0 {
            return Err(invalid_descriptor(format!(
                "bucket id must be non-negative, got {}",
                table_bucket.bucket_id()
            )));
        }
        if start_offset < 0 || stop_offset < 0 || start_offset > stop_offset {
            return Err(invalid_descriptor(format!(
                "logical changelog range is invalid: [{start_offset}, {stop_offset})"
            )));
        }
        if partitioned && table_bucket.partition_id().is_none() && start_offset != stop_offset {
            return Err(invalid_descriptor(
                "a partition that no longer exists in Fluss cannot carry a log range",
            ));
        }
        if let Some(snapshot_id) = snapshot_id
            && snapshot_id < 0
        {
            return Err(invalid_descriptor(format!(
                "lake snapshot id must be non-negative, got {snapshot_id}"
            )));
        }
        if !lake_splits.is_empty() && snapshot_id.is_none() {
            return Err(invalid_descriptor(
                "lake splits require a pinned snapshot id",
            ));
        }
        if lake_splits.iter().any(String::is_empty) {
            return Err(invalid_descriptor("lake split payloads must not be empty"));
        }
        let mut seen = HashSet::with_capacity(primary_key_indexes.len());
        if primary_key_indexes.iter().any(|index| !seen.insert(*index)) {
            return Err(invalid_descriptor(
                "primary-key indexes must not contain duplicates",
            ));
        }

        Ok(Self {
            table_path,
            schema_id,
            partitioned,
            table_bucket,
            start_offset,
            stop_offset,
            snapshot_id,
            lake_splits,
            primary_key_indexes,
        })
    }

    pub(crate) fn table_path(&self) -> &TablePath {
        &self.table_path
    }

    pub(crate) fn schema_id(&self) -> i32 {
        self.schema_id
    }

    pub(crate) fn is_partitioned(&self) -> bool {
        self.partitioned
    }

    pub(crate) fn table_bucket(&self) -> &TableBucket {
        &self.table_bucket
    }

    pub(crate) fn start_offset(&self) -> i64 {
        self.start_offset
    }

    pub(crate) fn stop_offset(&self) -> i64 {
        self.stop_offset
    }

    pub(crate) fn snapshot_id(&self) -> Option<i64> {
        self.snapshot_id
    }

    pub(crate) fn lake_splits(&self) -> &[String] {
        &self.lake_splits
    }

    pub(crate) fn primary_key_indexes(&self) -> &[usize] {
        &self.primary_key_indexes
    }

    pub(crate) fn is_primary_key(&self) -> bool {
        !self.primary_key_indexes.is_empty()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.start_offset == self.stop_offset && self.lake_splits.is_empty()
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let database = self.table_path.database().as_bytes();
        let table = self.table_path.table().as_bytes();
        let database_len = wire_len(database.len(), "database name")?;
        let table_len = wire_len(table.len(), "table name")?;
        let split_count = wire_len(self.lake_splits.len(), "lake splits")?;
        let pk_count = wire_len(self.primary_key_indexes.len(), "primary-key indexes")?;

        let mut flags = 0;
        if self.partitioned {
            flags |= PARTITIONED;
        }
        if self.table_bucket.partition_id().is_some() {
            flags |= LIVE_PARTITION_ID_PRESENT;
        }
        if self.snapshot_id.is_some() {
            flags |= SNAPSHOT_PRESENT;
        }
        if self.is_primary_key() {
            flags |= PRIMARY_KEY_TABLE;
        }

        let mut encoded =
            Vec::with_capacity(SPLIT_DESCRIPTOR_HEADER_SIZE + database.len() + table.len());
        encoded.extend_from_slice(&SPLIT_DESCRIPTOR_MAGIC);
        encoded.push(flags);
        encoded.extend_from_slice(&self.table_bucket.table_id().to_le_bytes());
        encoded.extend_from_slice(&self.schema_id.to_le_bytes());
        encoded.extend_from_slice(
            &self
                .table_bucket
                .partition_id()
                .unwrap_or_default()
                .to_le_bytes(),
        );
        encoded.extend_from_slice(&self.table_bucket.bucket_id().to_le_bytes());
        encoded.extend_from_slice(&self.start_offset.to_le_bytes());
        encoded.extend_from_slice(&self.stop_offset.to_le_bytes());
        encoded.extend_from_slice(&self.snapshot_id.unwrap_or_default().to_le_bytes());
        encoded.extend_from_slice(&database_len.to_le_bytes());
        encoded.extend_from_slice(&table_len.to_le_bytes());
        encoded.extend_from_slice(&split_count.to_le_bytes());
        encoded.extend_from_slice(&pk_count.to_le_bytes());
        encoded.extend_from_slice(database);
        encoded.extend_from_slice(table);
        for split in &self.lake_splits {
            encoded.extend_from_slice(&wire_len(split.len(), "lake split payload")?.to_le_bytes());
            encoded.extend_from_slice(split.as_bytes());
        }
        for pk_index in &self.primary_key_indexes {
            let pk_index = u32::try_from(*pk_index).map_err(|_| {
                invalid_descriptor(format!(
                    "primary-key index {pk_index} exceeds the split wire format limit"
                ))
            })?;
            encoded.extend_from_slice(&pk_index.to_le_bytes());
        }
        Ok(encoded)
    }

    pub(crate) fn decode(encoded: &[u8]) -> Result<Self> {
        if encoded.len() < SPLIT_DESCRIPTOR_HEADER_SIZE {
            return Err(invalid_descriptor(format!(
                "split descriptor is truncated: expected at least {SPLIT_DESCRIPTOR_HEADER_SIZE} bytes, got {}",
                encoded.len()
            )));
        }
        let mut reader = DescriptorReader::new(encoded);
        reader.expect_bytes(&SPLIT_DESCRIPTOR_MAGIC, "split descriptor magic")?;
        let flags = reader.read_u8("split flags")?;
        let known_flags =
            PARTITIONED | SNAPSHOT_PRESENT | PRIMARY_KEY_TABLE | LIVE_PARTITION_ID_PRESENT;
        if flags & !known_flags != 0 {
            return Err(invalid_descriptor(format!(
                "split descriptor contains unknown flags 0x{flags:02x}"
            )));
        }

        let table_id = reader.read_i64("table id")?;
        let schema_id = reader.read_i32("schema id")?;
        let encoded_partition_id = reader.read_i64("partition id")?;
        let bucket_id = reader.read_i32("bucket id")?;
        let start_offset = reader.read_i64("start offset")?;
        let stop_offset = reader.read_i64("stop offset")?;
        let encoded_snapshot_id = reader.read_i64("lake snapshot id")?;
        let database_len = reader.read_u32("database name length")? as usize;
        let table_len = reader.read_u32("table name length")? as usize;
        let split_count = reader.read_u32("lake split count")? as usize;
        let pk_count = reader.read_u32("primary-key index count")? as usize;

        let minimum_variable_bytes = database_len
            .checked_add(table_len)
            .and_then(|size| size.checked_add(split_count.checked_mul(4)?))
            .and_then(|size| size.checked_add(pk_count.checked_mul(4)?))
            .ok_or_else(|| invalid_descriptor("split descriptor lengths overflow usize"))?;
        if minimum_variable_bytes > reader.remaining() {
            return Err(invalid_descriptor(format!(
                "split descriptor counts require at least {minimum_variable_bytes} bytes, but only {} remain",
                reader.remaining()
            )));
        }

        let database = reader.read_string(database_len, "database name")?;
        let table = reader.read_string(table_len, "table name")?;
        let mut lake_splits = Vec::with_capacity(split_count);
        for _ in 0..split_count {
            let split_len = reader.read_u32("lake split payload length")? as usize;
            lake_splits.push(reader.read_string(split_len, "lake split payload")?);
        }
        let mut primary_key_indexes = Vec::with_capacity(pk_count);
        for _ in 0..pk_count {
            primary_key_indexes.push(reader.read_u32("primary-key field index")? as usize);
        }
        let primary_key = flags & PRIMARY_KEY_TABLE != 0;
        if primary_key == primary_key_indexes.is_empty() {
            return Err(invalid_descriptor(
                "primary-key flag and primary-key index count are inconsistent",
            ));
        }
        reader.finish()?;

        let partitioned = flags & PARTITIONED != 0;
        let partition_id = if flags & LIVE_PARTITION_ID_PRESENT != 0 {
            if !partitioned {
                return Err(invalid_descriptor(
                    "a live partition id requires the partitioned flag",
                ));
            }
            Some(encoded_partition_id)
        } else {
            if encoded_partition_id != 0 {
                return Err(invalid_descriptor(
                    "partition id must be zero when the partition flag is absent",
                ));
            }
            None
        };
        let snapshot_id = if flags & SNAPSHOT_PRESENT != 0 {
            Some(encoded_snapshot_id)
        } else {
            if encoded_snapshot_id != 0 {
                return Err(invalid_descriptor(
                    "snapshot id must be zero when the snapshot flag is absent",
                ));
            }
            None
        };
        Self::try_new(
            TablePath::new(database, table),
            schema_id,
            partitioned,
            TableBucket::new_with_partition(table_id, partition_id, bucket_id),
            start_offset,
            stop_offset,
            snapshot_id,
            lake_splits,
            primary_key_indexes,
        )
    }
}

struct DescriptorReader<'a> {
    encoded: &'a [u8],
    offset: usize,
}

impl<'a> DescriptorReader<'a> {
    fn new(encoded: &'a [u8]) -> Self {
        Self { encoded, offset: 0 }
    }

    fn expect_bytes(&mut self, expected: &[u8], field: &str) -> Result<()> {
        let actual = self.read_exact(expected.len(), field)?;
        if actual != expected {
            return Err(invalid_descriptor(format!("{field} is invalid")));
        }
        Ok(())
    }

    fn read_u8(&mut self, field: &str) -> Result<u8> {
        Ok(self.read_exact(size_of::<u8>(), field)?[0])
    }

    fn read_u32(&mut self, field: &str) -> Result<u32> {
        let bytes = self.read_exact(size_of::<u32>(), field)?;
        Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| {
            invalid_descriptor(format!("{field} is not a valid u32"))
        })?))
    }

    fn read_i32(&mut self, field: &str) -> Result<i32> {
        let bytes = self.read_exact(size_of::<i32>(), field)?;
        Ok(i32::from_le_bytes(bytes.try_into().map_err(|_| {
            invalid_descriptor(format!("{field} is not a valid i32"))
        })?))
    }

    fn read_i64(&mut self, field: &str) -> Result<i64> {
        let bytes = self.read_exact(size_of::<i64>(), field)?;
        Ok(i64::from_le_bytes(bytes.try_into().map_err(|_| {
            invalid_descriptor(format!("{field} is not a valid i64"))
        })?))
    }

    fn read_string(&mut self, len: usize, field: &str) -> Result<String> {
        let bytes = self.read_exact(len, field)?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|error| invalid_descriptor(format!("{field} is not valid UTF-8: {error}")))
    }

    fn read_exact(&mut self, len: usize, field: &str) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| invalid_descriptor(format!("{field} length overflows usize")))?;
        let bytes = self.encoded.get(self.offset..end).ok_or_else(|| {
            invalid_descriptor(format!(
                "descriptor is truncated while reading {field}: need {len} bytes"
            ))
        })?;
        self.offset = end;
        Ok(bytes)
    }

    fn finish(self) -> Result<()> {
        if self.offset != self.encoded.len() {
            return Err(invalid_descriptor(format!(
                "descriptor contains {} trailing bytes",
                self.encoded.len() - self.offset
            )));
        }
        Ok(())
    }

    fn remaining(&self) -> usize {
        self.encoded.len() - self.offset
    }
}

fn wire_len(len: usize, field: &str) -> Result<u32> {
    u32::try_from(len)
        .map_err(|_| invalid_descriptor(format!("{field} exceeds the split wire format limit")))
}

fn invalid_descriptor(message: impl Into<String>) -> FlussLakeError {
    FlussLakeError::Internal(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn descriptor(primary_key_indexes: Vec<usize>) -> SplitDescriptor {
        SplitDescriptor::try_new(
            TablePath::new("fluss", "orders"),
            3,
            true,
            TableBucket::new_with_partition(7, Some(11), 2),
            12,
            20,
            Some(42),
            vec![
                "{\"bucket\":2,\"file\":\"a\"}".to_string(),
                "{\"bucket\":2,\"file\":\"b\"}".to_string(),
            ],
            primary_key_indexes,
        )
        .unwrap()
    }

    #[test]
    fn descriptor_round_trips_append_and_primary_key_forms() {
        for descriptor in [descriptor(Vec::new()), descriptor(vec![0, 1])] {
            assert_eq!(
                SplitDescriptor::decode(&descriptor.encode().unwrap()).unwrap(),
                descriptor
            );
        }
    }

    #[test]
    fn version_one_descriptor_fixture_is_stable() {
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
        .unwrap();

        assert_eq!(
            hex(&descriptor.encode().unwrap()),
            "555244310005000000000000000100000000000000000000000000000000000000000000000a00000000000000000000000000000005000000060000000000000000000000666c7573736f7264657273"
        );
    }

    #[test]
    fn descriptor_requires_snapshot_for_physical_lake_splits() {
        assert!(matches!(
            SplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                false,
                TableBucket::new(7, 2),
                0,
                10,
                None,
                vec!["{}".to_string()],
                Vec::new(),
            ),
            Err(FlussLakeError::Internal(_))
        ));
    }

    #[test]
    fn descriptor_rejects_invalid_ranges_payloads_and_keys() {
        assert!(matches!(
            SplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                false,
                TableBucket::new(7, 2),
                21,
                20,
                None,
                Vec::new(),
                Vec::new(),
            ),
            Err(FlussLakeError::Internal(_))
        ));
        assert!(matches!(
            SplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                false,
                TableBucket::new(7, 2),
                0,
                20,
                Some(42),
                vec![String::new()],
                vec![0],
            ),
            Err(FlussLakeError::Internal(_))
        ));
        assert!(matches!(
            SplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                false,
                TableBucket::new(7, 2),
                0,
                20,
                None,
                Vec::new(),
                vec![0, 0],
            ),
            Err(FlussLakeError::Internal(_))
        ));
    }

    #[test]
    fn descriptor_rejects_unknown_flags_trailing_bytes_and_truncation() {
        let encoded = descriptor(vec![0, 1]).encode().unwrap();

        let mut unknown_flags = encoded.clone();
        unknown_flags[4] |= 1 << 7;
        assert!(matches!(
            SplitDescriptor::decode(&unknown_flags),
            Err(FlussLakeError::Internal(_))
        ));

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(matches!(
            SplitDescriptor::decode(&trailing),
            Err(FlussLakeError::Internal(_))
        ));

        assert!(matches!(
            SplitDescriptor::decode(&encoded[..encoded.len() - 1]),
            Err(FlussLakeError::Internal(_))
        ));
    }

    #[test]
    fn descriptor_rejects_inconsistent_optional_flags() {
        let encoded = descriptor(vec![0, 1]).encode().unwrap();

        let mut missing_partition_flag = encoded.clone();
        missing_partition_flag[4] &= !PARTITIONED;
        assert!(matches!(
            SplitDescriptor::decode(&missing_partition_flag),
            Err(FlussLakeError::Internal(_))
        ));

        let mut missing_snapshot_flag = encoded.clone();
        missing_snapshot_flag[4] &= !SNAPSHOT_PRESENT;
        assert!(matches!(
            SplitDescriptor::decode(&missing_snapshot_flag),
            Err(FlussLakeError::Internal(_))
        ));

        let mut missing_primary_key_flag = encoded;
        missing_primary_key_flag[4] &= !PRIMARY_KEY_TABLE;
        assert!(matches!(
            SplitDescriptor::decode(&missing_primary_key_flag),
            Err(FlussLakeError::Internal(_))
        ));
    }

    #[test]
    fn descriptor_rejects_untrusted_counts_before_allocating() {
        let mut encoded = descriptor(vec![0, 1]).encode().unwrap();
        encoded[61..65].copy_from_slice(&u32::MAX.to_le_bytes());

        assert!(matches!(
            SplitDescriptor::decode(&encoded),
            Err(FlussLakeError::Internal(_))
        ));
    }

    #[test]
    fn partitioned_lake_only_descriptor_needs_no_synthetic_partition_id() {
        let descriptor = SplitDescriptor::try_new(
            TablePath::new("fluss", "orders"),
            3,
            true,
            TableBucket::new(7, 2),
            0,
            0,
            Some(42),
            vec!["{}".to_string()],
            Vec::new(),
        )
        .unwrap();

        assert!(descriptor.is_partitioned());
        assert_eq!(descriptor.table_bucket().partition_id(), None);
        assert_eq!(
            SplitDescriptor::decode(&descriptor.encode().unwrap()).unwrap(),
            descriptor
        );
    }

    #[test]
    fn partitioned_descriptor_without_live_id_rejects_log_ranges() {
        assert!(matches!(
            SplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                true,
                TableBucket::new(7, 2),
                0,
                1,
                Some(42),
                vec!["{}".to_string()],
                Vec::new(),
            ),
            Err(FlussLakeError::Internal(_))
        ));
    }
}
