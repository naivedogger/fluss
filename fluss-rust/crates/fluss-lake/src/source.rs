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

//! Lake-format extension points, following Java's FIP-6 LakeSource model.
//!
//! A source owns lake planning, payload encoding and lake reading, not Fluss
//! log boundaries or scheduling. Unlike Java's mutable pushdown configuration,
//! each call receives an immutable request. This lets concurrent plans share a
//! source without leaking projection or filter state between queries.

use crate::{FlussLakeError, FlussLakePartitionIdentity, RecordBatchStream, Result};
use arrow::datatypes::SchemaRef;
use fluss::metadata::TableInfo;
use fluss::predicate::{BoundPredicate, CompoundFunction};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::{Debug, Formatter};

/// The baseline required by the UnionRead executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LakeReadSemantics {
    /// Every selected append record, without introducing duplicates.
    Append,
    /// A snapshot current view: at most one live row per full primary key.
    ///
    /// The source must apply lake-specific version, merge and deletion rules
    /// across the entire supplied split group. Output need not be sorted.
    PrimaryKey,
}

/// One lake-format task. This is not a Fluss log range or an engine thread.
///
/// The backend owns `payload_version` and `payload`, just as a Java LakeSource
/// owns its split serializer. Payloads must not contain credentials. Another
/// reader may consume these tasks only if it implements the same payload
/// contract; the envelope is not a universal file-list format.
///
/// The default executor requires tasks aligned with the Fluss partition/bucket
/// layout. Bucket-less files or a different lake layout need an adapter that
/// restores this ownership, or an engine-native reconciliation plan.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LakeSplit {
    pub format: String,
    pub snapshot_id: i64,
    pub partition: FlussLakePartitionIdentity,
    pub bucket_id: i32,
    pub estimated_rows: Option<usize>,
    pub estimated_size: Option<usize>,
    pub payload_version: u32,
    pub payload: Vec<u8>,
}

impl LakeSplit {
    pub(crate) fn validate(&self, format: &str, snapshot_id: i64, num_buckets: i32) -> Result<()> {
        if self.format != format
            || self.snapshot_id != snapshot_id
            || self.bucket_id < 0
            || self.bucket_id >= num_buckets
            || self.payload_version == 0
            || self.payload.is_empty()
        {
            return Err(FlussLakeError::PlanningFailed(
                "lake split has incompatible format, snapshot, bucket or payload".to_string(),
            ));
        }
        Ok(())
    }
}

impl Debug for LakeSplit {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LakeSplit")
            .field("format", &self.format)
            .field("snapshot_id", &self.snapshot_id)
            .field("partition", &self.partition)
            .field("bucket_id", &self.bucket_id)
            .field("payload_version", &self.payload_version)
            .field("payload_size", &self.payload.len())
            .finish_non_exhaustive()
    }
}

/// Immutable input to lake planning. The snapshot must never be refreshed.
pub struct LakePlannerContext<'a> {
    pub table_info: &'a TableInfo,
    pub snapshot_id: i64,
    pub semantics: LakeReadSemantics,
    /// Only predicates safe before UnionRead reconciliation are supplied.
    /// Backends may ignore them; the executor always evaluates the full filter.
    pub filter: &'a BoundPredicate,
}

/// Immutable input to a lake reader, created from a bound UnionRead plan.
pub struct LakeReaderContext<'a> {
    pub table_info: &'a TableInfo,
    pub snapshot_id: i64,
    pub semantics: LakeReadSemantics,
    /// All selected lake tasks for ONE logical partition/bucket.
    pub splits: &'a [LakeSplit],
    /// Fluss field indexes, including hidden filter and primary-key columns.
    /// Return columns in exactly this order; indexes refer to the full table.
    pub projection: &'a [usize],
    pub schema: SchemaRef,
    /// A safe optional optimization, not permission to change the result.
    pub filter: &'a BoundPredicate,
}

