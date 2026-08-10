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

//! Public entry points for bounded lake/log reads.

use crate::executor::{FlussUnionReadExecutor, apply_filter_and_projection};
use crate::planner::FlussUnionReadPlanner;
use crate::union_read::{FlussLakeExecutor, FlussLakePlanner, FlussLakeScanSpec};
use crate::{
    FlussLakeError, FlussLakeExecutionContext, FlussLakePredicate, FlussLakeReadMode,
    FlussLakeReadPlan, FlussLakeReadSplit, FlussLakeRecordBatchStream, FlussLakeResult,
};
use fluss::client::FlussConnection;
use fluss::metadata::{TableInfo, TablePath};
use std::collections::HashMap;
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
    properties: HashMap<String, String>,
}

impl FlussLakeTable {
    /// Opens a table and validates that its metadata can be resolved.
    pub async fn open(
        connection: Arc<FlussConnection>,
        table_path: &TablePath,
    ) -> FlussLakeResult<Self> {
        Self::open_with_properties(connection, table_path, HashMap::new()).await
    }

    /// Opens a table with per-table properties that affect planning and execution.
    pub async fn open_with_properties(
        connection: Arc<FlussConnection>,
        table_path: &TablePath,
        properties: HashMap<String, String>,
    ) -> FlussLakeResult<Self> {
        let admin = connection.get_admin().map_err(|error| {
            FlussLakeError::PlanningFailed(format!("failed to create Fluss admin client: {error}"))
        })?;
        let table_info = admin.get_table_info(table_path).await.map_err(|error| {
            FlussLakeError::PlanningFailed(format!(
                "failed to get table metadata for {table_path}: {error}"
            ))
        })?;
        Ok(Self {
            connection,
            table_path: table_path.clone(),
            properties: merge_table_properties(&table_info, properties),
        })
    }

    /// Creates a table handle from an already-resolved [`TableInfo`].
    ///
    /// This avoids an extra metadata round-trip when the caller already has
    /// the table description. The table path is taken from `table_info`.
    pub fn try_from_table_info(
        connection: Arc<FlussConnection>,
        table_info: &TableInfo,
    ) -> FlussLakeResult<Self> {
        Ok(Self {
            connection,
            table_path: table_info.table_path.clone(),
            properties: merge_table_properties(table_info, HashMap::new()),
        })
    }

    /// Creates an immutable bounded-read scan for this table.
    pub fn new_scan(&self) -> FlussLakeScan {
        FlussLakeScan::new(
            self.connection.clone(),
            self.table_path.clone(),
            self.properties.clone(),
        )
    }

    pub fn table_path(&self) -> &TablePath {
        &self.table_path
    }

    /// Returns the per-table properties stored when the table was opened.
    pub fn properties(&self) -> &HashMap<String, String> {
        &self.properties
    }
}

fn merge_table_properties(
    table_info: &TableInfo,
    overrides: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged: HashMap<String, String> = table_info
        .properties
        .iter()
        .chain(table_info.custom_properties.iter())
        .filter(|(key, _)| key.starts_with("table.datalake."))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    merged.extend(overrides);
    merged
}

impl Debug for FlussLakeTable {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlussLakeTable")
            .field("table_path", &self.table_path)
            .field("property_count", &self.properties.len())
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
    connection: Arc<FlussConnection>,
    planner: FlussUnionReadPlanner,
    specification: FlussLakeScanSpec,
    table_properties: HashMap<String, String>,
}

impl FlussLakeScan {
    pub(crate) fn new(
        connection: Arc<FlussConnection>,
        table_path: TablePath,
        table_properties: HashMap<String, String>,
    ) -> Self {
        Self {
            connection: connection.clone(),
            planner: FlussUnionReadPlanner::new(connection, table_properties.clone()),
            specification: FlussLakeScanSpec::new(table_path),
            table_properties,
        }
    }

    /// Sets the bounded read mode.
    pub fn with_read_mode(mut self, read_mode: FlussLakeReadMode) -> Self {
        self.specification = self.specification.with_read_mode(read_mode);
        self
    }

    /// Restricts the output to a column index projection.
    pub fn with_projection(mut self, output_projection: Vec<usize>) -> Self {
        self.specification = self.specification.with_output_projection(output_projection);
        self
    }

    /// Sets the engine filter predicate.
    ///
    /// Multiple calls are combined with AND. Expressions that cannot be
    /// represented as a [`FlussLakePredicate`] must be evaluated by the
    /// engine after UnionRead returns.
    pub fn with_filter(mut self, filter: FlussLakePredicate) -> Self {
        self.specification = self.specification.with_filter(filter);
        self
    }

    /// Convenience shorthand for [`FlussLakeReadMode::LakeOnly`].
    pub fn with_lake_only(mut self) -> Self {
        self.specification = self
            .specification
            .with_read_mode(FlussLakeReadMode::LakeOnly);
        self
    }

    pub fn with_target_parallelism(mut self, target_parallelism: usize) -> Self {
        self.specification = self
            .specification
            .with_target_parallelism(target_parallelism);
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.specification = self.specification.with_batch_size(batch_size);
        self
    }

    /// Freezes the current table boundary and returns bounded read splits.
    pub async fn plan(&self) -> FlussLakeResult<FlussLakeReadPlan> {
        self.planner.plan(self.specification.clone()).await
    }

