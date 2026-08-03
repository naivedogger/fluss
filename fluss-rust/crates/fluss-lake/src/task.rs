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
use std::collections::{BTreeMap, HashSet};

const TASK_DESCRIPTOR_MAGIC: [u8; 4] = *b"URD1";
const APPEND_LOG_TASK_KIND: u8 = 1;
const LAKE_SPLIT_TASK_KIND: u8 = 2;
const PK_HYBRID_TASK_KIND: u8 = 3;
const PARTITION_PRESENT: u8 = 1;
const PROJECTION_PRESENT: u8 = 1 << 1;
const SNAPSHOT_PRESENT: u8 = 1 << 2;
const APPEND_LOG_HEADER_SIZE: usize = 58;
const LAKE_SPLIT_HEADER_SIZE: usize = 34;
const PK_HYBRID_HEADER_SIZE: usize = 78;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskDescriptor {
    AppendLog(AppendLogTaskDescriptor),
    LakeSplit(LakeSplitTaskDescriptor),
    PkHybrid(PkHybridTaskDescriptor),
}

impl TaskDescriptor {
    pub(crate) fn encode(&self) -> UnionReadResult<Vec<u8>> {
        match self {
            Self::AppendLog(descriptor) => descriptor.encode(),
            Self::LakeSplit(descriptor) => descriptor.encode(),
            Self::PkHybrid(descriptor) => descriptor.encode(),
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
            LAKE_SPLIT_TASK_KIND => LakeSplitTaskDescriptor::decode(encoded).map(Self::LakeSplit),
            PK_HYBRID_TASK_KIND => PkHybridTaskDescriptor::decode(encoded).map(Self::PkHybrid),
            kind => Err(invalid_descriptor(format!(
                "descriptor contains unknown task kind {kind}"
            ))),
        }
    }
}

/// One immutable lake split of a frozen readable lake snapshot.
///
/// The descriptor carries everything an executor needs to reopen the lake
/// table on its own: the catalog options, the pinned snapshot id, the engine
/// scan projection resolved to lake field names, and the opaque split payload
/// produced by the lake format. It is decodable regardless of which lake
/// format features are compiled in, so an executor built without the matching
/// format reports a clear error instead of failing to parse the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LakeSplitTaskDescriptor {
    table_path: TablePath,
    snapshot_id: i64,
    catalog_options: BTreeMap<String, String>,
    projected_fields: Option<Vec<String>>,
    encoded_split: String,
}

impl LakeSplitTaskDescriptor {
    pub(crate) fn try_new(
        table_path: TablePath,
        snapshot_id: i64,
        catalog_options: BTreeMap<String, String>,
        projected_fields: Option<Vec<String>>,
        encoded_split: String,
    ) -> UnionReadResult<Self> {
        if table_path.database().is_empty() || table_path.table().is_empty() {
            return Err(invalid_descriptor(
                "database and table names must not be empty",
            ));
        }
        if snapshot_id < 0 {
            return Err(invalid_descriptor(format!(
                "lake snapshot id must be non-negative, got {snapshot_id}"
            )));
        }
        if encoded_split.is_empty() {
            return Err(invalid_descriptor("lake split payload must not be empty"));
        }
        if let Some(fields) = &projected_fields {
            if fields.is_empty() {
                return Err(invalid_descriptor(
                    "projected fields must not be empty when present",
                ));
            }
            if fields.iter().any(String::is_empty) {
                return Err(invalid_descriptor(
                    "projected field names must not be empty",
                ));
            }
        }

        Ok(Self {
            table_path,
            snapshot_id,
            catalog_options,
            projected_fields,
            encoded_split,
        })
    }

    pub(crate) fn table_path(&self) -> &TablePath {
        &self.table_path
    }

    pub(crate) fn snapshot_id(&self) -> i64 {
        self.snapshot_id
    }

    pub(crate) fn catalog_options(&self) -> &BTreeMap<String, String> {
        &self.catalog_options
    }

    pub(crate) fn projected_fields(&self) -> Option<&[String]> {
        self.projected_fields.as_deref()
    }

    pub(crate) fn encoded_split(&self) -> &str {
        &self.encoded_split
    }

