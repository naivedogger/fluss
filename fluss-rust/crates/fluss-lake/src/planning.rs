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
//! Upstream engines should use [`crate::UnionReadPlanner`] and treat its tasks
//! as opaque rather than depending on the types in this module.

use crate::task::{AppendLogTaskDescriptor, LakeSplitTaskDescriptor, TaskDescriptor};
use crate::{
    CURRENT_UNION_READ_TASK_VERSION, UnionReadError, UnionReadResult, UnionReadStatistics,
    UnionReadTask,
};
use fluss::SnapshotId;
use fluss::client::FlussAdmin;
use fluss::error::Error as ClientError;
use fluss::metadata::{LakeSnapshotInfo, PartitionInfo, TableBucket, TableInfo, TablePath};
use fluss::rpc::message::OffsetSpec;
use std::collections::{BTreeMap, HashMap};

/// One immutable `[start_offset, stop_offset)` Fluss log boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenBucketRange {
    table_bucket: TableBucket,
    partition_name: Option<String>,
    start_offset: i64,
    stop_offset: i64,
}

impl FrozenBucketRange {
    pub fn table_bucket(&self) -> &TableBucket {
        &self.table_bucket
    }

    pub fn partition_name(&self) -> Option<&str> {
        self.partition_name.as_deref()
    }

    pub fn start_offset(&self) -> i64 {
        self.start_offset
    }

    pub fn stop_offset(&self) -> i64 {
        self.stop_offset
    }

    pub fn is_empty(&self) -> bool {
        self.start_offset == self.stop_offset
    }
}

/// A readable lake snapshot and all server-issued Fluss log boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenReadBoundary {
    readable_lake_snapshot_id: Option<SnapshotId>,
    bucket_ranges: Vec<FrozenBucketRange>,
}

impl FrozenReadBoundary {
    pub fn readable_lake_snapshot_id(&self) -> Option<SnapshotId> {
        self.readable_lake_snapshot_id
    }

    pub fn bucket_ranges(&self) -> &[FrozenBucketRange] {
        &self.bucket_ranges
    }
}

/// Creates one opaque append-log task from a frozen bucket range.
pub fn create_append_log_task(
    table_path: &TablePath,
    schema_id: i32,
    bucket_range: &FrozenBucketRange,
    output_projection: Option<&[usize]>,
    statistics: UnionReadStatistics,
) -> UnionReadResult<UnionReadTask> {
    let descriptor = TaskDescriptor::AppendLog(AppendLogTaskDescriptor::try_new(
        table_path.clone(),
        schema_id,
        bucket_range.table_bucket.clone(),
        bucket_range.start_offset,
        bucket_range.stop_offset,
        output_projection.map(<[usize]>::to_vec),
    )?);
    let partition = bucket_range
        .table_bucket
        .partition_id()
        .map_or_else(|| "root".to_string(), |id| id.to_string());
    let task_id = format!(
        "append-log/{}/{}/{}/{}-{}",
        bucket_range.table_bucket.table_id(),
        partition,
        bucket_range.table_bucket.bucket_id(),
        bucket_range.start_offset,
        bucket_range.stop_offset
    );
    UnionReadTask::try_new(
        task_id,
        CURRENT_UNION_READ_TASK_VERSION,
        descriptor.encode()?,
        statistics,
    )
}

/// Creates one opaque lake-split task from a frozen lake snapshot split.
///
/// `split_index` only makes the task id unique and readable for scheduling; the
/// executor never derives read semantics from it.
pub(crate) fn create_lake_split_task(
    table_path: &TablePath,
    snapshot_id: i64,
    catalog_options: BTreeMap<String, String>,
    projected_fields: Option<Vec<String>>,
    encoded_split: String,
    split_index: usize,
    statistics: UnionReadStatistics,
) -> UnionReadResult<UnionReadTask> {
    let descriptor = TaskDescriptor::LakeSplit(LakeSplitTaskDescriptor::try_new(
        table_path.clone(),
        snapshot_id,
        catalog_options,
        projected_fields,
        encoded_split,
    )?);
    let task_id = format!("lake-split/{table_path}/{snapshot_id}/{split_index}");
    UnionReadTask::try_new(
        task_id,
        CURRENT_UNION_READ_TASK_VERSION,
        descriptor.encode()?,
        statistics,
    )
}

/// Freezes the mutable table state needed by a bounded UnionRead plan.
///
/// The readable lake snapshot supplies per-bucket start offsets. Buckets not
/// represented in the snapshot — including every bucket when no readable
/// snapshot exists at all — start at the server-issued earliest offset,
/// matching the Java connector's fallback in all no-snapshot cases. Stop
/// offsets always come from a server `Latest` list-offsets response. The
/// returned values are copied into task descriptions and never recomputed by
/// executors.
pub async fn freeze_read_boundary(
    admin: &FlussAdmin,
    table_path: &TablePath,
) -> UnionReadResult<FrozenReadBoundary> {
    let table_info = admin
        .get_table_info(table_path)
        .await
        .map_err(|error| planning_client_error("get table metadata", error))?;
    freeze_read_boundary_for_table(admin, table_path, &table_info, None).await
}

