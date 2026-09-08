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

//! Low-level planning helpers used by the UnionRead implementation.
//!
//! Engines consume the public read context rather than the default reader's
//! opaque split descriptors or these private planning helpers.

use crate::split::{FlussLakeReadSplit, SplitStatistics};
use crate::split_descriptor::SplitDescriptor;
use crate::{CURRENT_FLUSS_LAKE_SPLIT_VERSION, FlussLakeError, FlussLakePartitionIdentity, Result};
use fluss::SnapshotId;
use fluss::client::FlussAdmin;
use fluss::error::Error as ClientError;
use fluss::metadata::{LakeSnapshotInfo, PartitionInfo, TableBucket, TableInfo, TablePath};
use fluss::rpc::message::OffsetSpec;
use std::collections::HashMap;

pub(crate) use crate::FlussLakeLogRange as FrozenBucketRange;

/// A readable lake snapshot and all server-issued Fluss log boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrozenReadBoundary {
    pub(crate) readable_lake_snapshot_id: Option<SnapshotId>,
    pub(crate) bucket_ranges: Vec<FrozenBucketRange>,
}

/// Creates one opaque logical `(partition, bucket)` split.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_logical_split(
    table_path: &TablePath,
    schema_id: i32,
    bucket_range: &FrozenBucketRange,
    snapshot_id: Option<i64>,
    lake_splits: Vec<String>,
    primary_key_indexes: Vec<usize>,
    statistics: SplitStatistics,
) -> Result<FlussLakeReadSplit> {
    let descriptor = SplitDescriptor::try_new(
        table_path.clone(),
        schema_id,
        matches!(
            bucket_range.partition_identity,
            FlussLakePartitionIdentity::KeyValues(_)
        ),
        bucket_range.table_bucket.clone(),
        bucket_range.start_offset,
        bucket_range.stop_offset,
        snapshot_id,
        lake_splits,
        primary_key_indexes,
    )?;
    let partition = match (
        &bucket_range.partition_identity,
        bucket_range.table_bucket.partition_id(),
    ) {
        (FlussLakePartitionIdentity::Unpartitioned, None) => "root".to_string(),
        (FlussLakePartitionIdentity::KeyValues(_), Some(partition_id)) => partition_id.to_string(),
        (FlussLakePartitionIdentity::KeyValues(key_values), None) => format!(
            "lake-only({})",
            key_values
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("/")
        ),
        (FlussLakePartitionIdentity::Unpartitioned, Some(partition_id)) => {
            return Err(FlussLakeError::Internal(format!(
                "unpartitioned logical split unexpectedly carries partition id {partition_id}"
            )));
        }
    };
    let split_id = format!(
        "{table_path}:{partition}:{}",
        bucket_range.table_bucket.bucket_id()
    );
    FlussLakeReadSplit::try_new(
        split_id,
        bucket_range.table_bucket.bucket_id(),
        bucket_range.partition_identity.clone(),
        CURRENT_FLUSS_LAKE_SPLIT_VERSION,
        descriptor.encode()?,
        statistics,
    )
}

impl FrozenReadBoundary {
    pub fn readable_lake_snapshot_id(&self) -> Option<SnapshotId> {
        self.readable_lake_snapshot_id
    }

    pub fn bucket_ranges(&self) -> &[FrozenBucketRange] {
        &self.bucket_ranges
    }
}

