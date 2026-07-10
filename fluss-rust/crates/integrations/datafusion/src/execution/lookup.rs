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

//! Custom `ExecutionPlan` for a KV full-primary-key point lookup.
//!
//! A single-partition leaf plan that performs exactly one [`FlussSource::lookup`]
//! and yields 0 or 1 rows. The display name `FlussKvLookupExec` makes the lookup
//! visible in `EXPLAIN`, proving an unsupported query was never silently turned
//! into a generic scan.

use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use arrow::datatypes::SchemaRef;
use datafusion::error::Result as DfResult;
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_expr::{EquivalenceProperties, Partitioning};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};

use crate::source::{LookupKey, SharedFlussSource, TableRef};
use crate::execution::stream::single_batch_stream;
use crate::types::record_batch::project_batch;

/// Physical plan executing one KV point lookup against [`FlussSource`].
pub(crate) struct FlussKvLookupExec {
    source: SharedFlussSource,
    table_ref: TableRef,
    key: LookupKey,
    /// Output (post-projection) schema declared to DataFusion.
    projected_schema: SchemaRef,
    /// Column indices into the full table schema; `None` means all columns.
    projection: Option<Vec<usize>>,
    properties: Arc<PlanProperties>,
}

impl FlussKvLookupExec {
    pub(crate) fn new(
        source: SharedFlussSource,
        table_ref: TableRef,
        key: LookupKey,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
    ) -> Self {
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            // A point lookup is inherently a single partition of <= 1 row.
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        ));
        Self {
            source,
            table_ref,
            key,
            projected_schema,
            projection,
            properties,
        }
    }
}

impl Debug for FlussKvLookupExec {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "FlussKvLookupExec(table={})", self.table_ref)
    }
}

impl DisplayAs for FlussKvLookupExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(
                    f,
                    "FlussKvLookupExec: table={}, key={:?}",
                    self.table_ref, self.key
                )
            }
            DisplayFormatType::TreeRender => {
                write!(f, "FlussKvLookupExec\ntable={}", self.table_ref)
            }
        }
    }
}

impl ExecutionPlan for FlussKvLookupExec {
    fn name(&self) -> &str {
        "FlussKvLookupExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        // Leaf node: the lookup is the data source.
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
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> DfResult<SendableRecordBatchStream> {
        let source = self.source.clone();
        let table_ref = self.table_ref.clone();
        let key = self.key.clone();
        let projection = self.projection.clone();

        let future = async move {
            let batch = source.lookup(&table_ref, &key).await?;
            let projected = project_batch(batch, projection.as_deref())?;
            Ok(projected)
        };

        Ok(single_batch_stream(self.projected_schema.clone(), future))
    }
}
