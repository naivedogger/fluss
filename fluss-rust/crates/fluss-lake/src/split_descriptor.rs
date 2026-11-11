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
use fluss::metadata::TableInfo;
use fluss::metadata::{TableBucket, TablePath};
use std::collections::HashSet;
use std::sync::Arc;

/// Frozen execution state for one logical `(partition, bucket)` split.
///
/// Projection, filtering and read mode belong to the immutable reader configuration.
/// Lake task payloads are encoded and interpreted only by their LakeSource.
#[derive(Clone)]
pub(crate) struct SplitDescriptor {
    table_info: Arc<TableInfo>,
    snapshot_id: Option<i64>,
    table_bucket: TableBucket,
    start_offset: i64,
    stop_offset: i64,
    lake_splits: Vec<crate::LakeSplit>,
    primary_key_indexes: Vec<usize>,
}

impl std::fmt::Debug for SplitDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TableInfo may contain runtime credentials; do not print its properties.
        f.debug_struct("SplitDescriptor")
            .field("table_path", self.table_path())
            .field("schema_id", &self.schema_id())
            .field("table_bucket", &self.table_bucket)
            .field("snapshot_id", &self.snapshot_id)
            .field("start_offset", &self.start_offset)
            .field("stop_offset", &self.stop_offset)
            .field("lake_split_count", &self.lake_splits.len())
            .finish_non_exhaustive()
    }
}

