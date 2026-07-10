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

//! Custom `ExecutionPlan` for a Fluss bucket scan target.
//!
//! A leaf plan reporting one DataFusion partition per scan target, where a target
//! is a `(partition_id, bucket)` pair (`partition_id` is `None` for a
//! non-partitioned table). `execute(partition)` scans exactly that target, so
//! targets read in parallel. For a partitioned table the target set is the cross
//! product of the partitions kept after pruning and the table's buckets.
//!
//! For log tables, the plan executes a finite earliest-to-latest snapshot scan via
//! [`FlussSource::log_scan`]: `row_limit = Some(n)` means first-N from the head,
//! and `row_limit = None` means a full snapshot through the latest offsets
//! captured at query start. For KV tables, the same plan shape still backs the
//! existing bounded `LIMIT` scan via [`FlussSource::bounded_scan`].
//!
//! The `plan_name` field parameterizes the `EXPLAIN` label so the log provider
//! shows `FlussLogScanExec` and the KV provider shows `FlussKvScanExec`.
//!
//! Asymmetry vs the KV point-lookup path: source-side scans already return
//! batches projected to `projection`, so this plan passes the projection down and
//! must NOT re-project the returned batches.

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

/// Physical plan executing one bounded scan (log or KV) against [`FlussSource`].
pub(crate) struct FlussLogScanExec {
    source: SharedFlussSource,
    table_ref: TableRef,
    /// Column indices into the full table schema; `None` means all columns.
    projection: Option<Vec<usize>>,
    row_limit: Option<usize>,
    /// Whether this plan should use log snapshot semantics (`true`) or KV bounded
    /// scan semantics (`false`).
    log_mode: bool,
    /// Scan targets: one `(partition_id, bucket)` per DataFusion output partition.
    /// `partition_id` is `None` for a non-partitioned table.
    targets: Vec<(Option<i64>, i32)>,
    /// Output (post-projection) schema declared to DataFusion.
    projected_schema: SchemaRef,
    /// `EXPLAIN`/`name()` label: `"FlussLogScanExec"` for log tables,
    /// `"FlussKvScanExec"` for KV tables. Lets one plan serve both without
    /// changing the log table's existing `EXPLAIN` output.
    plan_name: &'static str,
    properties: Arc<PlanProperties>,
}

impl FlussLogScanExec {
    pub(crate) fn new(
        source: SharedFlussSource,
        table_ref: TableRef,
        projection: Option<Vec<usize>>,
        row_limit: Option<usize>,
        log_mode: bool,
        targets: Vec<(Option<i64>, i32)>,
        projected_schema: SchemaRef,
        plan_name: &'static str,
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
            row_limit,
            log_mode,
            targets,
            projected_schema,
            plan_name,
            properties,
        }
    }
}

impl Debug for FlussLogScanExec {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let limit = self
            .row_limit
            .map(|n| n.to_string())
            .unwrap_or_else(|| "None/full".to_string());
        write!(
            f,
            "{}(table={}, limit={}, partitions={})",
            self.plan_name,
            self.table_ref,
            limit,
            self.targets.len()
        )
    }
}

impl DisplayAs for FlussLogScanExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        let limit = self
            .row_limit
            .map(|n| n.to_string())
            .unwrap_or_else(|| "None/full".to_string());
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(
                    f,
                    "{}: table={}, limit={}, partitions={}",
                    self.plan_name,
                    self.table_ref,
                    limit,
                    self.targets.len()
                )
            }
            DisplayFormatType::TreeRender => {
                write!(f, "{}\ntable={}\nlimit={}", self.plan_name, self.table_ref, limit)
            }
        }
    }
}

impl ExecutionPlan for FlussLogScanExec {
    fn name(&self) -> &str {
        self.plan_name
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        // Leaf node: the bounded scan is the data source.
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
        let row_limit = self.row_limit;
        let log_mode = self.log_mode;
        // One DataFusion partition maps to one (partition_id, bucket) target.
        let (partition_id, bucket) = self.targets[partition];

        let future = async move {
            // Source-side scans already project to `projection`; do NOT re-project.
            let batches = if log_mode {
                source
                    .log_scan(
                        &table_ref,
                        partition_id,
                        bucket,
                        projection.as_deref(),
                        row_limit,
                    )
                    .await?
            } else {
                source
                    .bounded_scan(
                        &table_ref,
                        partition_id,
                        bucket,
                        projection.as_deref(),
                        row_limit.expect("KV bounded scan requires LIMIT"),
                    )
                    .await?
            };
            Ok(batches)
        };

        Ok(bounded_batches_stream(self.projected_schema.clone(), future))
    }
}