    fn encode(&self) -> UnionReadResult<Vec<u8>> {
        let database = self.table_path.database().as_bytes();
        let table = self.table_path.table().as_bytes();
        let split = self.encoded_split.as_bytes();
        let database_len = wire_len(database.len(), "database name")?;
        let table_len = wire_len(table.len(), "table name")?;
        let split_len = wire_len(split.len(), "lake split payload")?;
        let projection_count = match &self.projected_fields {
            Some(fields) => wire_len(fields.len(), "projected fields")?,
            None => 0,
        };
        let option_count = wire_len(self.catalog_options.len(), "catalog options")?;

        let mut flags = 0;
        if self.projected_fields.is_some() {
            flags |= PROJECTION_PRESENT;
        }

        let mut encoded = Vec::with_capacity(LAKE_SPLIT_HEADER_SIZE + database.len() + table.len());
        encoded.extend_from_slice(&TASK_DESCRIPTOR_MAGIC);
        encoded.push(LAKE_SPLIT_TASK_KIND);
        encoded.push(flags);
        encoded.extend_from_slice(&self.snapshot_id.to_le_bytes());
        encoded.extend_from_slice(&database_len.to_le_bytes());
        encoded.extend_from_slice(&table_len.to_le_bytes());
        encoded.extend_from_slice(&split_len.to_le_bytes());
        encoded.extend_from_slice(&projection_count.to_le_bytes());
        encoded.extend_from_slice(&option_count.to_le_bytes());
        encoded.extend_from_slice(database);
        encoded.extend_from_slice(table);
        encoded.extend_from_slice(split);
        if let Some(fields) = &self.projected_fields {
            for field in fields {
                encoded.extend_from_slice(
                    &wire_len(field.len(), "projected field name")?.to_le_bytes(),
                );
                encoded.extend_from_slice(field.as_bytes());
            }
        }
        // A BTreeMap keeps the encoding deterministic: the same plan always
        // produces the same task bytes.
        for (key, value) in &self.catalog_options {
            encoded.extend_from_slice(&wire_len(key.len(), "catalog option key")?.to_le_bytes());
            encoded
                .extend_from_slice(&wire_len(value.len(), "catalog option value")?.to_le_bytes());
            encoded.extend_from_slice(key.as_bytes());
            encoded.extend_from_slice(value.as_bytes());
        }
        Ok(encoded)
    }

    fn decode(encoded: &[u8]) -> UnionReadResult<Self> {
        if encoded.len() < LAKE_SPLIT_HEADER_SIZE {
            return Err(invalid_descriptor(format!(
                "lake split descriptor is truncated: expected at least {LAKE_SPLIT_HEADER_SIZE} bytes, got {}",
                encoded.len()
            )));
        }

        let mut reader = DescriptorReader::new(encoded);
        reader.expect_bytes(&TASK_DESCRIPTOR_MAGIC, "task descriptor magic")?;
        let kind = reader.read_u8("task kind")?;
        if kind != LAKE_SPLIT_TASK_KIND {
            return Err(invalid_descriptor(format!(
                "expected lake split task kind {LAKE_SPLIT_TASK_KIND}, got {kind}"
            )));
        }
        let flags = reader.read_u8("task flags")?;
        if flags & !PROJECTION_PRESENT != 0 {
            return Err(invalid_descriptor(format!(
                "lake split descriptor contains unknown flags 0x{flags:02x}"
            )));
        }

        let snapshot_id = reader.read_i64("lake snapshot id")?;
        let database_len = reader.read_u32("database name length")? as usize;
        let table_len = reader.read_u32("table name length")? as usize;
        let split_len = reader.read_u32("lake split payload length")? as usize;
        let projection_count = reader.read_u32("projected field count")? as usize;
        let option_count = reader.read_u32("catalog option count")? as usize;

        let database = reader.read_string(database_len, "database name")?;
        let table = reader.read_string(table_len, "table name")?;
        let encoded_split = reader.read_string(split_len, "lake split payload")?;

        let projection_present = flags & PROJECTION_PRESENT != 0;
        if projection_present != (projection_count > 0) {
            return Err(invalid_descriptor(
                "projection flag and projected field count are inconsistent",
            ));
        }
        let projected_fields = if projection_present {
            let mut fields = Vec::new();
            for _ in 0..projection_count {
                let field_len = reader.read_u32("projected field name length")? as usize;
                fields.push(reader.read_string(field_len, "projected field name")?);
            }
            Some(fields)
        } else {
            None
        };

        let mut catalog_options = BTreeMap::new();
        for _ in 0..option_count {
            let key_len = reader.read_u32("catalog option key length")? as usize;
            let value_len = reader.read_u32("catalog option value length")? as usize;
            let key = reader.read_string(key_len, "catalog option key")?;
            let value = reader.read_string(value_len, "catalog option value")?;
            if catalog_options.insert(key, value).is_some() {
                return Err(invalid_descriptor(
                    "lake split descriptor contains a duplicate catalog option key",
                ));
            }
        }
        reader.finish()?;

        Self::try_new(
            TablePath::new(database, table),
            snapshot_id,
            catalog_options,
            projected_fields,
            encoded_split,
        )
    }
}

