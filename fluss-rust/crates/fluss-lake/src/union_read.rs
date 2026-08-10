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

//! Core UnionRead traits, errors and scan specification.
//!
//! Concrete predicates, splits, plans and execution context live in
//! [`crate::predicate`], [`crate::split`], [`crate::plan`] and
//! [`crate::context`] respectively and are re-exported here for convenience.

pub use crate::context::FlussLakeExecutionContext;
pub use crate::plan::{FlussLakePlanStatistics, FlussLakeReadPlan};
pub use crate::predicate::{FlussLakeComparisonOp, FlussLakeLiteral, FlussLakePredicate};
pub use crate::split::{
    CURRENT_FLUSS_LAKE_SPLIT_VERSION, FlussLakePartitionIdentity, FlussLakeReadSplit,
    FlussLakeReadStatistics,
};

use arrow::record_batch::RecordBatch;
use fluss::metadata::TablePath;
use futures::Stream;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

/// Result type returned by UnionRead planning and execution APIs.
pub type FlussLakeResult<T> = std::result::Result<T, FlussLakeError>;

/// A finite stream of Arrow record batches produced from bounded UnionRead splits.
///
/// Despite the `Stream` name, this represents a bounded batch result. The
/// stream terminates after the immutable split boundary has been consumed.
pub type FlussLakeRecordBatchStream =
    Pin<Box<dyn Stream<Item = FlussLakeResult<RecordBatch>> + Send>>;

/// Future returned while constructing a frozen UnionRead plan.
pub(crate) type FlussLakePlanFuture<'a> =
    Pin<Box<dyn Future<Output = FlussLakeResult<FlussLakeReadPlan>> + Send + 'a>>;

/// Errors surfaced by the UnionRead planning and execution contract.
#[derive(Debug, Error)]
pub enum FlussLakeError {
    /// Requested table is not configured for lake reads.
    #[error("table is not lake-readable: {0}")]
    NotLakeReadable(String),

    /// Input to planning was invalid.
    #[error("invalid UnionRead request: {0}")]
    InvalidRequest(String),

    /// Transported split bytes are malformed or incompatible.
    #[error("invalid UnionRead split: {0}")]
    InvalidSplit(String),

    /// Split descriptor version is not supported by this reader.
    #[error("unsupported UnionRead split descriptor version {version}")]
    UnsupportedSplitVersion { version: u32 },

    /// Planning failed for a reason other than invalid input.
    #[error("UnionRead planning failed: {0}")]
    PlanningFailed(String),

    /// Execution failed for a reason other than invalid split or schema drift.
    #[error("UnionRead execution failed: {0}")]
    Execution(String),

    /// The data behind a frozen read boundary no longer exists.
    #[error("UnionRead data unavailable: {0}")]
    DataUnavailable(String),

    /// Schema of the resolved table is incompatible with the frozen split.
    #[error("UnionRead schema incompatible: {0}")]
    SchemaIncompatible(String),

    /// Connection to Fluss or the lake catalog failed.
    #[error("UnionRead connection error: {0}")]
    ConnectionError(String),

    /// Merge engine is not supported by UnionRead.
    #[error("unsupported merge engine: {0}")]
    UnsupportedMergeEngine(String),

    /// Split descriptor version is incompatible with this reader.
    #[error("incompatible UnionRead split version: {0}")]
    IncompatibleSplitVersion(String),

    /// Internal error that should not escape to engines.
    #[error("UnionRead internal error: {0}")]
    Internal(String),
}

/// Bounded read mode requested by an upstream engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlussLakeReadMode {
    /// Read a fixed lake snapshot together with its bounded Fluss log tail.
    #[default]
    Union,

    /// Read only the fixed lake snapshot.
    LakeOnly,
}

/// Engine-neutral input to UnionRead planning.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FlussLakeScanSpec {
    table_path: TablePath,
    read_mode: FlussLakeReadMode,
    output_projection: Option<Vec<usize>>,
    filter: FlussLakePredicate,
    target_parallelism: Option<usize>,
    batch_size: Option<usize>,
}

impl FlussLakeScanSpec {
    pub(crate) fn new(table_path: TablePath) -> Self {
        Self {
            table_path,
            read_mode: FlussLakeReadMode::Union,
            output_projection: None,
            filter: FlussLakePredicate::always_true(),
            target_parallelism: None,
            batch_size: None,
        }
    }

    pub(crate) fn with_read_mode(mut self, read_mode: FlussLakeReadMode) -> Self {
        self.read_mode = read_mode;
        self
    }

    /// Sets the columns that the engine scan needs from UnionRead.
    pub(crate) fn with_output_projection(mut self, output_projection: Vec<usize>) -> Self {
        self.output_projection = Some(output_projection);
        self
    }

    /// Sets the engine filter predicate.
    ///
    /// Multiple calls are combined with AND. The engine must retain the
    /// original expression for residual evaluation of expressions that cannot
    /// be represented as a [`FlussLakePredicate`].
    pub(crate) fn with_filter(mut self, filter: FlussLakePredicate) -> Self {
        self.filter = FlussLakePredicate::and([self.filter, filter]);
        self
    }

    pub(crate) fn with_target_parallelism(mut self, target_parallelism: usize) -> Self {
        self.target_parallelism = Some(target_parallelism);
        self
    }

