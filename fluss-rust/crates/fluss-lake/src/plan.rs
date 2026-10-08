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

//! UnionRead planning result.

use crate::executor::execute_split;
use crate::split::FlussLakeReadSplit;
use crate::table::{FlussLakeScan, stop_after_first_error};
use crate::{FlussLakeError, FlussLakeReadContext, RecordBatchStream, Result};
use arrow::datatypes::SchemaRef;
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;

/// Aggregated statistics about a planned UnionRead job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlussLakePlanStatistics {
    /// Number of logical tasks, not the engine's execution parallelism.
    pub split_count: usize,
    /// Total estimated input rows; `None` means an estimate is missing or overflows.
    /// This is not an exact PK result cardinality.
    pub estimated_rows: Option<usize>,
    /// Total estimated input bytes; `None` means an estimate is missing or overflows.
    pub estimated_size: Option<usize>,
}

impl FlussLakePlanStatistics {
    pub(crate) fn from_splits(splits: &[FlussLakeReadSplit]) -> Self {
        Self {
            split_count: splits.len(),
            estimated_rows: sum_estimates(splits.iter().map(|split| split.estimated_rows)),
            estimated_size: sum_estimates(splits.iter().map(|split| split.estimated_size)),
        }
    }
}

/// Immutable frozen tasks, output schema and source context.
/// Execution uses a reader created from the matching scan configuration.
#[derive(Clone)]
pub struct FlussLakeReadPlan {
    inner: Arc<ReadPlan>,
}

struct ReadPlan {
    context: FlussLakeReadContext,
    schema: SchemaRef,
    splits: Vec<FlussLakeReadSplit>,
    statistics: FlussLakePlanStatistics,
}

impl FlussLakeReadPlan {
    pub(crate) fn new(
        context: FlussLakeReadContext,
        schema: SchemaRef,
        splits: Vec<FlussLakeReadSplit>,
    ) -> Result<Self> {
        let mut split_ids = HashSet::with_capacity(splits.len());
        for split in &splits {
            if !split_ids.insert(&split.split_id) {
                return Err(FlussLakeError::PlanningFailed(
                    "duplicate logical split identity".to_string(),
                ));
            }
        }
        let statistics = FlussLakePlanStatistics::from_splits(&splits);
        Ok(Self {
            inner: Arc::new(ReadPlan {
                context,
                schema,
                splits,
                statistics,
            }),
        })
    }

    /// Source state used by this plan, reusable with another scan configuration.
    pub fn read_context(&self) -> &FlussLakeReadContext {
        &self.inner.context
    }

    /// Schema of every output batch, including zero-column count scans.
    pub fn schema(&self) -> SchemaRef {
        self.inner.schema.clone()
    }

    /// Number of logical splits, not engine parallelism.
    pub fn split_count(&self) -> usize {
        self.inner.splits.len()
    }

    /// Immutable logical tasks. Engines decide how to schedule them.
    pub fn splits(&self) -> &[FlussLakeReadSplit] {
        &self.inner.splits
    }

    /// Best-effort input estimates, not an exact PK result cardinality.
    pub fn statistics(&self) -> FlussLakePlanStatistics {
        self.inner.statistics
    }
}

impl std::fmt::Debug for FlussLakeReadPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlussLakeReadPlan")
            .field("table_path", self.inner.context.table_path())
            .field("statistics", &self.inner.statistics)
            .finish_non_exhaustive()
    }
}

/// Reusable reader with immutable scan configuration.
///
/// Local callers and distributed workers use the same scan-created reader.
/// The caller must supply the original tasks and the planner's scan settings,
/// including a compatible lake backend. No plan object is retained or rebuilt.
#[derive(Clone, Debug)]
pub struct FlussLakeReader {
    scan: Arc<FlussLakeScan>,
}

impl FlussLakeReader {
    pub(crate) fn from_scan(scan: FlussLakeScan) -> Self {
        Self {
            scan: Arc::new(scan),
        }
    }

    pub(crate) fn scan(&self) -> &FlussLakeScan {
        &self.scan
    }

    /// Read a frozen task, including a serialized round-trip from a coordinator.
    /// Opening is lazy; the first stream error is terminal. Drop cancels the read.
    pub async fn read_split(&self, split: &FlussLakeReadSplit) -> Result<RecordBatchStream> {
        self.scan().validate_configuration()?;
        execute_split(split, self).map(stop_after_first_error)
    }

    /// Read a task collection as an unordered stream with at most eight
    /// active logical tasks. Duplicate tasks are rejected. Engines may instead
    /// schedule `read_split` themselves. An error drops all sibling streams;
    /// discard this attempt's output before replanning with fresh boundaries.
    pub async fn read_splits(&self, splits: &[FlussLakeReadSplit]) -> Result<RecordBatchStream> {
        self.read_splits_with_concurrency(splits, 8).await
    }