    /// Creates a reusable reader from the same immutable configuration as planning.
    ///
    /// The returned reader may consume any number of splits from plans produced
    /// by this scan.
    pub fn new_reader(
        &self,
        mut context: FlussLakeExecutionContext,
    ) -> FlussLakeResult<FlussLakeReader> {
        if context.fluss_connection().is_none() {
            context = context.with_fluss_connection(self.connection.clone());
        }
        Ok(FlussLakeReader::new(
            self.specification.clone(),
            context,
            self.table_properties.clone(),
        ))
    }
}

impl Debug for FlussLakeScan {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlussLakeScan")
            .field("table_path", &self.specification.table_path())
            .field("read_mode", &self.specification.read_mode())
            .field("output_projection", &self.specification.output_projection())
            .field("filter", &self.specification.filter())
            .field(
                "target_parallelism",
                &self.specification.target_parallelism(),
            )
            .finish()
    }
}

/// Reusable bounded reader created by [`FlussLakeScan`].
#[derive(Clone)]
pub struct FlussLakeReader {
    specification: FlussLakeScanSpec,
    context: FlussLakeExecutionContext,
    table_properties: HashMap<String, String>,
}

impl FlussLakeReader {
    fn new(
        specification: FlussLakeScanSpec,
        context: FlussLakeExecutionContext,
        table_properties: HashMap<String, String>,
    ) -> Self {
        Self {
            specification,
            context,
            table_properties,
        }
    }

    /// Reads one split and returns a lazy bounded stream.
    pub async fn read_split(
        &self,
        split: &FlussLakeReadSplit,
    ) -> FlussLakeResult<FlussLakeRecordBatchStream> {
        let stream = FlussUnionReadExecutor.execute(
            split.clone(),
            self.context.clone(),
            self.specification.clone(),
            self.table_properties.clone(),
        )?;
        Ok(apply_filter_and_projection(
            stream,
            self.specification.filter(),
            self.specification.output_projection().map(<[usize]>::len),
        ))
    }

    /// Reads all splits as one merged stream.
    ///
    /// Ordering between logical splits is intentionally unspecified.
    pub async fn read_splits(
        &self,
        splits: &[FlussLakeReadSplit],
    ) -> FlussLakeResult<FlussLakeRecordBatchStream> {
        let mut streams = Vec::with_capacity(splits.len());
        for split in splits {
            streams.push(FlussUnionReadExecutor.execute(
                split.clone(),
                self.context.clone(),
                self.specification.clone(),
                self.table_properties.clone(),
            )?);
        }
        let merged = merge_split_streams(streams);
        Ok(apply_filter_and_projection(
            merged,
            self.specification.filter(),
            self.specification.output_projection().map(<[usize]>::len),
        ))
    }

    /// Returns the scan-level filter carried by this reader.
    pub fn filter(&self) -> &FlussLakePredicate {
        self.specification.filter()
    }

    /// Returns the merged table properties available to the reader.
    pub fn properties(&self) -> &HashMap<String, String> {
        &self.table_properties
    }
}

fn merge_split_streams(streams: Vec<FlussLakeRecordBatchStream>) -> FlussLakeRecordBatchStream {
    Box::pin(futures::stream::select_all(streams))
}

impl Debug for FlussLakeReader {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlussLakeReader")
            .field("table_path", &self.specification.table_path())
            .field("read_mode", &self.specification.read_mode())
            .field("output_projection", &self.specification.output_projection())
            .field("filter", &self.specification.filter())
            .field("context", &self.context)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, ArrayRef, Int32Array};
    use fluss::metadata::{DataTypes, Schema};
    use futures::TryStreamExt;

    #[test]
    fn multiple_split_streams_are_exposed_as_one_stream() {
        let batch = |value| {
            arrow::record_batch::RecordBatch::try_from_iter(vec![(
                "id",
                Arc::new(Int32Array::from(vec![value])) as ArrayRef,
            )])
            .unwrap()
        };
        let streams: Vec<FlussLakeRecordBatchStream> = vec![
            Box::pin(futures::stream::iter(vec![Ok(batch(1))])),
            Box::pin(futures::stream::iter(vec![Ok(batch(2))])),
        ];

        let batches: Vec<_> =
            futures::executor::block_on(merge_split_streams(streams).try_collect()).unwrap();
        let mut values: Vec<i32> = batches
            .iter()
            .map(|batch| {
                batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int32Array>()
                    .unwrap()
                    .value(0)
            })
            .collect();
        values.sort_unstable();

        assert_eq!(values, vec![1, 2]);
    }

    #[test]
    fn caller_table_properties_override_server_datalake_properties() {
        let schema = Schema::builder()
            .column("id", DataTypes::int())
            .build()
            .unwrap();
        let mut properties = HashMap::new();
        properties.insert(
            "table.datalake.paimon.warehouse".to_string(),
            "s3://server".to_string(),
        );
        properties.insert("table.merge-engine".to_string(), "deduplicate".to_string());
        let table_info = TableInfo::new(
            TablePath::new("fluss", "orders"),
            7,
            1,
            schema,
            vec!["id".to_string()],
            Vec::<String>::new().into(),
            1,
            properties,
            HashMap::new(),
            None,
            0,
            0,
        );
        let mut overrides = HashMap::new();
        overrides.insert(
            "table.datalake.paimon.warehouse".to_string(),
            "s3://caller".to_string(),
        );

        let merged = merge_table_properties(&table_info, overrides);

        assert_eq!(
            merged.get("table.datalake.paimon.warehouse"),
            Some(&"s3://caller".to_string())
        );
        assert!(!merged.contains_key("table.merge-engine"));
    }
}
