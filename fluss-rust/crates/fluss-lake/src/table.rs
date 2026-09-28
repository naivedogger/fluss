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

//! Table and scan APIs for bounded UnionRead.

use crate::planner::{plan_union_read, plan_with_context, prepare_read_context};
use crate::{FlussLakeError, FlussLakeReadContext, FlussLakeReadPlan, RecordBatchStream, Result};
use fluss::client::FlussConnection;
use fluss::error::Error as ClientError;
use fluss::metadata::{RowType, TableInfo, TablePath};
use fluss::predicate::Predicate;
use futures::StreamExt;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

/// Projection requested by a scan before it is resolved against fresh metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FlussLakeProjection {
    Indices(Vec<usize>),
    Names(Vec<String>),
}

/// Entry point for bounded reads over a lake-enabled Fluss table.
#[derive(Clone)]
pub struct FlussLakeTable {
    connection: Arc<FlussConnection>,
    table_path: TablePath,
    catalog_property_overrides: HashMap<String, String>,
}

impl FlussLakeTable {
    /// Opens a table and validates that it is configured for lake reads.
    pub async fn open(connection: Arc<FlussConnection>, table_path: &TablePath) -> Result<Self> {
        Self::open_with_properties(connection, table_path, HashMap::new()).await
    }

    /// Opens a table with runtime lake properties such as storage credentials.
    ///
    /// Runtime properties are held by the table and reader. They are never
    /// serialized into a read split.
    pub async fn open_with_properties(
        connection: Arc<FlussConnection>,
        table_path: &TablePath,
        catalog_property_overrides: HashMap<String, String>,
    ) -> Result<Self> {
        let admin = connection
            .get_admin()
            .map_err(|error| table_client_error("create Fluss admin client", error))?;
        let table_info = admin
            .get_table_info(table_path)
            .await
            .map_err(|error| table_client_error("get table metadata", error))?;
        validate_lake_readable(&table_info)?;
        Ok(Self {
            connection,
            table_path: table_path.clone(),
            catalog_property_overrides,
        })
    }

    /// Creates a table from metadata already resolved by the caller.
    pub fn try_from_table_info(
        connection: Arc<FlussConnection>,
        table_info: &TableInfo,
    ) -> Result<Self> {
        validate_lake_readable(table_info)?;
        Ok(Self {
            connection,
            table_path: table_info.table_path.clone(),
            catalog_property_overrides: HashMap::new(),
        })
    }

    /// Creates a bounded read scan.
    pub fn new_scan(&self) -> FlussLakeScan {
        FlussLakeScan {
            connection: self.connection.clone(),
            table_path: self.table_path.clone(),
            projection: None,
            filter: None,
            batch_size: None,
            lake_only: false,
            lake_source: None,
            catalog_property_overrides: self.catalog_property_overrides.clone(),
        }
    }

    /// Captures table-wide source boundaries without planning lake files.
    ///
    /// The result may be shared by scans with different projections and
    /// predicates, including separate references to the table in one query.
    /// This requires no `paimon` feature and does not acquire a retention lease.
    pub async fn prepare(&self) -> Result<FlussLakeReadContext> {
        self.new_scan().prepare().await
    }
}

impl Debug for FlussLakeTable {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlussLakeTable")
            .field("table_path", &self.table_path)
            .field(
                "catalog_property_override_count",
                &self.catalog_property_overrides.len(),
            )
            .finish_non_exhaustive()
    }
}

/// Immutable configuration for one bounded UnionRead.
///
/// Planning freezes this configuration together with the source inputs.
/// Create readers from the resulting plan, not from an independently configured scan.
#[derive(Clone)]
pub struct FlussLakeScan {
    connection: Arc<FlussConnection>,
    table_path: TablePath,
    projection: Option<FlussLakeProjection>,
    filter: Option<Predicate>,
    batch_size: Option<usize>,
    lake_only: bool,
    pub(crate) lake_source: Option<Arc<dyn crate::LakeSource>>,
    catalog_property_overrides: HashMap<String, String>,
}

impl FlussLakeScan {
    /// Restricts output to table field indexes, in the requested order.
    /// An empty projection preserves row counts in zero-column output batches.
    pub fn with_projection(mut self, projection: Vec<usize>) -> Self {
        self.projection = Some(FlussLakeProjection::Indices(projection));
        self
    }