    /// As `read_splits`, with an explicit positive active-task limit.
    pub async fn read_splits_with_concurrency(
        &self,
        splits: &[FlussLakeReadSplit],
        concurrency: usize,
    ) -> Result<RecordBatchStream> {
        if concurrency == 0 {
            return Err(FlussLakeError::PlanningFailed(
                "read concurrency must be positive".to_string(),
            ));
        }
        self.scan().validate_configuration()?;
        let mut seen = HashSet::with_capacity(splits.len());
        for split in splits {
            if !seen.insert(&split.split_id) {
                return Err(FlussLakeError::PlanningFailed(
                    "duplicate read task".to_string(),
                ));
            }
        }
        let streams = splits
            .iter()
            .map(|split| execute_split(split, self))
            .collect::<Result<Vec<_>>>()?;
        Ok(merge_split_streams(streams, concurrency))
    }
}

fn merge_split_streams(streams: Vec<RecordBatchStream>, concurrency: usize) -> RecordBatchStream {
    stop_after_first_error(Box::pin(
        futures::stream::iter(streams).flatten_unordered(concurrency),
    ))
}

fn sum_estimates(mut estimates: impl Iterator<Item = Option<usize>>) -> Option<usize> {
    estimates.try_fold(0usize, |total, estimate| total.checked_add(estimate?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split::{FlussLakePartitionIdentity, SplitStatistics};
    use crate::split_descriptor::SplitDescriptor;
    use arrow::array::Int32Array;
    use arrow::record_batch::RecordBatch;
    use fluss::metadata::{TableBucket, TablePath};
    use futures::TryStreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ActiveRead(Arc<AtomicUsize>);
    impl Drop for ActiveRead {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn counted_stream(active: Arc<AtomicUsize>, peak: Arc<AtomicUsize>) -> RecordBatchStream {
        Box::pin(
            futures::stream::once(async move {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                let guard = ActiveRead(active);
                let batch = RecordBatch::try_from_iter(vec![(
                    "id",
                    Arc::new(Int32Array::from(vec![1])) as arrow::array::ArrayRef,
                )])
                .unwrap();
                Ok::<RecordBatchStream, FlussLakeError>(Box::pin(
                    futures::stream::iter([Ok(batch.clone()), Ok(batch)]).map(move |item| {
                        let _keep_alive = &guard;
                        item
                    }),
                ))
            })
            .try_flatten(),
        )
    }

    #[test]
    fn merged_reads_bound_active_streams_and_release_on_drop() {
        futures::executor::block_on(async {
            let active = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let streams = (0..12)
                .map(|_| counted_stream(active.clone(), peak.clone()))
                .collect();
            let batches = merge_split_streams(streams, 2)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert_eq!(batches.len(), 24);
            assert!(peak.load(Ordering::SeqCst) <= 2);
            assert_eq!(active.load(Ordering::SeqCst), 0);

            let streams = (0..12)
                .map(|_| counted_stream(active.clone(), peak.clone()))
                .collect();
            let mut merged = merge_split_streams(streams, 2);
            assert!(merged.next().await.unwrap().is_ok());
            drop(merged);
            assert_eq!(active.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn merged_error_drops_siblings_and_cannot_be_followed_by_rows() {
        futures::executor::block_on(async {
            let active = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let streams = vec![
                counted_stream(active.clone(), peak),
                Box::pin(futures::stream::once(async {
                    Err(FlussLakeError::DataUnavailable("expired input".into()))
                })) as RecordBatchStream,
            ];
            let mut stream = merge_split_streams(streams, 2);
            loop {
                if stream
                    .next()
                    .await
                    .expect("must report the failure")
                    .is_err()
                {
                    break;
                }
            }
            assert_eq!(active.load(Ordering::SeqCst), 0);
            assert!(stream.next().await.is_none());
        });
    }

    fn split(rows: Option<usize>, size: Option<usize>) -> FlussLakeReadSplit {
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
        FlussLakeReadSplit::try_new(
            "fluss.orders:root:0".to_string(),
            0,
            FlussLakePartitionIdentity::Unpartitioned,
            crate::CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor,
            SplitStatistics::new(rows, size),
        )
        .unwrap()
    }

    #[test]
    fn plan_statistics_sum_only_complete_estimates() {
        let complete = vec![split(Some(2), Some(10)), split(Some(3), Some(20))];
        assert_eq!(
            FlussLakePlanStatistics::from_splits(&complete),
            FlussLakePlanStatistics {
                split_count: 2,
                estimated_rows: Some(5),
                estimated_size: Some(30),
            }
        );

        let incomplete = vec![split(Some(2), None), split(None, Some(20))];
        assert_eq!(
            FlussLakePlanStatistics::from_splits(&incomplete),
            FlussLakePlanStatistics {
                split_count: 2,
                estimated_rows: None,
                estimated_size: None,
            }
        );
    }

    #[test]
    fn empty_plan_reports_zero_work() {
        assert_eq!(
            FlussLakePlanStatistics::from_splits(&[]),
            FlussLakePlanStatistics {
                split_count: 0,
                estimated_rows: Some(0),
                estimated_size: Some(0),
            }
        );
    }
}