    pub(crate) fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = Some(batch_size);
        self
    }

    pub(crate) fn table_path(&self) -> &TablePath {
        &self.table_path
    }

    pub(crate) fn read_mode(&self) -> FlussLakeReadMode {
        self.read_mode
    }

    pub(crate) fn output_projection(&self) -> Option<&[usize]> {
        self.output_projection.as_deref()
    }

    pub(crate) fn filter(&self) -> &FlussLakePredicate {
        &self.filter
    }

    pub(crate) fn target_parallelism(&self) -> Option<usize> {
        self.target_parallelism
    }
}

/// Plans engine-neutral bounded read splits.
pub(crate) trait FlussLakePlanner: Send + Sync {
    fn plan(&self, request: FlussLakeScanSpec) -> FlussLakePlanFuture<'_>;
}

/// Executes one immutable UnionRead split as a finite Arrow batch stream.
pub(crate) trait FlussLakeExecutor: Send + Sync {
    fn execute(
        &self,
        split: FlussLakeReadSplit,
        context: FlussLakeExecutionContext,
        specification: FlussLakeScanSpec,
        table_properties: HashMap<String, String>,
    ) -> FlussLakeResult<FlussLakeRecordBatchStream>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split_descriptor::{AppendLogSplitDescriptor, SplitDescriptor};
    use arrow::datatypes::Schema;
    use futures::{StreamExt, stream};
    use std::sync::Arc;

    fn testing_execution_descriptor() -> Vec<u8> {
        SplitDescriptor::AppendLog(
            AppendLogSplitDescriptor::try_new(
                TablePath::new("fluss", "orders"),
                1,
                fluss::metadata::TableBucket::new(5, 0),
                0,
                1,
                None,
            )
            .unwrap(),
        )
        .encode()
        .unwrap()
    }

    #[test]
    fn request_distinguishes_scan_output_projection() {
        let request = FlussLakeScanSpec::new(TablePath::new("fluss", "orders"))
            .with_read_mode(FlussLakeReadMode::LakeOnly)
            .with_output_projection(vec![2, 4])
            .with_filter(FlussLakePredicate::gt("id", 100_i64))
            .with_target_parallelism(8);

        assert_eq!(request.table_path(), &TablePath::new("fluss", "orders"));
        assert_eq!(request.read_mode(), FlussLakeReadMode::LakeOnly);
        assert_eq!(request.output_projection(), Some([2, 4].as_slice()));
        assert_eq!(request.filter(), &FlussLakePredicate::gt("id", 100_i64));
        assert_eq!(request.target_parallelism(), Some(8));
    }

    #[test]
    fn planner_and_executor_are_object_safe_engine_boundaries() {
        struct TestingUnionRead;

        impl FlussLakePlanner for TestingUnionRead {
            fn plan(&self, _request: FlussLakeScanSpec) -> FlussLakePlanFuture<'_> {
                Box::pin(async {
                    let split = FlussLakeReadSplit::try_new(
                        "bucket-0".to_string(),
                        0,
                        None,
                        CURRENT_FLUSS_LAKE_SPLIT_VERSION,
                        testing_execution_descriptor(),
                        FlussLakeReadStatistics::default(),
                    )?;
                    Ok(FlussLakeReadPlan::new(
                        Arc::new(Schema::empty()),
                        vec![split],
                        FlussLakePlanStatistics::new(1),
                    ))
                })
            }
        }

        impl FlussLakeExecutor for TestingUnionRead {
            fn execute(
                &self,
                _split: FlussLakeReadSplit,
                _context: FlussLakeExecutionContext,
                _specification: FlussLakeScanSpec,
                _table_properties: HashMap<String, String>,
            ) -> FlussLakeResult<FlussLakeRecordBatchStream> {
                let batches: FlussLakeRecordBatchStream =
                    Box::pin(stream::iter(vec![Ok::<_, FlussLakeError>(
                        RecordBatch::new_empty(Arc::new(Schema::empty())),
                    )]));
                Ok(batches)
            }
        }

        let service = TestingUnionRead;
        let planner: &dyn FlussLakePlanner = &service;
        let executor: &dyn FlussLakeExecutor = &service;
        let plan = futures::executor::block_on(
            planner.plan(FlussLakeScanSpec::new(TablePath::new("fluss", "orders"))),
        )
        .unwrap();
        let mut batches = executor
            .execute(
                plan.splits()[0].clone(),
                FlussLakeExecutionContext::default(),
                FlussLakeScanSpec::new(TablePath::new("fluss", "orders")),
                HashMap::new(),
            )
            .unwrap();

        assert!(futures::executor::block_on(batches.next()).unwrap().is_ok());
        assert!(futures::executor::block_on(batches.next()).is_none());
    }

    #[test]
    fn execution_context_debug_does_not_expose_credentials() {
        use std::collections::HashMap;

        let mut credentials = HashMap::new();
        credentials.insert("s3.secret-key".to_string(), "TOP-SECRET".to_string());
        let context = FlussLakeExecutionContext::default()
            .with_lake_credentials(credentials)
            .with_memory_limit_bytes(1024);

        let debug = format!("{context:?}");

        assert_eq!(
            debug,
            "FlussLakeExecutionContext { has_fluss_connection: false, lake_credential_count: 1, memory_limit_bytes: Some(1024) }"
        );
        assert!(!debug.contains("TOP-SECRET"));
        assert!(!debug.contains("secret-key"));
    }
}