    /// Restricts output to table field names, in the requested order.
    pub fn with_projection_by_names(mut self, projection: Vec<String>) -> Self {
        self.projection = Some(FlussLakeProjection::Names(projection));
        self
    }

    /// Adds an exact filter. Repeated calls are combined with `AND`.
    pub fn with_filter(mut self, filter: Predicate) -> Self {
        self.filter = Some(match self.filter.take() {
            Some(existing) => existing.and(filter),
            None => filter,
        });
        self
    }

    /// Sets the target output batch size.
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = Some(batch_size);
        self
    }

    /// Enables or disables lake-only execution.
    pub fn with_lake_only(mut self, lake_only: bool) -> Self {
        self.lake_only = lake_only;
        self
    }

    /// Captures source boundaries for an engine-native or default execution.
    ///
    /// Projection and filter are validated but do not prune this table-wide
    /// context. They are applied by `plan_with_context` or by the host engine.
    /// No lake catalog or file reader is opened by this method.
    pub async fn prepare(&self) -> Result<FlussLakeReadContext> {
        prepare_read_context(self).await
    }

    /// Prepares source boundaries and builds the complete default read plan.
    ///
    /// Create the reader using `plan.new_reader()`. Lake planning and reading
    /// use the injected source or the feature-selected default implementation.
    pub async fn plan(&self) -> Result<FlussLakeReadPlan> {
        plan_union_read(self).await
    }

    /// Builds a default plan without choosing new snapshots or log offsets.
    ///
    /// The context must refer to this table. Metadata drift is rejected.
    /// Different scans may share a context without sharing predicates or
    /// projection. Engines implementing their own UnionRead can instead
    /// consume the public context directly and skip this default planner.
    pub async fn plan_with_context(
        &self,
        context: &FlussLakeReadContext,
    ) -> Result<FlussLakeReadPlan> {
        plan_with_context(self, context).await
    }

    /// Use a lake backend instead of the feature-selected default implementation.
    /// The source owns both task planning and reading; it may delegate either
    /// operation to another compatible source. It must match the table format.
    pub fn with_lake_source(mut self, source: Arc<dyn crate::LakeSource>) -> Self {
        self.lake_source = Some(source);
        self
    }

    pub(crate) fn connection(&self) -> &Arc<FlussConnection> {
        &self.connection
    }

    pub(crate) fn table_path(&self) -> &TablePath {
        &self.table_path
    }

    pub(crate) fn projection(&self) -> Option<&FlussLakeProjection> {
        self.projection.as_ref()
    }

    pub(crate) fn resolve_projection(&self, row_type: &RowType) -> Result<Option<Vec<usize>>> {
        let projection = match self.projection() {
            Some(FlussLakeProjection::Indices(projection)) => projection.clone(),
            Some(FlussLakeProjection::Names(names)) => {
                let mut projection = Vec::with_capacity(names.len());
                for name in names {
                    let field_index = row_type
                        .fields()
                        .iter()
                        .position(|field| field.name() == name)
                        .ok_or_else(|| {
                            FlussLakeError::PlanningFailed(format!(
                                "output projection references unknown field '{name}'"
                            ))
                        })?;
                    projection.push(field_index);
                }
                projection
            }
            None => return Ok(None),
        };

        let mut seen = std::collections::HashSet::with_capacity(projection.len());
        for field_index in &projection {
            if *field_index >= row_type.fields().len() {
                return Err(FlussLakeError::PlanningFailed(format!(
                    "output projection field index {field_index} exceeds table width {}",
                    row_type.fields().len()
                )));
            }
            if !seen.insert(*field_index) {
                return Err(FlussLakeError::PlanningFailed(format!(
                    "output projection contains duplicate field index {field_index}"
                )));
            }
        }
        Ok(Some(projection))
    }

    pub(crate) fn filter(&self) -> Option<&Predicate> {
        self.filter.as_ref()
    }

    pub(crate) fn batch_size(&self) -> Option<usize> {
        self.batch_size
    }

    pub(crate) fn lake_only(&self) -> bool {
        self.lake_only
    }

    #[cfg(feature = "paimon")]
    pub(crate) fn catalog_property_overrides(&self) -> &HashMap<String, String> {
        &self.catalog_property_overrides
    }

    pub(crate) fn validate_configuration(&self) -> Result<()> {
        if self.table_path.database().is_empty() || self.table_path.table().is_empty() {
            return Err(FlussLakeError::PlanningFailed(
                "database and table names must not be empty".to_string(),
            ));
        }
        if self.batch_size == Some(0) {
            return Err(FlussLakeError::PlanningFailed(
                "batch size must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

impl Debug for FlussLakeScan {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FlussLakeScan")
            .field("table_path", &self.table_path)
            .field("projection", &self.projection)
            .field("filter", &self.filter)
            .field("batch_size", &self.batch_size)
            .field("lake_only", &self.lake_only)
            .field(
                "catalog_property_override_count",
                &self.catalog_property_overrides.len(),
            )
            .finish()
    }
}

pub(crate) fn validate_lake_readable(table_info: &TableInfo) -> Result<()> {
    match table_info.table_config.is_datalake_enabled() {
        Ok(true) => {}
        Ok(false) => {
            return Err(FlussLakeError::NotLakeReadable(format!(
                "table {} does not enable table.datalake.enabled",
                table_info.table_path
            )));
        }
        Err(error) => {
            return Err(FlussLakeError::NotLakeReadable(format!(
                "table {} has an invalid lake configuration: {error}",
                table_info.table_path
            )));
        }
    }
    match table_info.table_config.get_datalake_format() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(FlussLakeError::NotLakeReadable(format!(
            "table {} does not define table.datalake.format",
            table_info.table_path
        ))),
        Err(error) => Err(FlussLakeError::NotLakeReadable(format!(
            "table {} has an invalid lake configuration: {error}",
            table_info.table_path
        ))),
    }
}

fn table_client_error(action: &str, error: ClientError) -> FlussLakeError {
    match error {
        ClientError::RpcError { .. } => {
            FlussLakeError::ConnectionError(format!("failed to {action}: {error}"))
        }
        _ => FlussLakeError::PlanningFailed(format!("failed to {action}: {error}")),
    }
}

/// Makes the first stream error terminal and immediately drops the source.
///
/// This is especially important for `read_splits`: dropping `select_all`
/// cancels every sibling split as soon as one split invalidates the attempt.
pub(crate) fn stop_after_first_error(stream: RecordBatchStream) -> RecordBatchStream {
    Box::pin(futures::stream::unfold(Some(stream), |stream| async move {
        let mut stream = stream?;
        match stream.next().await {
            Some(Ok(batch)) => Some((Ok(batch), Some(stream))),
            Some(Err(error)) => Some((Err(error), None)),
            None => None,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ArrayRef, Int32Array};
    use fluss::metadata::{DataTypes, Schema};

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn reader_is_send_and_sync() {
        assert_send_sync::<crate::FlussLakeReader>();
    }

    #[test]
    fn reader_stream_stops_after_the_first_error() {
        let batch = arrow::record_batch::RecordBatch::try_from_iter(vec![(
            "id",
            Arc::new(Int32Array::from(vec![1])) as ArrayRef,
        )])
        .unwrap();
        let source: RecordBatchStream = Box::pin(futures::stream::iter(vec![
            Ok(batch.clone()),
            Err(FlussLakeError::DataUnavailable(
                "planned range expired".to_string(),
            )),
            Ok(batch),
        ]));

        let items = futures::executor::block_on(stop_after_first_error(source).collect::<Vec<_>>());

        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok());
        assert!(matches!(items[1], Err(FlussLakeError::DataUnavailable(_))));
    }

    #[test]
    fn preconfigured_lake_format_is_not_readable_until_enabled() {
        let schema = Schema::builder()
            .column("id", DataTypes::int())
            .build()
            .unwrap();
        let table_info = TableInfo::new(
            TablePath::new("fluss", "orders"),
            7,
            1,
            schema,
            Vec::new(),
            Vec::<String>::new().into(),
            1,
            HashMap::from([("table.datalake.format".to_string(), "paimon".to_string())]),
            HashMap::new(),
            None,
            0,
            0,
        );

        assert!(matches!(
            validate_lake_readable(&table_info),
            Err(FlussLakeError::NotLakeReadable(_))
        ));
    }
}