/// Freezes boundaries for a resolved table, optionally skipping partitions.
///
/// A partition excluded by `partition_filter` contributes no bucket ranges
/// and no offset requests. The filter must be conservative: it may only
/// exclude partitions that provably contain no matching rows.
pub(crate) async fn freeze_read_boundary_for_table(
    admin: &FlussAdmin,
    table_path: &TablePath,
    table_info: &TableInfo,
    partition_filter: Option<&(dyn Fn(&PartitionInfo) -> bool + Sync)>,
) -> UnionReadResult<FrozenReadBoundary> {
    if table_info.num_buckets <= 0 {
        return Err(UnionReadError::Planning(format!(
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
        vec![(None, None)]
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
            .filter(|partition_info| partition_filter.is_none_or(|matches| matches(partition_info)))
            .map(partition_identity)
            .collect()
    };

    let bucket_ids: Vec<i32> = (0..table_info.num_buckets).collect();
    let mut bucket_ranges = Vec::with_capacity(partitions.len() * bucket_ids.len());
    for (partition_id, partition_name) in partitions.drain(..) {
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
                partition_name.clone(),
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

fn partition_identity(partition_info: PartitionInfo) -> (Option<i64>, Option<String>) {
    (
        Some(partition_info.get_partition_id()),
        Some(partition_info.get_partition_name()),
    )
}

fn collect_snapshot_offsets(
    table_path: &TablePath,
    table_id: i64,
    readable_snapshot: Option<&LakeSnapshotInfo>,
) -> UnionReadResult<HashMap<TableBucket, i64>> {
    let Some(snapshot) = readable_snapshot else {
        return Ok(HashMap::new());
    };
    if snapshot.table_id != table_id {
        return Err(UnionReadError::Planning(format!(
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
            return Err(UnionReadError::Planning(format!(
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
            return Err(UnionReadError::Planning(format!(
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
) -> UnionReadResult<(HashMap<i32, i64>, HashMap<i32, i64>)> {
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
) -> UnionReadResult<HashMap<i32, i64>> {
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
) -> UnionReadResult<i64> {
    offsets
        .get(&table_bucket.bucket_id())
        .copied()
        .ok_or_else(|| {
            UnionReadError::Planning(format!(
                "server did not return the {offset_name} offset for {table_bucket}"
            ))
        })
}

fn freeze_bucket_range(
    table_bucket: TableBucket,
    partition_name: Option<String>,
    snapshot_offset: Option<i64>,
    earliest_offset: i64,
    stop_offset: i64,
) -> UnionReadResult<FrozenBucketRange> {
    if earliest_offset < 0 || stop_offset < 0 {
        return Err(UnionReadError::Planning(format!(
            "server returned a negative log boundary [{earliest_offset}, {stop_offset}) for {table_bucket}"
        )));
    }

    let start_offset = snapshot_offset.unwrap_or(earliest_offset);
    if start_offset < earliest_offset {
        // Log retention has removed data between the snapshot boundary and
        // the server's earliest offset. The Java connector fails too, at
        // fetch time (offset-out-of-range); surfacing it at plan time gives
        // a clearer, typed error before any work is scheduled.
        return Err(UnionReadError::DataUnavailable(format!(
            "readable snapshot offset {start_offset} for {table_bucket} is older than the server earliest offset {earliest_offset}: the log tail needed to complete the result has been removed by retention; re-plan to freeze currently-valid boundaries"
        )));
    }
    if start_offset > stop_offset {
        return Err(UnionReadError::Planning(format!(
            "read start offset {start_offset} exceeds server latest offset {stop_offset} for {table_bucket}"
        )));
    }

    Ok(FrozenBucketRange {
        table_bucket,
        partition_name,
        start_offset,
        stop_offset,
    })
}

fn planning_client_error(action: &str, error: ClientError) -> UnionReadError {
    UnionReadError::Planning(format!("failed to {action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_offset_defines_start_and_server_latest_defines_stop() {
        let range = freeze_bucket_range(TableBucket::new(5, 2), None, Some(12), 8, 20).unwrap();

        assert_eq!(range.start_offset(), 12);
        assert_eq!(range.stop_offset(), 20);
        assert!(!range.is_empty());
    }

    #[test]
    fn bucket_without_snapshot_uses_frozen_server_earliest() {
        let range = freeze_bucket_range(TableBucket::new(5, 2), None, None, 8, 20).unwrap();

        assert_eq!(range.start_offset(), 8);
        assert_eq!(range.stop_offset(), 20);
    }

    #[test]
    fn snapshot_gap_caused_by_log_retention_is_data_unavailable() {
        let result = freeze_bucket_range(TableBucket::new(5, 2), None, Some(7), 8, 20);

        // Re-executing a task frozen on this boundary can never succeed, so
        // the error must be the typed, re-plan-recoverable kind rather than
        // a generic planning failure.
        assert!(matches!(result, Err(UnionReadError::DataUnavailable(_))));
    }

    #[test]
    fn rejects_start_after_server_latest() {
        let result = freeze_bucket_range(TableBucket::new(5, 2), None, Some(21), 8, 20);

        assert!(matches!(result, Err(UnionReadError::Planning(_))));
    }

    #[test]
    fn allows_empty_bounded_tail() {
        let range = freeze_bucket_range(TableBucket::new(5, 2), None, Some(20), 8, 20).unwrap();

        assert!(range.is_empty());
    }
}