/// One primary-key bucket's frozen lake baseline plus its bounded log tail.
///
/// Primary-key merge completes independently per `(partition, bucket)`, so a
/// PK task must carry **all** lake splits of its bucket together with the
/// bucket's log range: the merge overlays the tail onto the lake baseline,
/// and a task split by lake file boundaries could not partition the tail
/// consistently with arbitrary file subsets. This mirrors the Java
/// connector's combined `LakeSnapshotAndFlussLogSplit`.
///
/// `pk_indexes` are frozen here rather than re-derived by executors: the
/// merge is correctness-critical, so the key definition must come from the
/// plan, not from whatever schema the executor happens to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PkHybridTaskDescriptor {
    table_path: TablePath,
    schema_id: i32,
    table_bucket: TableBucket,
    start_offset: i64,
    stop_offset: i64,
    snapshot_id: Option<i64>,
    catalog_options: BTreeMap<String, String>,
    lake_splits: Vec<String>,
    pk_indexes: Vec<usize>,
    output_projection: Option<Vec<usize>>,
}

impl PkHybridTaskDescriptor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_new(
        table_path: TablePath,
        schema_id: i32,
        table_bucket: TableBucket,
        start_offset: i64,
        stop_offset: i64,
        snapshot_id: Option<i64>,
        catalog_options: BTreeMap<String, String>,
        lake_splits: Vec<String>,
        pk_indexes: Vec<usize>,
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
                "changelog range must be non-negative, got [{start_offset}, {stop_offset})"
            )));
        }
        if start_offset > stop_offset {
            return Err(invalid_descriptor(format!(
                "changelog start offset {start_offset} exceeds stop offset {stop_offset}"
            )));
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
                "lake splits require the pinned lake snapshot id they were planned against",
            ));
        }
        if lake_splits.iter().any(String::is_empty) {
            return Err(invalid_descriptor("lake split payloads must not be empty"));
        }
        if pk_indexes.is_empty() {
            return Err(invalid_descriptor(
                "primary-key field indexes must not be empty",
            ));
        }
        let mut seen_pk = HashSet::with_capacity(pk_indexes.len());
        if pk_indexes.iter().any(|index| !seen_pk.insert(*index)) {
            return Err(invalid_descriptor(
                "primary-key field indexes must not contain duplicates",
            ));
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
            snapshot_id,
            catalog_options,
            lake_splits,
            pk_indexes,
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

    pub(crate) fn snapshot_id(&self) -> Option<i64> {
        self.snapshot_id
    }

    pub(crate) fn catalog_options(&self) -> &BTreeMap<String, String> {
        &self.catalog_options
    }

    pub(crate) fn lake_splits(&self) -> &[String] {
        &self.lake_splits
    }

    pub(crate) fn pk_indexes(&self) -> &[usize] {
        &self.pk_indexes
    }

    pub(crate) fn output_projection(&self) -> Option<&[usize]> {
        self.output_projection.as_deref()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.start_offset == self.stop_offset && self.lake_splits.is_empty()
    }

    fn encode(&self) -> UnionReadResult<Vec<u8>> {
        let database = self.table_path.database().as_bytes();
        let table = self.table_path.table().as_bytes();
        let database_len = wire_len(database.len(), "database name")?;
        let table_len = wire_len(table.len(), "table name")?;
        let split_count = wire_len(self.lake_splits.len(), "lake splits")?;
        let pk_count = wire_len(self.pk_indexes.len(), "primary-key indexes")?;
        let projection_count = match &self.output_projection {
            Some(projection) => wire_len(projection.len(), "output projection")?,
            None => 0,
        };
        let option_count = wire_len(self.catalog_options.len(), "catalog options")?;

        let mut flags = 0;
        if self.table_bucket.partition_id().is_some() {
            flags |= PARTITION_PRESENT;
        }
        if self.output_projection.is_some() {
            flags |= PROJECTION_PRESENT;
        }
        if self.snapshot_id.is_some() {
            flags |= SNAPSHOT_PRESENT;
        }

        let mut encoded = Vec::with_capacity(PK_HYBRID_HEADER_SIZE + database.len() + table.len());
        encoded.extend_from_slice(&TASK_DESCRIPTOR_MAGIC);
        encoded.push(PK_HYBRID_TASK_KIND);
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
        encoded.extend_from_slice(&projection_count.to_le_bytes());
        encoded.extend_from_slice(&option_count.to_le_bytes());
        encoded.extend_from_slice(database);
        encoded.extend_from_slice(table);
        for split in &self.lake_splits {
            encoded.extend_from_slice(&wire_len(split.len(), "lake split payload")?.to_le_bytes());
            encoded.extend_from_slice(split.as_bytes());
        }
        for pk_index in &self.pk_indexes {
            let pk_index = u32::try_from(*pk_index).map_err(|_| {
                invalid_descriptor(format!(
                    "primary-key index {pk_index} exceeds the task wire format limit"
                ))
            })?;
            encoded.extend_from_slice(&pk_index.to_le_bytes());
        }
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
        // A BTreeMap keeps the encoding deterministic: the same plan always
        // produces the same task bytes.
        for (key, value) in &self.catalog_options {
            encoded.extend_from_slice(&wire_len(key.len(), "catalog option key")?.to_le_bytes());
            encoded
                .extend_from_slice(&wire_len(value.len(), "catalog option value")?.to_le_bytes());
            encoded.extend_from_slice(key.as_bytes());
            encoded.extend_from_slice(value.as_bytes());
        }
        Ok(encoded)
    }

    fn decode(encoded: &[u8]) -> UnionReadResult<Self> {
        if encoded.len() < PK_HYBRID_HEADER_SIZE {
            return Err(invalid_descriptor(format!(
                "pk-hybrid descriptor is truncated: expected at least {PK_HYBRID_HEADER_SIZE} bytes, got {}",
                encoded.len()
            )));
        }

        let mut reader = DescriptorReader::new(encoded);
        reader.expect_bytes(&TASK_DESCRIPTOR_MAGIC, "task descriptor magic")?;
        let kind = reader.read_u8("task kind")?;
        if kind != PK_HYBRID_TASK_KIND {
            return Err(invalid_descriptor(format!(
                "expected pk-hybrid task kind {PK_HYBRID_TASK_KIND}, got {kind}"
            )));
        }
        let flags = reader.read_u8("task flags")?;
        let known_flags = PARTITION_PRESENT | PROJECTION_PRESENT | SNAPSHOT_PRESENT;
        if flags & !known_flags != 0 {
            return Err(invalid_descriptor(format!(
                "pk-hybrid descriptor contains unknown flags 0x{flags:02x}"
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
        let projection_count = reader.read_u32("projection count")? as usize;
        let option_count = reader.read_u32("catalog option count")? as usize;

        let database = reader.read_string(database_len, "database name")?;
        let table = reader.read_string(table_len, "table name")?;
        let mut lake_splits = Vec::new();
        for _ in 0..split_count {
            let split_len = reader.read_u32("lake split payload length")? as usize;
            lake_splits.push(reader.read_string(split_len, "lake split payload")?);
        }
        let mut pk_indexes = Vec::new();
        for _ in 0..pk_count {
            pk_indexes.push(reader.read_u32("primary-key field index")? as usize);
        }
        let projection_present = flags & PROJECTION_PRESENT != 0;
        if projection_present != (projection_count > 0) {
            return Err(invalid_descriptor(
                "projection flag and projection count are inconsistent",
            ));
        }
        let output_projection = if projection_present {
            let mut projection = Vec::new();
            for _ in 0..projection_count {
                projection.push(reader.read_u32("projection field index")? as usize);
            }
            Some(projection)
        } else {
            None
        };
        let mut catalog_options = BTreeMap::new();
        for _ in 0..option_count {
            let key_len = reader.read_u32("catalog option key length")? as usize;
            let value_len = reader.read_u32("catalog option value length")? as usize;
            let key = reader.read_string(key_len, "catalog option key")?;
            let value = reader.read_string(value_len, "catalog option value")?;
            if catalog_options.insert(key, value).is_some() {
                return Err(invalid_descriptor(
                    "pk-hybrid descriptor contains a duplicate catalog option key",
                ));
            }
        }
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
        let snapshot_id = if flags & SNAPSHOT_PRESENT != 0 {
            Some(encoded_snapshot_id)
        } else {
            if encoded_snapshot_id != 0 {
                return Err(invalid_descriptor(
                    "lake snapshot id must be zero when the snapshot flag is absent",
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
            snapshot_id,
            catalog_options,
            lake_splits,
            pk_indexes,
            output_projection,
        )
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

    fn catalog_options() -> BTreeMap<String, String> {
        let mut options = BTreeMap::new();
        options.insert("warehouse".to_string(), "s3://bucket/warehouse".to_string());
        options.insert("s3.region".to_string(), "us-east-1".to_string());
        options
    }

    fn lake_split_descriptor(projected_fields: Option<Vec<String>>) -> LakeSplitTaskDescriptor {
        LakeSplitTaskDescriptor::try_new(
            TablePath::new("fluss", "orders"),
            42,
            catalog_options(),
            projected_fields,
            "{\"snapshotId\":42}".to_string(),
        )
        .unwrap()
    }

    #[test]
    fn lake_split_descriptor_round_trips() {
        let descriptor = TaskDescriptor::LakeSplit(lake_split_descriptor(Some(vec![
            "amount".to_string(),
            "id".to_string(),
        ])));

        assert_eq!(
            TaskDescriptor::decode(&descriptor.encode().unwrap()).unwrap(),
            descriptor
        );
    }

    #[test]
    fn lake_split_descriptor_round_trips_without_projection() {
        let descriptor = TaskDescriptor::LakeSplit(lake_split_descriptor(None));

        assert_eq!(
            TaskDescriptor::decode(&descriptor.encode().unwrap()).unwrap(),
            descriptor
        );
    }

    #[test]
    fn lake_split_encoding_is_deterministic() {
        let descriptor = TaskDescriptor::LakeSplit(lake_split_descriptor(None));

        assert_eq!(
            descriptor.encode().unwrap(),
            descriptor.encode().unwrap(),
            "the same plan must always produce the same task bytes"
        );
    }

    #[test]
    fn rejects_invalid_lake_split_identity_and_payload() {
        assert!(matches!(
            LakeSplitTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                -1,
                catalog_options(),
                None,
                "{}".to_string(),
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
        assert!(matches!(
            LakeSplitTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                1,
                catalog_options(),
                None,
                String::new(),
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
        assert!(matches!(
            LakeSplitTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                1,
                catalog_options(),
                Some(Vec::new()),
                "{}".to_string(),
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
        assert!(matches!(
            LakeSplitTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                1,
                catalog_options(),
                Some(vec![String::new()]),
                "{}".to_string(),
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
    }

    #[test]
    fn rejects_malformed_lake_split_envelopes() {
        let encoded = TaskDescriptor::LakeSplit(lake_split_descriptor(None))
            .encode()
            .unwrap();

        let mut unknown_flags = encoded.clone();
        unknown_flags[5] = 1 << 7;
        assert!(matches!(
            TaskDescriptor::decode(&unknown_flags),
            Err(UnionReadError::InvalidTask(_))
        ));

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(matches!(
            TaskDescriptor::decode(&trailing),
            Err(UnionReadError::InvalidTask(_))
        ));

        assert!(matches!(
            TaskDescriptor::decode(&encoded[..encoded.len() - 1]),
            Err(UnionReadError::InvalidTask(_))
        ));

        // A projection flag without any projected field name must not decode.
        let mut inconsistent_projection = encoded;
        inconsistent_projection[5] = PROJECTION_PRESENT;
        assert!(matches!(
            TaskDescriptor::decode(&inconsistent_projection),
            Err(UnionReadError::InvalidTask(_))
        ));
    }

    fn pk_hybrid_descriptor() -> PkHybridTaskDescriptor {
        PkHybridTaskDescriptor::try_new(
            TablePath::new("fluss", "orders"),
            3,
            TableBucket::new_with_partition(7, Some(11), 2),
            12,
            20,
            Some(42),
            catalog_options(),
            vec![
                "{\"bucket\":2}".to_string(),
                "{\"bucket\":2,\"b\":1}".to_string(),
            ],
            vec![0, 1],
            Some(vec![2, 0]),
        )
        .unwrap()
    }

    #[test]
    fn pk_hybrid_descriptor_round_trips() {
        let descriptor = TaskDescriptor::PkHybrid(pk_hybrid_descriptor());

        assert_eq!(
            TaskDescriptor::decode(&descriptor.encode().unwrap()).unwrap(),
            descriptor
        );
    }

    /// A PK table without a readable snapshot folds the changelog only: no
    /// partition, no snapshot, no lake splits, no projection.
    #[test]
    fn pk_hybrid_descriptor_round_trips_in_log_only_form() {
        let descriptor = TaskDescriptor::PkHybrid(
            PkHybridTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                TableBucket::new(7, 2),
                0,
                20,
                None,
                BTreeMap::new(),
                Vec::new(),
                vec![0],
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
    fn pk_hybrid_encoding_is_deterministic() {
        let descriptor = TaskDescriptor::PkHybrid(pk_hybrid_descriptor());

        assert_eq!(
            descriptor.encode().unwrap(),
            descriptor.encode().unwrap(),
            "the same plan must always produce the same task bytes"
        );
    }

    #[test]
    fn rejects_invalid_pk_hybrid_shapes() {
        // Lake splits without the snapshot they were planned against.
        assert!(matches!(
            PkHybridTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                TableBucket::new(7, 2),
                0,
                20,
                None,
                catalog_options(),
                vec!["{}".to_string()],
                vec![0],
                None,
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
        // A primary-key task without key indexes cannot merge anything.
        assert!(matches!(
            PkHybridTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                TableBucket::new(7, 2),
                0,
                20,
                Some(42),
                catalog_options(),
                vec!["{}".to_string()],
                Vec::new(),
                None,
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
        // Duplicate key indexes.
        assert!(matches!(
            PkHybridTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                TableBucket::new(7, 2),
                0,
                20,
                Some(42),
                catalog_options(),
                vec!["{}".to_string()],
                vec![0, 0],
                None,
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
        // Inverted changelog range.
        assert!(matches!(
            PkHybridTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                TableBucket::new(7, 2),
                21,
                20,
                Some(42),
                catalog_options(),
                vec!["{}".to_string()],
                vec![0],
                None,
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
        // Empty split payload.
        assert!(matches!(
            PkHybridTaskDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                3,
                TableBucket::new(7, 2),
                0,
                20,
                Some(42),
                catalog_options(),
                vec![String::new()],
                vec![0],
                None,
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
    }

    #[test]
    fn rejects_malformed_pk_hybrid_envelopes() {
        let encoded = TaskDescriptor::PkHybrid(pk_hybrid_descriptor())
            .encode()
            .unwrap();

        let mut unknown_flags = encoded.clone();
        unknown_flags[5] |= 1 << 7;
        assert!(matches!(
            TaskDescriptor::decode(&unknown_flags),
            Err(UnionReadError::InvalidTask(_))
        ));

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(matches!(
            TaskDescriptor::decode(&trailing),
            Err(UnionReadError::InvalidTask(_))
        ));

        assert!(matches!(
            TaskDescriptor::decode(&encoded[..encoded.len() - 1]),
            Err(UnionReadError::InvalidTask(_))
        ));

        // Clearing the snapshot flag while its value remains set must not
        // decode into a different-but-valid descriptor.
        let mut inconsistent_snapshot = encoded;
        inconsistent_snapshot[5] &= !SNAPSHOT_PRESENT;
        assert!(matches!(
            TaskDescriptor::decode(&inconsistent_snapshot),
            Err(UnionReadError::InvalidTask(_))
        ));
    }
}
