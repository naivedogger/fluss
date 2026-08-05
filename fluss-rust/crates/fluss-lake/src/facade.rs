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

use crate::executor::FlussUnionReadExecutor;
use crate::planner::FlussUnionReadPlanner;
use crate::union_read::{FlussLakeExecutor, FlussLakePlanner, FlussLakeScanSpec};
use crate::{
    FlussLakeError, FlussLakeExecutionContext, FlussLakePredicateInput, FlussLakeReadMode,
    FlussLakeReadPlan, FlussLakeReadSplit, FlussLakeRecordBatchStream, FlussLakeResult,
};
use fluss::client::FlussConnection;
use fluss::metadata::TablePath;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

/// Entry object for bounded reads over a lake-enabled Fluss table.
///
/// The table owns the connection and table identity used to create immutable
/// scan configurations. Planning still resolves fresh metadata before
/// freezing a read, so a long-lived table does not make plans against stale
/// schema or bucket information.
#[derive(Clone)]
pub struct FlussLakeTable {
    connection: Arc<FlussConnection>,
    table_path: TablePath,
}

impl FlussLakeTable {
    /// Opens a table and validates that its metadata can be resolved.
    pub async fn open(
        connection: Arc<FlussConnection>,
        table_path: &TablePath,
    ) -> FlussLakeResult<Self> {
        let admin = connection.get_admin().map_err(|error| {
            FlussLakeError::Planning(format!("failed to create Fluss admin client: {error}"))
        })?;
        admin.get_table_info(table_path).await.map_err(|error| {
            FlussLakeError::Planning(format!(
                "failed to get table metadata for {table_path}: {error}"
            ))
        })?;
        Ok(Self {
            connection,
            table_path: table_path.clone(),
        })
    }

    /// Creates an immutable bounded-read scan for this table.
    pub fn new_scan(&self) -> FlussLakeScan {
        FlussLakeScan::new(self.connection.clone(), self.table_path.clone())
    }

    pub fn table_path(&self) -> &TablePath {
        &self.table_path
    }
}

impl Debug for FlussLakeTable {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlussLakeTable")
            .field("table_path", &self.table_path)
            .finish_non_exhaustive()
    }
}

/// Immutable configuration and planning entry for one bounded lake read.
///
/// This public facade contains both the internal scan specification and the
/// concrete Fluss planner. Callers configure the scan and invoke
/// [`plan`](Self::plan) directly instead of constructing a separate request
/// and planner service.
#[derive(Clone)]
pub struct FlussLakeScan {
    planner: FlussUnionReadPlanner,
    specification: FlussLakeScanSpec,
}

impl FlussLakeScan {
    pub(crate) fn new(connection: Arc<FlussConnection>, table_path: TablePath) -> Self {
        Self {
            planner: FlussUnionReadPlanner::new(connection),
            specification: FlussLakeScanSpec::new(table_path),
        }
    }

    pub fn with_read_mode(mut self, read_mode: FlussLakeReadMode) -> Self {
        self.specification = self.specification.with_read_mode(read_mode);
        self
    }

    pub fn with_output_projection(mut self, output_projection: Vec<usize>) -> Self {
        self.specification = self.specification.with_output_projection(output_projection);
        self
    }

    pub fn with_predicates(mut self, predicates: Vec<FlussLakePredicateInput>) -> Self {
        self.specification = self.specification.with_predicates(predicates);
        self
    }

    pub fn with_target_parallelism(mut self, target_parallelism: usize) -> Self {
        self.specification = self
            .specification
            .with_target_parallelism(target_parallelism);
        self
    }

    /// Freezes the current table boundary and returns bounded read splits.
    pub async fn plan(&self) -> FlussLakeResult<FlussLakeReadPlan> {
        self.planner.plan(self.specification.clone()).await
    }

    /// Creates a reusable read from the same immutable configuration as planning.
    ///
    /// The returned read may consume any number of splits from plans produced by
    /// this scan.
    pub fn new_read(&self, context: FlussLakeExecutionContext) -> FlussLakeResult<FlussLakeRead> {
        Ok(FlussLakeRead::new(self.specification.clone(), context))
    }
}

impl Debug for FlussLakeScan {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlussLakeScan")
            .field("table_path", &self.specification.table_path())
            .field("read_mode", &self.specification.read_mode())
            .field("output_projection", &self.specification.output_projection())
            .field("predicates", &self.specification.predicates())
            .field(
                "target_parallelism",
                &self.specification.target_parallelism(),
            )
            .finish()
    }
}

/// Reusable bounded reader created by [`FlussLakeScan`].
#[derive(Clone)]
pub struct FlussLakeRead {
    specification: FlussLakeScanSpec,
    context: FlussLakeExecutionContext,
}

impl FlussLakeRead {
    fn new(specification: FlussLakeScanSpec, context: FlussLakeExecutionContext) -> Self {
        Self {
            specification,
            context,
        }
    }

    /// Reads one split and returns synchronously with a lazy bounded stream.
    ///
    /// The read itself is reusable; each invocation returns an independent
    /// stream bounded by the supplied split.
    pub fn read_split(
        &self,
        split: FlussLakeReadSplit,
    ) -> FlussLakeResult<FlussLakeRecordBatchStream> {
        FlussUnionReadExecutor.execute(split, self.context.clone())
    }
}

impl Debug for FlussLakeRead {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlussLakeRead")
            .field("table_path", &self.specification.table_path())
            .field("read_mode", &self.specification.read_mode())
            .field("output_projection", &self.specification.output_projection())
            .field("predicates", &self.specification.predicates())
            .field("context", &self.context)
            .finish()
    }
}