/// Freezes all live boundaries for a resolved table, before scan pruning.
pub(crate) async fn freeze_read_boundary_for_table(
    admin: &FlussAdmin,
    table_path: &TablePath,
    table_info: &TableInfo,
) -> Result<FrozenReadBoundary> {
    if table_info.num_buckets <= 0 {
        return Err(FlussLakeError::PlanningFailed(format!(
            "table {table_path} has invalid bucket count {}",
            table_info.num_buckets
        )));
    }

    let readable_snapshot = match admin.get_readable_lake_snapshot(table_path).await {
        Ok(snapshot) => Some(snapshot),
        Err(error) if error.api_error() == Some(fluss::error::FlussError::LakeSnapshotNotExist) => {
            None
        }
        Err(error) => {
            return Err(planning_client_error("get readable lake snapshot", error));
        }
    };

    let snapshot_offsets =
        collect_snapshot_offsets(table_path, table_info.table_id, readable_snapshot.as_ref())?;
    let mut partitions = if table_info.partition_keys.is_empty() {
        vec![(None, None, FlussLakePartitionIdentity::Unpartitioned)]
    } else {
        let mut partition_infos = admin
            .list_partition_infos(table_path)
            .await
            .map_err(|error| planning_client_error("list table partitions", error))?;
        partition_infos.sort_by(|left, right| {
            left.get_partition_name()
                .cmp(&right.get_partition_name())
                .then_with(|| left.get_partition_id().cmp(&right.get_partition_id()))
        });
        partition_infos
            .into_iter()
            .map(partition_identity)
            .collect()
    };

    let bucket_ids: Vec<i32> = (0..table_info.num_buckets).collect();
    let mut bucket_ranges = Vec::with_capacity(partitions.len() * bucket_ids.len());
    for (partition_id, partition_name, partition_identity) in partitions.drain(..) {
        let (earliest_offsets, latest_offsets) =
            load_server_offsets(admin, table_path, partition_name.as_deref(), &bucket_ids).await?;

        for bucket_id in &bucket_ids {
            let table_bucket =
                TableBucket::new_with_partition(table_info.table_id, partition_id, *bucket_id);
            let earliest_offset = required_offset(&earliest_offsets, &table_bucket, "earliest")?;
            let stop_offset = required_offset(&latest_offsets, &table_bucket, "latest")?;
            let snapshot_offset = snapshot_offsets.get(&table_bucket).copied();
            bucket_ranges.push(freeze_bucket_range(
                table_bucket,
                partition_identity.clone(),
                snapshot_offset,
                earliest_offset,
                stop_offset,
            )?);
        }
    }

    Ok(FrozenReadBoundary {
        readable_lake_snapshot_id: readable_snapshot.map(|snapshot| snapshot.snapshot_id),
        bucket_ranges,
    })
}

fn partition_identity(
    partition_info: PartitionInfo,
) -> (Option<i64>, Option<String>, FlussLakePartitionIdentity) {
    let resolved = partition_info.get_resolved_partition_spec();
    let key_values = resolved
        .get_partition_keys()
        .iter()
        .cloned()
        .zip(resolved.get_partition_values().iter().cloned())
        .collect();
    (
        Some(partition_info.get_partition_id()),
        Some(partition_info.get_partition_name()),
        FlussLakePartitionIdentity::KeyValues(key_values),
    )
}

fn collect_snapshot_offsets(
    table_path: &TablePath,
    table_id: i64,
    readable_snapshot: Option<&LakeSnapshotInfo>,
) -> Result<HashMap<TableBucket, i64>> {
    let Some(snapshot) = readable_snapshot else {
        return Ok(HashMap::new());
    };
    if snapshot.table_id != table_id {
        return Err(FlussLakeError::PlanningFailed(format!(
            "readable snapshot {} belongs to table id {}, but {table_path} resolved to table id {table_id}",
            snapshot.snapshot_id, snapshot.table_id
        )));
    }

    let mut offsets = HashMap::new();
    for bucket_snapshot in &snapshot.bucket_snapshots {
        let Some(offset) = bucket_snapshot.log_offset else {
            continue;
        };
        if offset < 0 {
            return Err(FlussLakeError::PlanningFailed(format!(
                "readable snapshot {} returned negative log offset {offset} for bucket {}",
                snapshot.snapshot_id, bucket_snapshot.bucket_id
            )));
        }
        let table_bucket = TableBucket::new_with_partition(
            table_id,
            bucket_snapshot.partition_id,
            bucket_snapshot.bucket_id,
        );
        if offsets.insert(table_bucket.clone(), offset).is_some() {
            return Err(FlussLakeError::PlanningFailed(format!(
                "readable snapshot {} contains duplicate boundary for {table_bucket}",
                snapshot.snapshot_id
            )));
        }
    }
    Ok(offsets)
}