impl SplitDescriptor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_new(
        table_info: Arc<TableInfo>,
        table_bucket: TableBucket,
        start_offset: i64,
        stop_offset: i64,
        snapshot_id: Option<i64>,
        lake_splits: Vec<crate::LakeSplit>,
        primary_key_indexes: Vec<usize>,
    ) -> Result<Self> {
        let descriptor = Self {
            table_info,
            snapshot_id,
            table_bucket,
            start_offset,
            stop_offset,
            lake_splits,
            primary_key_indexes,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub(crate) fn table_info(&self) -> &TableInfo {
        &self.table_info
    }

    pub(crate) fn validate_live_table(&self, live: &TableInfo) -> Result<()> {
        crate::table::validate_supported_table(live)?;
        let frozen = &self.table_info;
        let lake_changed = frozen
            .table_config
            .is_datalake_enabled()
            .map_err(|e| invalid_descriptor(e.to_string()))?
            && (!live
                .table_config
                .is_datalake_enabled()
                .map_err(|e| invalid_descriptor(e.to_string()))?
                || crate::table::resolve_lake_table_path(frozen)?
                    != crate::table::resolve_lake_table_path(live)?
                || frozen
                    .table_config
                    .get_datalake_format()
                    .map_err(|e| invalid_descriptor(e.to_string()))?
                    != live
                        .table_config
                        .get_datalake_format()
                        .map_err(|e| invalid_descriptor(e.to_string()))?);
        if live.table_id != frozen.table_id || live.table_path != frozen.table_path || lake_changed
        {
            return Err(FlussLakeError::SchemaIncompatible(
                "source identity or lake mapping changed after planning".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let Self {
            table_info,
            snapshot_id,
            table_bucket,
            start_offset,
            stop_offset,
            lake_splits,
            primary_key_indexes,
        } = self;
        let partitioned = !table_info.partition_keys.is_empty();
        if table_info.table_id != table_bucket.table_id() {
            return Err(invalid_descriptor(
                "frozen source does not match task table",
            ));
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
        if table_bucket.bucket_id() < 0
            && !(self.is_append_lake() && table_bucket.bucket_id() == -1)
        {
            return Err(invalid_descriptor(format!(
                "bucket id must be non-negative, got {}",
                table_bucket.bucket_id()
            )));
        }
        if *start_offset < 0 || *stop_offset < 0 || start_offset > stop_offset {
            return Err(invalid_descriptor(format!(
                "logical changelog range is invalid: [{start_offset}, {stop_offset})"
            )));
        }
        if partitioned && table_bucket.partition_id().is_none() && start_offset != stop_offset {
            return Err(invalid_descriptor(
                "a partition that no longer exists in Fluss cannot carry a log range",
            ));
        }
        if !lake_splits.is_empty() && snapshot_id.is_none() {
            return Err(invalid_descriptor(
                "lake splits require a pinned snapshot id",
            ));
        }
        for task in lake_splits {
            if task.format.is_empty()
                || Some(task.snapshot_id) != *snapshot_id
                || task.bucket_id != table_bucket.bucket_id()
                || !task.has_valid_bucket()
                || (self.is_primary_key() && task.bucket_id < 0)
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

        if !self.is_primary_key() && !lake_splits.is_empty() && !self.is_append_lake() {
            return Err(invalid_descriptor(
                "append lake tasks must be independent of log ranges",
            ));
        }
        Ok(())
    }

    pub(crate) fn table_path(&self) -> &TablePath {
        &self.table_info.table_path
    }

    pub(crate) fn schema_id(&self) -> i32 {
        self.table_info.schema_id
    }

    pub(crate) fn is_partitioned(&self) -> bool {
        !self.table_info.partition_keys.is_empty()
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

    pub(crate) fn is_append_lake(&self) -> bool {
        !self.is_primary_key()
            && self.lake_splits.len() == 1
            && self.start_offset == self.stop_offset
    }
}

fn invalid_descriptor(message: impl Into<String>) -> FlussLakeError {
    FlussLakeError::Internal(message.into())
}

#[cfg(test)]
pub(crate) fn testing_table(
    path: TablePath,
    schema_id: i32,
    partitioned: bool,
    table_id: i64,
) -> TableInfo {
    use fluss::metadata::{DataTypes, Schema};
    use std::collections::HashMap;
    let schema = Schema::builder()
        .column("id", DataTypes::int())
        .column("region", DataTypes::string())
        .build()
        .unwrap();
    TableInfo::new(
        path,
        table_id,
        schema_id,
        schema,
        vec![],
        if partitioned {
            vec!["region".into()]
        } else {
            vec![]
        }
        .into(),
        4,
        HashMap::from([
            ("table.datalake.enabled".into(), "true".into()),
            ("table.datalake.format".into(), "paimon".into()),
        ]),
        HashMap::new(),
        None,
        0,
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_metadata_allows_schema_evolution_but_rejects_recreated_sources() {
        let mut info = testing_table(TablePath::new("fluss", "orders"), 1, false, 7);
        info.properties
            .insert("private-key".into(), "SECRET".into());
        let task = SplitDescriptor::try_new(
            Arc::new(info.clone()),
            TableBucket::new(7, 0),
            0,
            10,
            None,
            vec![],
            vec![],
        )
        .unwrap();
        assert!(!format!("{task:?}").contains("SECRET"));
        info.schema_id += 1;
        task.validate_live_table(&info).unwrap();
        assert_eq!(task.table_info().schema_id, 1);
        info.table_id += 1;
        assert!(matches!(
            task.validate_live_table(&info),
            Err(FlussLakeError::SchemaIncompatible(_))
        ));
        info.table_id -= 1;
        info.properties
            .insert("table.datalake.table-name".into(), "other".into());
        assert!(task.validate_live_table(&info).is_err());
    }

    #[test]
    fn expired_partition_cannot_carry_a_tail() {
        let info = Arc::new(testing_table(TablePath::new("fluss", "orders"), 1, true, 7));
        assert!(
            SplitDescriptor::try_new(
                info.clone(),
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
                info,
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

    #[test]
    fn bucket_unaware_append_task_cannot_carry_a_log_tail() {
        let info = Arc::new(testing_table(
            TablePath::new("fluss", "orders"),
            1,
            false,
            7,
        ));
        let mut task = crate::source::testing_split();
        task.bucket_id = -1;
        task.bucket_count = -1;
        assert!(
            SplitDescriptor::try_new(
                info.clone(),
                TableBucket::new(7, -1),
                0,
                0,
                Some(42),
                vec![task.clone()],
                vec![]
            )
            .is_ok()
        );
        assert!(
            SplitDescriptor::try_new(
                info,
                TableBucket::new(7, -1),
                0,
                1,
                Some(42),
                vec![task],
                vec![]
            )
            .is_err()
        );
    }
}
