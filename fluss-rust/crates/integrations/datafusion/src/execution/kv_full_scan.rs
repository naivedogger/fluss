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

//! Custom `ExecutionPlan` for a KV full-table scan (changelog merge).
//!
//! A leaf plan reporting one DataFusion partition per scan target, where a target
//! is a `(partition_id, bucket)` pair (`partition_id` is `None` for a
//! non-partitioned table). `execute(partition)` merges exactly that bucket's CDC
//! changelog into its current state via [`FlussSource::kv_full_scan`], so targets
//! read in parallel. For a partitioned table the target set is the cross product
//! of the partitions kept after pruning and the table's buckets.
//!
//! Distinct from [`FlussLogScanExec`](crate::execution::log_scan::FlussLogScanExec):
//! that plan backs log snapshot scans and KV bounded `LIMIT` scans, whereas this
//! plan backs the unbounded (no-filter / no-`LIMIT`) KV full scan. Its `EXPLAIN`
//! label is `FlussKvFullScanExec`.
//!
//! Source-side scans already return batches projected to `projection`, so this
//! plan passes the projection down and must NOT re-project the returned batches.

use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use arrow::datatypes::SchemaRef;
use datafusion::error::Result as DfResult;
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_expr::{EquivalenceProperties, Partitioning};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};

use crate::source::{SharedFlussSource, TableRef};
use crate::execution::stream::bounded_batches_stream;

/// `EXPLAIN`/`name()` label for the KV full-scan plan.
const KV_FULL_SCAN_PLAN_NAME: &str = "FlussKvFullScanExec";

/// Physical plan executing one KV full-table scan (changelog merge) per target.
pub(crate) struct FlussKvFullScanExec {
    source: SharedFlussSource,
    table_ref: TableRef,
    /// Column indices into the full table schema; `None` means all columns.
    projection: Option<Vec<usize>>,
    /// Scan targets: one `(partition_id, bucket)` per DataFusion output partition.
    /// `partition_id` is `None` for a non-partitioned table.
    targets: Vec<(Option<i64>, i32)>,
    /// Output (post-projection) schema declared to DataFusion.
    projected_schema: SchemaRef,
    properties: Arc<PlanProperties>,
}

impl FlussKvFullScanExec {
    pub(crate) fn new(
        source: SharedFlussSource,
        table_ref: TableRef,
        projection: Option<Vec<usize>>,
        targets: Vec<(Option<i64>, i32)>,
        projected_schema: SchemaRef,
    ) -> Self {
        // One DataFusion partition per target so targets read in parallel;
        // `execute(partition)` scans `targets[partition]`.
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            Partitioning::UnknownPartitioning(targets.len()),
            EmissionType::Incremental,
            Boundedness::Bounded,
        ));
        Self {
            source,
            table_ref,
            projection,
            targets,
            projected_schema,
            properties,
        }
    }
}

impl Debug for FlussKvFullScanExec {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}(table={}, partitions={})",
            KV_FULL_SCAN_PLAN_NAME,
            self.table_ref,
            self.targets.len()
        )
    }
}

impl DisplayAs for FlussKvFullScanExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(
                    f,
                    "{}: table={}, partitions={}",
                    KV_FULL_SCAN_PLAN_NAME,
                    self.table_ref,
                    self.targets.len()
                )
            }
            DisplayFormatType::TreeRender => {
                write!(f, "{}\ntable={}", KV_FULL_SCAN_PLAN_NAME, self.table_ref)
            }
        }
    }
}

impl ExecutionPlan for FlussKvFullScanExec {
    fn name(&self) -> &str {
        KV_FULL_SCAN_PLAN_NAME
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        // Leaf node: the changelog merge is the data source.
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        // No children to replace; return self unchanged.
        Ok(self)
    }

    fn execute(
        &self,
        partition: usize,
        _context: Arc<TaskContext>,
    ) -> DfResult<SendableRecordBatchStream> {
        let source = self.source.clone();
        let table_ref = self.table_ref.clone();
        let projection = self.projection.clone();
        // One DataFusion partition maps to one (partition_id, bucket) target.
        let (partition_id, bucket) = self.targets[partition];

        let future = async move {
            // Source-side scan already projects to `projection`; do NOT re-project.
            let batches = source
                .kv_full_scan(&table_ref, partition_id, bucket, projection.as_deref())
                .await?;
            Ok(batches)
        };

        Ok(bounded_batches_stream(self.projected_schema.clone(), future))
    }
}