async fn load_server_offsets(
    admin: &FlussAdmin,
    table_path: &TablePath,
    partition_name: Option<&str>,
    bucket_ids: &[i32],
) -> Result<(HashMap<i32, i64>, HashMap<i32, i64>)> {
    let earliest = list_server_offsets(
        admin,
        table_path,
        partition_name,
        bucket_ids,
        OffsetSpec::Earliest,
    );
    let latest = list_server_offsets(
        admin,
        table_path,
        partition_name,
        bucket_ids,
        OffsetSpec::Latest,
    );
    futures::try_join!(earliest, latest)
}

async fn list_server_offsets(
    admin: &FlussAdmin,
    table_path: &TablePath,
    partition_name: Option<&str>,
    bucket_ids: &[i32],
    offset_spec: OffsetSpec,
) -> Result<HashMap<i32, i64>> {
    let offset_name = match offset_spec {
        OffsetSpec::Earliest => "earliest",
        OffsetSpec::Latest => "latest",
        OffsetSpec::Timestamp(_) => "timestamp",
    };
    let result = match partition_name {
        Some(partition_name) => {
            admin
                .list_partition_offsets(table_path, partition_name, bucket_ids, offset_spec)
                .await
        }
        None => {
            admin
                .list_offsets(table_path, bucket_ids, offset_spec)
                .await
        }
    };
    result.map_err(|error| {
        planning_client_error(&format!("get server-issued {offset_name} offsets"), error)
    })
}

fn required_offset(
    offsets: &HashMap<i32, i64>,
    table_bucket: &TableBucket,
    offset_name: &str,
) -> Result<i64> {
    offsets
        .get(&table_bucket.bucket_id())
        .copied()
        .ok_or_else(|| {
            FlussLakeError::PlanningFailed(format!(
                "server did not return the {offset_name} offset for {table_bucket}"
            ))
        })
}

fn freeze_bucket_range(
    table_bucket: TableBucket,
    partition_identity: FlussLakePartitionIdentity,
    snapshot_offset: Option<i64>,
    earliest_offset: i64,
    stop_offset: i64,
) -> Result<FrozenBucketRange> {
    if earliest_offset < 0 || stop_offset < 0 {
        return Err(FlussLakeError::PlanningFailed(format!(
            "server returned a negative log boundary [{earliest_offset}, {stop_offset}) for {table_bucket}"
        )));
    }

    if earliest_offset > stop_offset {
        return Err(FlussLakeError::PlanningFailed(format!(
            "server earliest offset {earliest_offset} exceeds latest offset {stop_offset} for {table_bucket}"
        )));
    }
    // Without a lake baseline the complete history is required, not merely
    // the retained suffix. Availability is checked after scan-specific pruning
    // so an irrelevant range cannot break a lake-only or pruned read.
    let start_offset = snapshot_offset.unwrap_or(0);
    if start_offset > stop_offset {
        return Err(FlussLakeError::PlanningFailed(format!(
            "read start offset {start_offset} exceeds server latest offset {stop_offset} for {table_bucket}"
        )));
    }

    Ok(FrozenBucketRange {
        table_bucket,
        partition_identity,
        start_offset,
        stop_offset,
        earliest_offset,
    })
}