/// A lake backend, corresponding to Java's LakeSource planner/reader pair.
///
/// `plan` and `read` are asynchronous rather than returning stateless factory
/// wrappers. Implementations must not store request-specific mutable state.
/// A read returns owned batches and must release I/O resources when its future
/// or stream is dropped. Missing frozen inputs must fail, never fall back to
/// a newer snapshot or skip files. Unknown payload versions must be rejected.
///
/// This is a Rust extension API, not a stable binary ABI. Engines owning their
/// complete execution can instead use source metadata/log APIs and need not
/// implement this trait or adopt the default logical splits.
pub trait LakeSource: Send + Sync {
    /// Lake format identifier matching the Fluss table configuration.
    fn format(&self) -> &str;

    /// Plan every selected lake partition, including lake-only partitions.
    fn plan<'a>(&'a self, context: LakePlannerContext<'a>)
    -> BoxFuture<'a, Result<Vec<LakeSplit>>>;

    /// Read the specified group under its required baseline semantics.
    fn read<'a>(
        &'a self,
        context: LakeReaderContext<'a>,
    ) -> BoxFuture<'a, Result<RecordBatchStream>>;
}

/// Selects the part of the exact core predicate that is safe to evaluate
/// inside a lake baseline reader.
///
/// Append and lake-only reads may push the complete predicate. During a
/// primary-key UnionRead, only primary-key predicates are immutable across
/// the lake baseline and changelog tail. Mixed top-level `AND` expressions
/// therefore contribute only their safe key conjuncts; `OR` is pushed only
/// when every branch is key-only.
pub(crate) fn lake_pushdown_filter(
    predicate: &BoundPredicate,
    table_info: &TableInfo,
    reconcile_primary_key: bool,
) -> Option<BoundPredicate> {
    if matches!(predicate, BoundPredicate::AlwaysTrue) {
        return None;
    }
    if !reconcile_primary_key {
        return Some(predicate.clone());
    }

    let primary_key_indexes: HashSet<usize> = table_info
        .primary_keys
        .iter()
        .filter_map(|key| {
            table_info
                .row_type()
                .fields()
                .iter()
                .position(|field| field.name() == key)
        })
        .collect();
    if primary_key_indexes.len() != table_info.primary_keys.len() {
        return None;
    }
    project_predicate_to_fields(predicate, &primary_key_indexes)
}

fn project_predicate_to_fields(
    predicate: &BoundPredicate,
    allowed_fields: &HashSet<usize>,
) -> Option<BoundPredicate> {
    match predicate {
        BoundPredicate::AlwaysTrue => None,
        BoundPredicate::Leaf { field_index, .. } => allowed_fields
            .contains(field_index)
            .then(|| predicate.clone()),
        BoundPredicate::Compound {
            function: CompoundFunction::And,
            children,
        } => {
            let children: Vec<_> = children
                .iter()
                .filter_map(|child| project_predicate_to_fields(child, allowed_fields))
                .collect();
            (!children.is_empty()).then_some(BoundPredicate::Compound {
                function: CompoundFunction::And,
                children,
            })
        }
        BoundPredicate::Compound {
            function: CompoundFunction::Or,
            children,
        } => {
            let children: Option<Vec<_>> = children
                .iter()
                .map(|child| project_predicate_to_fields(child, allowed_fields))
                .collect();
            Some(BoundPredicate::Compound {
                function: CompoundFunction::Or,
                children: children?,
            })
        }
    }
}

