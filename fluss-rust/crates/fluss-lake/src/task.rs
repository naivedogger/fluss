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

use crate::{UnionReadError, UnionReadResult};
use fluss::metadata::{TableBucket, TablePath};
use std::collections::HashSet;

const TASK_DESCRIPTOR_MAGIC: [u8; 4] = *b"URD1";
const APPEND_LOG_TASK_KIND: u8 = 1;
const PARTITION_PRESENT: u8 = 1;
const PROJECTION_PRESENT: u8 = 1 << 1;
const APPEND_LOG_HEADER_SIZE: usize = 58;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskDescriptor {
    AppendLog(AppendLogTaskDescriptor),
}

impl TaskDescriptor {
    pub(crate) fn encode(&self) -> UnionReadResult<Vec<u8>> {
        match self {
            Self::AppendLog(descriptor) => descriptor.encode(),
        }
    }

    pub(crate) fn decode(encoded: &[u8]) -> UnionReadResult<Self> {
        if encoded.len() < TASK_DESCRIPTOR_MAGIC.len() + 1 {
            return Err(invalid_descriptor("descriptor is truncated"));
        }
        if encoded[..TASK_DESCRIPTOR_MAGIC.len()] != TASK_DESCRIPTOR_MAGIC {
            return Err(invalid_descriptor("descriptor has an invalid magic header"));
        }

        match encoded[TASK_DESCRIPTOR_MAGIC.len()] {
            APPEND_LOG_TASK_KIND => AppendLogTaskDescriptor::decode(encoded).map(Self::AppendLog),
            kind => Err(invalid_descriptor(format!(
                "descriptor contains unknown task kind {kind}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppendLogTaskDescriptor {
    table_path: TablePath,
    schema_id: i32,
    table_bucket: TableBucket,
    start_offset: i64,
    stop_offset: i64,
    output_projection: Option<Vec<usize>>,
}

impl AppendLogTaskDescriptor {
    pub(crate) fn try_new(
        table_path: TablePath,
        schema_id: i32,
        table_bucket: TableBucket,
        start_offset: i64,
        stop_offset: i64,
        output_projection: Option<Vec<usize>>,
    ) -> UnionReadResult<Self> {
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
        if let Some(partition_id) = table_bucket.partition_id()
            && partition_id < 0
        {
            return Err(invalid_descriptor(format!(
                "partition id must be non-negative, got {partition_id}"
            )));
        }
        if table_bucket.bucket_id() < 0 {
            return Err(invalid_descriptor(format!(
                "bucket id must be non-negative, got {}",
                table_bucket.bucket_id()
            )));
        }
        if start_offset < 0 || stop_offset < 0 {
            return Err(invalid_descriptor(format!(
                "append-log range must be non-negative, got [{start_offset}, {stop_offset})"
            )));
        }
        if start_offset > stop_offset {
            return Err(invalid_descriptor(format!(
                "append-log start offset {start_offset} exceeds stop offset {stop_offset}"
            )));
        }
        if let Some(projection) = &output_projection {
            if projection.is_empty() {
                return Err(invalid_descriptor(
                    "output projection must not be empty when present",
                ));
            }
            let mut seen = HashSet::with_capacity(projection.len());
            if projection.iter().any(|index| !seen.insert(*index)) {
                return Err(invalid_descriptor(
                    "output projection must not contain duplicate field indexes",
                ));
            }
        }

        Ok(Self {
            table_path,
            schema_id,
            table_bucket,
            start_offset,
            stop_offset,
            output_projection,
        })
    }

    pub(crate) fn table_path(&self) -> &TablePath {
        &self.table_path
    }

    pub(crate) fn schema_id(&self) -> i32 {
        self.schema_id
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

    pub(crate) fn output_projection(&self) -> Option<&[usize]> {
        self.output_projection.as_deref()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.start_offset == self.stop_offset
    }

    fn encode(&self) -> UnionReadResult<Vec<u8>> {
        let database = self.table_path.database().as_bytes();
        let table = self.table_path.table().as_bytes();
        let database_len = wire_len(database.len(), "database name")?;
        let table_len = wire_len(table.len(), "table name")?;
        let projection_count = match &self.output_projection {
            Some(projection) => wire_len(projection.len(), "output projection")?,
            None => 0,
        };
        let projection_bytes = (projection_count as usize)
            .checked_mul(size_of::<u32>())
            .ok_or_else(|| invalid_descriptor("projection size overflows usize"))?;
        let capacity = APPEND_LOG_HEADER_SIZE
            .checked_add(database.len())
            .and_then(|size| size.checked_add(table.len()))
            .and_then(|size| size.checked_add(projection_bytes))
            .ok_or_else(|| invalid_descriptor("encoded descriptor size overflows usize"))?;

        let mut flags = 0;
        if self.table_bucket.partition_id().is_some() {
            flags |= PARTITION_PRESENT;
        }
        if self.output_projection.is_some() {
            flags |= PROJECTION_PRESENT;
        }

        let mut encoded = Vec::with_capacity(capacity);
        encoded.extend_from_slice(&TASK_DESCRIPTOR_MAGIC);
        encoded.push(APPEND_LOG_TASK_KIND);
        encoded.push(flags);
        encoded.extend_from_slice(&database_len.to_le_bytes());
        encoded.extend_from_slice(&table_len.to_le_bytes());
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
        encoded.extend_from_slice(&projection_count.to_le_bytes());
        encoded.extend_from_slice(database);
        encoded.extend_from_slice(table);
        if let Some(projection) = &self.output_projection {
            for field_index in projection {
                let field_index = u32::try_from(*field_index).map_err(|_| {
                    invalid_descriptor(format!(
                        "field index {field_index} exceeds the task wire format limit"
                    ))
                })?;
                encoded.extend_from_slice(&field_index.to_le_bytes());
            }
        }
        Ok(encoded)
    }

    fn decode(encoded: &[u8]) -> UnionReadResult<Self> {
        if encoded.len() < APPEND_LOG_HEADER_SIZE {
            return Err(invalid_descriptor(format!(
                "append-log descriptor is truncated: expected at least {APPEND_LOG_HEADER_SIZE} bytes, got {}",
                encoded.len()
            )));
        }

        let mut reader = DescriptorReader::new(encoded);
        reader.expect_bytes(&TASK_DESCRIPTOR_MAGIC, "task descriptor magic")?;
        let kind = reader.read_u8("task kind")?;
        if kind != APPEND_LOG_TASK_KIND {
            return Err(invalid_descriptor(format!(
                "expected append-log task kind {APPEND_LOG_TASK_KIND}, got {kind}"
            )));
        }
        let flags = reader.read_u8("task flags")?;
        let known_flags = PARTITION_PRESENT | PROJECTION_PRESENT;
        if flags & !known_flags != 0 {
            return Err(invalid_descriptor(format!(
                "append-log descriptor contains unknown flags 0x{flags:02x}"
            )));
        }

        let database_len = reader.read_u32("database name length")? as usize;
        let table_len = reader.read_u32("table name length")? as usize;
        let table_id = reader.read_i64("table id")?;
        let schema_id = reader.read_i32("schema id")?;
        let encoded_partition_id = reader.read_i64("partition id")?;
        let bucket_id = reader.read_i32("bucket id")?;
        let start_offset = reader.read_i64("start offset")?;
        let stop_offset = reader.read_i64("stop offset")?;
        let projection_count = reader.read_u32("projection count")? as usize;

        let database = reader.read_string(database_len, "database name")?;
        let table = reader.read_string(table_len, "table name")?;
        let projection_present = flags & PROJECTION_PRESENT != 0;
        if projection_present != (projection_count > 0) {
            return Err(invalid_descriptor(
                "projection flag and projection count are inconsistent",
            ));
        }
        let projection_bytes = projection_count
            .checked_mul(size_of::<u32>())
            .ok_or_else(|| invalid_descriptor("projection byte count overflows usize"))?;
        if reader.remaining() != projection_bytes {
            return Err(invalid_descriptor(format!(
                "projection length mismatch: expected {projection_bytes} bytes, got {}",
                reader.remaining()
            )));
        }
        let output_projection = if projection_present {
            let mut projection = Vec::with_capacity(projection_count);
            for _ in 0..projection_count {
                projection.push(reader.read_u32("projection field index")? as usize);
            }
            Some(projection)
        } else {
            None
        };
        reader.finish()?;

        let partition_id = if flags & PARTITION_PRESENT != 0 {
            Some(encoded_partition_id)
        } else {
            if encoded_partition_id != 0 {
                return Err(invalid_descriptor(
                    "partition id must be zero when the partition flag is absent",
                ));
            }
            None
        };
        Self::try_new(
            TablePath::new(database, table),
            schema_id,
            TableBucket::new_with_partition(table_id, partition_id, bucket_id),
            start_offset,
            stop_offset,
            output_projection,
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

    fn expect_bytes(&mut self, expected: &[u8], field: &str) -> UnionReadResult<()> {
        let actual = self.read_exact(expected.len(), field)?;
        if actual != expected {
            return Err(invalid_descriptor(format!("{field} is invalid")));
        }
        Ok(())
    }

    fn read_u8(&mut self, field: &str) -> UnionReadResult<u8> {
        Ok(self.read_exact(size_of::<u8>(), field)?[0])
    }

    fn read_u32(&mut self, field: &str) -> UnionReadResult<u32> {
        let bytes = self.read_exact(size_of::<u32>(), field)?;
        Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| {
            invalid_descriptor(format!("{field} is not a valid u32"))
        })?))
    }

    fn read_i32(&mut self, field: &str) -> UnionReadResult<i32> {
        let bytes = self.read_exact(size_of::<i32>(), field)?;
        Ok(i32::from_le_bytes(bytes.try_into().map_err(|_| {
            invalid_descriptor(format!("{field} is not a valid i32"))
        })?))
    }

    fn read_i64(&mut self, field: &str) -> UnionReadResult<i64> {
        let bytes = self.read_exact(size_of::<i64>(), field)?;
        Ok(i64::from_le_bytes(bytes.try_into().map_err(|_| {
            invalid_descriptor(format!("{field} is not a valid i64"))
        })?))
    }

    fn read_string(&mut self, len: usize, field: &str) -> UnionReadResult<String> {
        let bytes = self.read_exact(len, field)?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|error| invalid_descriptor(format!("{field} is not valid UTF-8: {error}")))
    }

    fn read_exact(&mut self, len: usize, field: &str) -> UnionReadResult<&'a [u8]> {
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

    fn finish(self) -> UnionReadResult<()> {
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

fn wire_len(len: usize, field: &str) -> UnionReadResult<u32> {
    u32::try_from(len)
        .map_err(|_| invalid_descriptor(format!("{field} exceeds the task wire format limit")))
}

fn invalid_descriptor(message: impl Into<String>) -> UnionReadError {
    UnionReadError::InvalidTask(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(projection: Option<Vec<usize>>) -> AppendLogTaskDescriptor {
        AppendLogTaskDescriptor::try_new(
            TablePath::new("fluss", "orders"),
            3,
            TableBucket::new_with_partition(7, Some(11), 2),
            12,
            20,
            projection,
        )
        .unwrap()
    }

    #[test]
    fn append_log_descriptor_round_trips() {
        let descriptor = TaskDescriptor::AppendLog(descriptor(Some(vec![2, 0])));

        assert_eq!(
            TaskDescriptor::decode(&descriptor.encode().unwrap()).unwrap(),
            descriptor
        );
    }

    #[test]
    fn append_log_descriptor_round_trips_without_partition_or_projection() {
        let descriptor = TaskDescriptor::AppendLog(
            AppendLogTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                TableBucket::new(7, 2),
                12,
                20,
                None,
            )
            .unwrap(),
        );

        assert_eq!(
            TaskDescriptor::decode(&descriptor.encode().unwrap()).unwrap(),
            descriptor
        );
    }

    #[test]
    fn rejects_invalid_append_log_ranges_and_projection() {
        assert!(matches!(
            AppendLogTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                TableBucket::new(7, 2),
                21,
                20,
                None,
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
        assert!(matches!(
            AppendLogTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                TableBucket::new(7, 2),
                12,
                20,
                Some(vec![1, 1]),
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
    }

    #[test]
    fn rejects_unknown_kind_flags_and_trailing_bytes() {
        let encoded = TaskDescriptor::AppendLog(descriptor(Some(vec![2, 0])))
            .encode()
            .unwrap();

        let mut unknown_kind = encoded.clone();
        unknown_kind[4] = 99;
        assert!(matches!(
            TaskDescriptor::decode(&unknown_kind),
            Err(UnionReadError::InvalidTask(_))
        ));

        let mut unknown_flags = encoded.clone();
        unknown_flags[5] = 1 << 7;
        assert!(matches!(
            TaskDescriptor::decode(&unknown_flags),
            Err(UnionReadError::InvalidTask(_))
        ));

        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(
            TaskDescriptor::decode(&trailing),
            Err(UnionReadError::InvalidTask(_))
        ));
    }

    #[test]
    fn rejects_untrusted_projection_count_before_allocating() {
        let mut encoded = TaskDescriptor::AppendLog(descriptor(Some(vec![2, 0])))
            .encode()
            .unwrap();
        encoded[54..58].copy_from_slice(&u32::MAX.to_le_bytes());

        assert!(matches!(
            TaskDescriptor::decode(&encoded),
            Err(UnionReadError::InvalidTask(_))
        ));
    }
}