fn planning_client_error(action: &str, error: ClientError) -> FlussLakeError {
    match error {
        ClientError::RpcError { .. } => {
            FlussLakeError::ConnectionError(format!("failed to {action}: {error}"))
        }
        _ => FlussLakeError::PlanningFailed(format!("failed to {action}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_range(
        snapshot_offset: Option<i64>,
        earliest_offset: i64,
        stop_offset: i64,
    ) -> Result<FrozenBucketRange> {
        freeze_bucket_range(
            TableBucket::new(5, 2),
            FlussLakePartitionIdentity::Unpartitioned,
            snapshot_offset,
            earliest_offset,
            stop_offset,
        )
    }

    #[test]
    fn snapshot_offset_defines_start_and_server_latest_defines_stop() {
        let range = root_range(Some(12), 8, 20).unwrap();

        assert_eq!(range.start_offset, 12);
        assert_eq!(range.stop_offset, 20);
        assert!(!range.is_empty());
    }

    #[test]
    fn bucket_without_snapshot_requires_complete_history() {
        let range = root_range(None, 8, 20).unwrap();

        assert_eq!(range.start_offset, 0);
        assert_eq!(range.stop_offset, 20);
        assert!(matches!(
            range.validate_available(),
            Err(FlussLakeError::DataUnavailable(_))
        ));
    }

    #[test]
    fn snapshot_gap_caused_by_log_retention_is_data_unavailable() {
        let result = root_range(Some(7), 8, 20).unwrap().validate_available();

        // Re-executing a split frozen on this boundary can never succeed, so
        // the error must invalidate the attempt rather than look like a
        // generic planning failure. Replanning is not guaranteed to recover.
        assert!(matches!(result, Err(FlussLakeError::DataUnavailable(_))));
    }

    #[test]
    fn rejects_start_after_server_latest() {
        let result = root_range(Some(21), 8, 20);

        assert!(matches!(result, Err(FlussLakeError::PlanningFailed(_))));
    }

    #[test]
    fn allows_empty_bounded_tail() {
        let range = root_range(Some(20), 8, 20).unwrap();

        assert!(range.is_empty());
    }

    #[test]
    fn logical_split_carries_one_partition_bucket_lake_and_log_unit() {
        let range = FrozenBucketRange {
            table_bucket: TableBucket::new_with_partition(5, Some(9), 2),
            partition_identity: FlussLakePartitionIdentity::KeyValues(vec![(
                "region".to_string(),
                "US".to_string(),
            )]),
            start_offset: 12,
            stop_offset: 20,
            earliest_offset: 8,
        };
        let split = create_logical_split(
            &TablePath::new("fluss", "orders"),
            3,
            &range,
            Some(42),
            vec!["{\"bucket\":2}".to_string()],
            Vec::new(),
            SplitStatistics::default(),
        )
        .unwrap();

        assert_eq!(split.bucket_id, 2);
        assert_eq!(
            split.partition,
            FlussLakePartitionIdentity::KeyValues(vec![("region".to_string(), "US".to_string(),)])
        );
        let descriptor = split.decode_execution_descriptor().unwrap();
        assert_eq!(descriptor.snapshot_id(), Some(42));
        assert_eq!(descriptor.start_offset(), 12);
        assert_eq!(descriptor.stop_offset(), 20);
        assert_eq!(descriptor.lake_splits().len(), 1);
    }

    #[test]
    fn lake_only_expired_partition_has_identity_without_live_partition_id() {
        let range = FrozenBucketRange::lake_only(
            5,
            2,
            FlussLakePartitionIdentity::KeyValues(vec![("region".to_string(), "US".to_string())]),
        );
        let split = create_logical_split(
            &TablePath::new("fluss", "orders"),
            3,
            &range,
            Some(42),
            vec!["{\"bucket\":2}".to_string()],
            Vec::new(),
            SplitStatistics::new(Some(3), Some(100)),
        )
        .unwrap();

        assert!(split.split_id.contains("lake-only(region=US)"));
        assert_eq!(
            split.partition,
            FlussLakePartitionIdentity::KeyValues(vec![("region".to_string(), "US".to_string())])
        );
        let descriptor = split.decode_execution_descriptor().unwrap();
        assert!(descriptor.is_partitioned());
        assert_eq!(descriptor.table_bucket().partition_id(), None);
        assert_eq!(descriptor.start_offset(), 0);
        assert_eq!(descriptor.stop_offset(), 0);
    }
}