#[cfg(test)]
pub(crate) fn testing_split() -> LakeSplit {
    LakeSplit {
        format: "paimon".into(),
        snapshot_id: 42,
        partition: FlussLakePartitionIdentity::Unpartitioned,
        bucket_id: 0,
        estimated_rows: Some(3),
        estimated_size: None,
        payload_version: 1,
        payload: b"testing-task".to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int32Array;
    use arrow::record_batch::RecordBatch;
    use fluss::metadata::{DataTypes, Schema, TablePath};
    use fluss::predicate::col;
    use futures::TryStreamExt;
    use std::collections::HashMap;
    use std::sync::Arc;

    // Deliberately not Paimon. This test also runs without any lake-format feature.
    struct TestingLakeSource;

    impl LakeSource for TestingLakeSource {
        fn format(&self) -> &str {
            "iceberg"
        }
        fn plan<'a>(
            &'a self,
            context: LakePlannerContext<'a>,
        ) -> BoxFuture<'a, Result<Vec<LakeSplit>>> {
            Box::pin(async move {
                Ok(vec![LakeSplit {
                    format: self.format().into(),
                    snapshot_id: context.snapshot_id,
                    ..testing_split()
                }])
            })
        }
        fn read<'a>(
            &'a self,
            context: LakeReaderContext<'a>,
        ) -> BoxFuture<'a, Result<RecordBatchStream>> {
            Box::pin(async move {
                for task in context.splits {
                    task.validate(
                        self.format(),
                        context.snapshot_id,
                        context.table_info.num_buckets,
                    )?;
                    if task.payload_version != 1 {
                        return Err(FlussLakeError::IncompatibleSplitVersion(
                            "testing source supports V1".into(),
                        ));
                    }
                }
                let batch = RecordBatch::try_new(
                    context.schema,
                    vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
                )
                .unwrap();
                Ok(Box::pin(futures::stream::iter([Ok(batch)])) as RecordBatchStream)
            })
        }
    }

    #[test]
    fn format_neutral_source_uses_fixed_snapshot_and_versioned_tasks() {
        let info = TableInfo::new(
            TablePath::new("db", "table"),
            7,
            1,
            Schema::builder()
                .column("id", DataTypes::int())
                .build()
                .unwrap(),
            vec![],
            vec![].into(),
            1,
            HashMap::new(),
            HashMap::new(),
            None,
            0,
            0,
        );
        let source: Arc<dyn LakeSource> = Arc::new(TestingLakeSource);
        futures::executor::block_on(async {
            let tasks = source
                .plan(LakePlannerContext {
                    table_info: &info,
                    snapshot_id: 42,
                    semantics: LakeReadSemantics::Append,
                    filter: &BoundPredicate::AlwaysTrue,
                })
                .await
                .unwrap();
            let tasks: Vec<LakeSplit> =
                serde_json::from_slice(&serde_json::to_vec(&tasks).unwrap()).unwrap();
            let schema = fluss::record::to_arrow_schema(info.row_type()).unwrap();
            let read = |tasks| {
                source.read(LakeReaderContext {
                    table_info: &info,
                    snapshot_id: 42,
                    semantics: LakeReadSemantics::Append,
                    splits: tasks,
                    projection: &[0],
                    schema: schema.clone(),
                    filter: &BoundPredicate::AlwaysTrue,
                })
            };
            let batches = read(&tasks)
                .await
                .unwrap()
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert_eq!(batches[0].num_rows(), 3);
            let mut invalid_tasks = tasks.clone();
            invalid_tasks[0].payload_version = 2;
            assert!(read(&invalid_tasks).await.is_err());
        });
    }

    #[test]
    fn safe_pushdown_does_not_expose_mutable_pk_filters() {
        let info = TableInfo::new(
            TablePath::new("db", "table"),
            7,
            1,
            Schema::builder()
                .column("id", DataTypes::int())
                .column("value", DataTypes::int())
                .primary_key(["id"])
                .unwrap()
                .build()
                .unwrap(),
            vec!["id".into()],
            vec![].into(),
            1,
            HashMap::new(),
            HashMap::new(),
            None,
            0,
            0,
        );
        let filter = BoundPredicate::bind(
            Some(&col("id").eq(1).and(col("value").gt(10))),
            info.row_type(),
        )
        .unwrap();
        assert_eq!(
            lake_pushdown_filter(&filter, &info, true)
                .unwrap()
                .referenced_field_indexes(),
            vec![0]
        );
        assert_eq!(
            lake_pushdown_filter(&filter, &info, false)
                .unwrap()
                .referenced_field_indexes(),
            vec![0, 1]
        );
        let filter = BoundPredicate::bind(
            Some(&col("id").eq(1).or(col("value").gt(10))),
            info.row_type(),
        )
        .unwrap();
        assert!(lake_pushdown_filter(&filter, &info, true).is_none());
    }
}
