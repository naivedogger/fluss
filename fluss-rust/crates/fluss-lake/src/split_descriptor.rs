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
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Frozen execution state for one logical `(partition, bucket)` split.
///
/// Projection, filtering and read mode belong to the immutable reader configuration.
/// Lake task payloads are encoded and interpreted only by their LakeSource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SplitDescriptor {
    table_path: TablePath,
    schema_id: i32,
    partitioned: bool,
    table_bucket: TableBucket,
    start_offset: i64,
    stop_offset: i64,
    snapshot_id: Option<i64>,
    lake_splits: Vec<crate::LakeSplit>,
    primary_key_indexes: Vec<usize>,
}

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
        lake_splits: Vec<crate::LakeSplit>,
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
        for task in &lake_splits {
            if task.format.is_empty()
                || Some(task.snapshot_id) != snapshot_id
                || task.bucket_id != table_bucket.bucket_id()
                || task.bucket_count <= 0
                || task.bucket_id >= task.bucket_count
                || task.bucket_count != lake_splits[0].bucket_count
                || task.payload_version == 0
                || task.payload.is_empty()
            {
                return Err(invalid_descriptor(
                    "lake task identity or payload does not match the logical split",
                ));
            }
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

    pub(crate) fn lake_splits(&self) -> &[crate::LakeSplit] {
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

    pub(crate) fn validate(self) -> Result<Self> {
        Self::try_new(
            self.table_path,
            self.schema_id,
            self.partitioned,
            self.table_bucket,
            self.start_offset,
            self.stop_offset,
            self.snapshot_id,
            self.lake_splits,
            self.primary_key_indexes,
        )
    }

    #[cfg(test)]
    fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| invalid_descriptor(e.to_string()))
    }

    #[cfg(test)]
    fn decode(encoded: &[u8]) -> Result<Self> {
        let value: Self =
            serde_json::from_slice(encoded).map_err(|e| invalid_descriptor(e.to_string()))?;
        value.validate()
    }
}

fn invalid_descriptor(message: impl Into<String>) -> FlussLakeError {
    FlussLakeError::Internal(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn descriptor() -> SplitDescriptor {
        SplitDescriptor::try_new(
            TablePath::new("fluss", "orders"),
            3,
            false,
            TableBucket::new(7, 0),
            12,
            20,
            Some(42),
            vec![crate::source::testing_split()],
            vec![0],
        )
        .unwrap()
    }

    #[test]
    fn descriptor_round_trip_and_rejects_corrupt_transport() {
        let value = descriptor();
        let encoded = value.encode().unwrap();
        assert_eq!(SplitDescriptor::decode(&encoded).unwrap(), value);
        assert!(SplitDescriptor::decode(&encoded[..encoded.len() - 1]).is_err());
        let mut trailing = encoded.clone();
        trailing.push(b'x');
        assert!(SplitDescriptor::decode(&trailing).is_err());
        assert!(SplitDescriptor::decode(b"URD1").is_err());
    }

    #[test]
    fn descriptor_revalidates_identity_ranges_payloads_and_keys() {
        for (field, invalid) in [
            ("schema_id", json!(-1)),
            ("start_offset", json!(21)),
            ("stop_offset", json!(-1)),
            ("snapshot_id", json!(null)),
            ("primary_key_indexes", json!([0, 0])),
            ("unknown", json!(true)),
            ("lake_splits", json!([{}])),
        ] {
            let mut value = serde_json::to_value(descriptor()).unwrap();
            value[field] = invalid;
            assert!(
                SplitDescriptor::decode(&serde_json::to_vec(&value).unwrap()).is_err(),
                "{field}"
            );
        }
        for (field, invalid) in [
            ("bucket_id", json!(1)),
            ("bucket_count", json!(0)),
            ("bucket_count", json!(-1)),
            ("snapshot_id", json!(43)),
            ("payload", json!([])),
            ("payload_version", json!(0)),
        ] {
            let mut value = serde_json::to_value(descriptor()).unwrap();
            value["lake_splits"][0][field] = invalid;
            assert!(
                SplitDescriptor::decode(&serde_json::to_vec(&value).unwrap()).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn expired_partition_cannot_carry_a_tail() {
        assert!(
            SplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                1,
                true,
                TableBucket::new(7, 0),
                1,
                2,
                None,
                vec![],
                vec![]
            )
            .is_err()
        );
        assert!(
            SplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                1,
                true,
                TableBucket::new(7, 0),
                0,
                0,
                Some(42),
                vec![crate::source::testing_split()],
                vec![]
            )
            .is_ok()
        );
    }
}
