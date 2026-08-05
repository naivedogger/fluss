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
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::split_descriptor::SplitDescriptor;
use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use fluss::client::FlussConnection;
use fluss::metadata::TablePath;
use fluss::predicate::PruningPredicate;
use futures::Stream;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

const UNION_READ_SPLIT_MAGIC: [u8; 4] = *b"FLUR";
const UNION_READ_SPLIT_HEADER_SIZE: usize = 33;
const STATISTICS_ROWS_PRESENT: u8 = 1;
const STATISTICS_BYTES_PRESENT: u8 = 1 << 1;

/// Current version of the serialized UnionRead split descriptor envelope.
pub const CURRENT_FLUSS_LAKE_SPLIT_VERSION: u32 = 1;

/// Default idle timeout applied to bounded read execution.
///
/// A bounded read has exactly two exits: reaching its frozen stop boundary,
/// or a typed error. The stop boundary existed at plan time, so a fetch that
/// makes no progress for this long is an operational failure, not a reason
/// to wait forever. Override per execution with
/// [`FlussLakeExecutionContext::with_idle_timeout`].
pub const DEFAULT_FLUSS_LAKE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

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
    #[error("invalid UnionRead request: {0}")]
    InvalidRequest(String),

    #[error("invalid UnionRead split: {0}")]
    InvalidSplit(String),

    #[error("unsupported UnionRead split descriptor version {version}")]
    UnsupportedSplitVersion { version: u32 },

    #[error("UnionRead planning failed: {0}")]
    Planning(String),

    #[error("UnionRead execution failed: {0}")]
    Execution(String),

    /// The data behind a frozen read boundary no longer exists.
    ///
    /// Raised when a frozen start offset lies before the server's earliest
    /// offset: log retention has removed data the result depends on, so a
    /// silent read would be silently incomplete. This error is **not
    /// retryable at split level** — the frozen offsets are gone and
    /// re-executing the same split can never succeed. The documented recovery
    /// is re-planning: a fresh [`crate::FlussLakeScan::plan`] freezes
    /// currently-valid boundaries, and the truncated range has typically been
    /// tiered into the lake by then, so the same rows are served from the
    /// lake side instead.
    #[error("UnionRead data unavailable: {0}")]
    DataUnavailable(String),
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

/// Stable identity assigned to an engine predicate for pushdown reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlussLakePredicateId(u32);

impl FlussLakePredicateId {
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn value(self) -> u32 {
        self.0
    }
}

/// One engine predicate translated into Fluss's conservative pruning model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlussLakePredicateInput {
    id: FlussLakePredicateId,
    predicate: PruningPredicate,
}

impl FlussLakePredicateInput {
    pub fn new(id: FlussLakePredicateId, predicate: PruningPredicate) -> Self {
        Self { id, predicate }
    }

    pub fn id(&self) -> FlussLakePredicateId {
        self.id
    }

    pub fn predicate(&self) -> &PruningPredicate {
        &self.predicate
    }
}

/// The level at which UnionRead can use an input predicate.
///
/// V1 intentionally has no exact level. Every input predicate remains an
/// engine residual even when UnionRead can use it for pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlussLakePredicatePushdownLevel {
    /// UnionRead cannot use this predicate.
    Unsupported,

    /// UnionRead may use this predicate to prune data without false negatives.
    PruningOnly,
}

impl FlussLakePredicatePushdownLevel {
    /// Returns whether UnionRead may use the predicate for data pruning.
    pub fn can_prune(self) -> bool {
        matches!(self, Self::PruningOnly)
    }

    /// Returns whether the engine must still evaluate its original predicate.
    pub fn requires_residual_evaluation(self) -> bool {
        true
    }
}

/// Planner decision for one predicate, correlated by engine-assigned id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlussLakePredicatePushdownDecision {
    predicate_id: FlussLakePredicateId,
    level: FlussLakePredicatePushdownLevel,
}

impl FlussLakePredicatePushdownDecision {
    pub fn new(predicate_id: FlussLakePredicateId, level: FlussLakePredicatePushdownLevel) -> Self {
        Self {
            predicate_id,
            level,
        }
    }

    pub fn predicate_id(self) -> FlussLakePredicateId {
        self.predicate_id
    }

    pub fn level(self) -> FlussLakePredicatePushdownLevel {
        self.level
    }
}

/// Engine-neutral input to UnionRead planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FlussLakeScanSpec {
    table_path: TablePath,
    read_mode: FlussLakeReadMode,
    output_projection: Option<Vec<usize>>,
    predicates: Vec<FlussLakePredicateInput>,
    target_parallelism: Option<usize>,
}

impl FlussLakeScanSpec {
    pub(crate) fn new(table_path: TablePath) -> Self {
        Self {
            table_path,
            read_mode: FlussLakeReadMode::Union,
            output_projection: None,
            predicates: Vec::new(),
            target_parallelism: None,
        }
    }

    pub(crate) fn with_read_mode(mut self, read_mode: FlussLakeReadMode) -> Self {
        self.read_mode = read_mode;
        self
    }

    /// Sets the columns that the engine scan needs from UnionRead.
    ///
    /// This is the middle layer of the projection contract:
    ///
    /// * the final SQL projection remains engine-owned;
    /// * this scan output projection must include final output columns and
    ///   columns needed by engine residual predicates;
    /// * UnionRead may add hidden physical columns internally for lake/log
    ///   decoding and primary-key merge, but removes them before producing its
    ///   output schema.
    pub(crate) fn with_output_projection(mut self, output_projection: Vec<usize>) -> Self {
        self.output_projection = Some(output_projection);
        self
    }

    /// Sets engine predicates translated into Fluss's pruning model.
    ///
    /// Predicate ids must be unique within the request. The engine must retain
    /// every original expression for residual evaluation.
    pub(crate) fn with_predicates(mut self, predicates: Vec<FlussLakePredicateInput>) -> Self {
        self.predicates = predicates;
        self
    }

    pub(crate) fn with_target_parallelism(mut self, target_parallelism: usize) -> Self {
        self.target_parallelism = Some(target_parallelism);
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

    pub(crate) fn predicates(&self) -> &[FlussLakePredicateInput] {
        &self.predicates
    }

    pub(crate) fn target_parallelism(&self) -> Option<usize> {
        self.target_parallelism
    }
}

/// Estimated work exposed to an upstream engine for scheduling and costing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlussLakeReadStatistics {
    estimated_rows: Option<u64>,
    estimated_bytes: Option<u64>,
}

impl FlussLakeReadStatistics {
    pub fn new(estimated_rows: Option<u64>, estimated_bytes: Option<u64>) -> Self {
        Self {
            estimated_rows,
            estimated_bytes,
        }
    }

    pub fn estimated_rows(&self) -> Option<u64> {
        self.estimated_rows
    }

    pub fn estimated_bytes(&self) -> Option<u64> {
        self.estimated_bytes
    }
}

/// An opaque, serializable bounded read split.
///
/// Engines may inspect the split id and statistics for scheduling, but must
/// treat the execution descriptor as opaque and consume it through
/// [`crate::FlussLakeRead`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlussLakeReadSplit {
    split_id: String,
    descriptor_version: u32,
    execution_descriptor: Vec<u8>,
    statistics: FlussLakeReadStatistics,
}

impl FlussLakeReadSplit {
    /// Creates a split from a planner-produced execution descriptor.
    pub(crate) fn try_new(
        split_id: String,
        descriptor_version: u32,
        execution_descriptor: Vec<u8>,
        statistics: FlussLakeReadStatistics,
    ) -> FlussLakeResult<Self> {
        if descriptor_version != CURRENT_FLUSS_LAKE_SPLIT_VERSION {
            return Err(FlussLakeError::UnsupportedSplitVersion {
                version: descriptor_version,
            });
        }
        if split_id.is_empty() {
            return Err(FlussLakeError::InvalidSplit(
                "split id must not be empty".to_string(),
            ));
        }
        if execution_descriptor.is_empty() {
            return Err(FlussLakeError::InvalidSplit(
                "execution descriptor must not be empty".to_string(),
            ));
        }
        SplitDescriptor::decode(&execution_descriptor)?;
        Ok(Self {
            split_id,
            descriptor_version,
            execution_descriptor,
            statistics,
        })
    }

    pub fn split_id(&self) -> &str {
        &self.split_id
    }

    pub fn descriptor_version(&self) -> u32 {
        self.descriptor_version
    }

    /// Returns the opaque execution descriptor for transport to an executor.
    pub(crate) fn execution_descriptor(&self) -> &[u8] {
        &self.execution_descriptor
    }

    pub fn statistics(&self) -> FlussLakeReadStatistics {
        self.statistics
    }

    /// Encodes this split for distribution to an execution worker.
    ///
    /// The wire envelope is versioned independently of the internal split
    /// implementation. Engines must treat the returned bytes as opaque.
    pub fn encode(&self) -> FlussLakeResult<Vec<u8>> {
        let split_id = self.split_id.as_bytes();
        let split_id_len = u32::try_from(split_id.len()).map_err(|_| {
            FlussLakeError::InvalidSplit("split id exceeds the wire format limit".to_string())
        })?;
        let descriptor_len = u32::try_from(self.execution_descriptor.len()).map_err(|_| {
            FlussLakeError::InvalidSplit(
                "execution descriptor exceeds the wire format limit".to_string(),
            )
        })?;

        let mut statistics_flags = 0;
        if self.statistics.estimated_rows.is_some() {
            statistics_flags |= STATISTICS_ROWS_PRESENT;
        }
        if self.statistics.estimated_bytes.is_some() {
            statistics_flags |= STATISTICS_BYTES_PRESENT;
        }

        let capacity = UNION_READ_SPLIT_HEADER_SIZE
            .checked_add(split_id.len())
            .and_then(|size| size.checked_add(self.execution_descriptor.len()))
            .ok_or_else(|| {
                FlussLakeError::InvalidSplit("encoded split size overflows usize".to_string())
            })?;
        let mut encoded = Vec::with_capacity(capacity);
        encoded.extend_from_slice(&UNION_READ_SPLIT_MAGIC);
        encoded.extend_from_slice(&self.descriptor_version.to_le_bytes());
        encoded.extend_from_slice(&split_id_len.to_le_bytes());
        encoded.extend_from_slice(&descriptor_len.to_le_bytes());
        encoded.push(statistics_flags);
        encoded.extend_from_slice(
            &self
                .statistics
                .estimated_rows
                .unwrap_or_default()
                .to_le_bytes(),
        );
        encoded.extend_from_slice(
            &self
                .statistics
                .estimated_bytes
                .unwrap_or_default()
                .to_le_bytes(),
        );
        encoded.extend_from_slice(split_id);
        encoded.extend_from_slice(&self.execution_descriptor);
        Ok(encoded)
    }

    /// Decodes an opaque split produced by [`FlussLakeReadSplit::encode`].
    pub fn decode(encoded: &[u8]) -> FlussLakeResult<Self> {
        if encoded.len() < UNION_READ_SPLIT_HEADER_SIZE {
            return Err(FlussLakeError::InvalidSplit(format!(
                "split envelope is truncated: expected at least {UNION_READ_SPLIT_HEADER_SIZE} bytes, got {}",
                encoded.len()
            )));
        }
        if encoded[..UNION_READ_SPLIT_MAGIC.len()] != UNION_READ_SPLIT_MAGIC {
            return Err(FlussLakeError::InvalidSplit(
                "split envelope has an invalid magic header".to_string(),
            ));
        }

        let descriptor_version = read_u32(encoded, 4)?;
        if descriptor_version != CURRENT_FLUSS_LAKE_SPLIT_VERSION {
            return Err(FlussLakeError::UnsupportedSplitVersion {
                version: descriptor_version,
            });
        }

        let split_id_len = read_u32(encoded, 8)? as usize;
        let descriptor_len = read_u32(encoded, 12)? as usize;
        let statistics_flags = encoded[16];
        let known_statistics_flags = STATISTICS_ROWS_PRESENT | STATISTICS_BYTES_PRESENT;
        if statistics_flags & !known_statistics_flags != 0 {
            return Err(FlussLakeError::InvalidSplit(format!(
                "split envelope contains unknown statistics flags 0x{statistics_flags:02x}"
            )));
        }

        let estimated_rows = (statistics_flags & STATISTICS_ROWS_PRESENT != 0)
            .then(|| read_u64(encoded, 17))
            .transpose()?;
        let estimated_bytes = (statistics_flags & STATISTICS_BYTES_PRESENT != 0)
            .then(|| read_u64(encoded, 25))
            .transpose()?;
        let expected_len = UNION_READ_SPLIT_HEADER_SIZE
            .checked_add(split_id_len)
            .and_then(|size| size.checked_add(descriptor_len))
            .ok_or_else(|| {
                FlussLakeError::InvalidSplit("decoded split size overflows usize".to_string())
            })?;
        if encoded.len() != expected_len {
            return Err(FlussLakeError::InvalidSplit(format!(
                "split envelope length mismatch: expected {expected_len} bytes, got {}",
                encoded.len()
            )));
        }

        let split_id_end = UNION_READ_SPLIT_HEADER_SIZE + split_id_len;
        let split_id = std::str::from_utf8(&encoded[UNION_READ_SPLIT_HEADER_SIZE..split_id_end])
            .map_err(|error| {
                FlussLakeError::InvalidSplit(format!("split id is not valid UTF-8: {error}"))
            })?
            .to_string();
        let execution_descriptor = encoded[split_id_end..expected_len].to_vec();
        Self::try_new(
            split_id,
            descriptor_version,
            execution_descriptor,
            FlussLakeReadStatistics::new(estimated_rows, estimated_bytes),
        )
    }
}

fn read_u32(encoded: &[u8], offset: usize) -> FlussLakeResult<u32> {
    let bytes = encoded
        .get(offset..offset + size_of::<u32>())
        .ok_or_else(|| FlussLakeError::InvalidSplit("split envelope is truncated".to_string()))?;
    Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| {
        FlussLakeError::InvalidSplit("invalid u32 in split envelope".to_string())
    })?))
}

fn read_u64(encoded: &[u8], offset: usize) -> FlussLakeResult<u64> {
    let bytes = encoded
        .get(offset..offset + size_of::<u64>())
        .ok_or_else(|| FlussLakeError::InvalidSplit("split envelope is truncated".to_string()))?;
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| {
        FlussLakeError::InvalidSplit("invalid u64 in split envelope".to_string())
    })?))
}

/// A frozen plan whose splits can be distributed and scheduled independently.
#[derive(Debug, Clone)]
pub struct FlussLakeReadPlan {
    output_schema: SchemaRef,
    splits: Vec<FlussLakeReadSplit>,
    statistics: FlussLakeReadStatistics,
    predicate_pushdown_decisions: Vec<FlussLakePredicatePushdownDecision>,
}

impl FlussLakeReadPlan {
    /// Creates a frozen plan from planner-produced splits.
    pub(crate) fn new(
        output_schema: SchemaRef,
        splits: Vec<FlussLakeReadSplit>,
        statistics: FlussLakeReadStatistics,
        predicate_pushdown_decisions: Vec<FlussLakePredicatePushdownDecision>,
    ) -> Self {
        Self {
            output_schema,
            splits,
            statistics,
            predicate_pushdown_decisions,
        }
    }

    pub fn output_schema(&self) -> &SchemaRef {
        &self.output_schema
    }

    pub fn splits(&self) -> &[FlussLakeReadSplit] {
        &self.splits
    }

    pub fn into_splits(self) -> Vec<FlussLakeReadSplit> {
        self.splits
    }

    pub fn statistics(&self) -> FlussLakeReadStatistics {
        self.statistics
    }

    /// Returns conservative pushdown decisions correlated to request ids.
    ///
    /// These decisions never replace the engine's original residual filters.
    pub fn predicate_pushdown_decisions(&self) -> &[FlussLakePredicatePushdownDecision] {
        &self.predicate_pushdown_decisions
    }
}

/// Runtime-only resources supplied while reading frozen splits.
///
/// Cancellation and metrics hooks will be added here as execution backends
/// are introduced. These resources intentionally do not belong to the
/// serializable split descriptor: splits are cached, logged and persisted by
/// engines, so anything secret or environment-bound must arrive through this
/// context instead.
#[derive(Clone, Default)]
pub struct FlussLakeExecutionContext {
    fluss_connection: Option<Arc<FlussConnection>>,
    lake_credentials: HashMap<String, String>,
    memory_limit_bytes: Option<usize>,
    idle_timeout: Option<Duration>,
}

impl FlussLakeExecutionContext {
    pub fn with_fluss_connection(mut self, fluss_connection: Arc<FlussConnection>) -> Self {
        self.fluss_connection = Some(fluss_connection);
        self
    }

    /// Sets the secret lake catalog options withheld from split descriptors.
    ///
    /// Keys use the same names as the lake catalog options (for Paimon, the
    /// `table.datalake.paimon.` property suffixes such as `s3.secret-key`).
    /// At execution time these values override any equally-named option
    /// carried by the split, so credentials rotated after planning take
    /// effect without re-planning.
    pub fn with_lake_credentials(mut self, lake_credentials: HashMap<String, String>) -> Self {
        self.lake_credentials = lake_credentials;
        self
    }

    pub fn with_memory_limit_bytes(mut self, memory_limit_bytes: usize) -> Self {
        self.memory_limit_bytes = Some(memory_limit_bytes);
        self
    }

    /// Overrides [`DEFAULT_FLUSS_LAKE_IDLE_TIMEOUT`] for this execution.
    ///
    /// The timeout bounds the wait for the *next* progress, not the total
    /// read: it resets whenever data arrives. It must be long enough to
    /// cover normal fetch latency, or healthy reads will be failed.
    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = Some(idle_timeout);
        self
    }

    pub fn fluss_connection(&self) -> Option<&Arc<FlussConnection>> {
        self.fluss_connection.as_ref()
    }

    pub fn lake_credentials(&self) -> &HashMap<String, String> {
        &self.lake_credentials
    }

    pub fn memory_limit_bytes(&self) -> Option<usize> {
        self.memory_limit_bytes
    }

    /// Returns the effective idle timeout for bounded read execution.
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout.unwrap_or(DEFAULT_FLUSS_LAKE_IDLE_TIMEOUT)
    }
}

impl Debug for FlussLakeExecutionContext {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        // Credential keys and values must never reach logs; only the count
        // is safe to expose.
        formatter
            .debug_struct("FlussLakeExecutionContext")
            .field("has_fluss_connection", &self.fluss_connection.is_some())
            .field("lake_credential_count", &self.lake_credentials.len())
            .field("memory_limit_bytes", &self.memory_limit_bytes)
            .field("idle_timeout", &self.idle_timeout())
            .finish()
    }
}

/// Plans engine-neutral bounded read splits.
///
/// Planning resolves mutable table state into immutable split descriptions.
/// Engines own scan construction and scheduling, but do not interpret split
/// descriptors or implement lake/log stitch semantics.
pub(crate) trait FlussLakePlanner: Send + Sync {
    fn plan(&self, request: FlussLakeScanSpec) -> FlussLakePlanFuture<'_>;
}

/// Executes one immutable UnionRead split as a finite Arrow batch stream.
///
/// `execute` returns synchronously with a lazy stream: structural problems
/// (undecodable descriptors, unknown kinds, missing required context) fail
/// fast in the call itself, while environment work — opening connections,
/// files and subscriptions — happens on first poll, so environment failures
/// surface as the first stream item. This adapts directly to synchronous
/// engine interfaces such as DataFusion's `ExecutionPlan::execute`.
///
/// The returned stream is bounded even though it is consumed asynchronously,
/// and it has exactly two exits: reaching the frozen split boundary, or a
/// typed error. A read that stops making progress for longer than the
/// context's idle timeout fails instead of waiting forever, and never
/// returns a silent partial result.
///
/// Execution may use runtime resources from the context, but split semantics
/// must come entirely from the frozen split descriptor.
pub(crate) trait FlussLakeExecutor: Send + Sync {
    fn execute(
        &self,
        split: FlussLakeReadSplit,
        context: FlussLakeExecutionContext,
    ) -> FlussLakeResult<FlussLakeRecordBatchStream>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split_descriptor::{AppendLogSplitDescriptor, SplitDescriptor};
    use arrow::datatypes::Schema;
    use fluss::metadata::DataTypes;
    use fluss::predicate::{ComparisonOperator, FieldRef, PruningPredicate};
    use futures::{StreamExt, stream};

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
            .with_predicates(vec![FlussLakePredicateInput::new(
                FlussLakePredicateId::new(7),
                PruningPredicate::comparison(
                    ComparisonOperator::GreaterThan,
                    FieldRef::new(2, "amount", DataTypes::bigint()),
                    100_i64,
                ),
            )])
            .with_target_parallelism(8);

        assert_eq!(request.table_path(), &TablePath::new("fluss", "orders"));
        assert_eq!(request.read_mode(), FlussLakeReadMode::LakeOnly);
        assert_eq!(request.output_projection(), Some([2, 4].as_slice()));
        assert_eq!(request.predicates()[0].id(), FlussLakePredicateId::new(7));
        assert_eq!(request.target_parallelism(), Some(8));
    }

    #[test]
    fn split_descriptor_is_opaque_to_consumers() {
        let statistics = FlussLakeReadStatistics::new(Some(10), Some(100));
        let descriptor = testing_execution_descriptor();
        let split = FlussLakeReadSplit::try_new(
            "bucket-0".to_string(),
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            descriptor.clone(),
            statistics,
        )
        .unwrap();
        let plan = FlussLakeReadPlan::new(
            Arc::new(Schema::empty()),
            vec![split.clone()],
            statistics,
            Vec::new(),
        );

        assert_eq!(split.split_id(), "bucket-0");
        assert_eq!(split.descriptor_version(), CURRENT_FLUSS_LAKE_SPLIT_VERSION);
        assert_eq!(split.execution_descriptor(), descriptor);
        assert_eq!(plan.splits(), &[split]);
        assert_eq!(plan.statistics(), statistics);
    }

    #[test]
    fn pushdown_decisions_never_remove_engine_residuals() {
        let decisions = vec![
            FlussLakePredicatePushdownDecision::new(
                FlussLakePredicateId::new(1),
                FlussLakePredicatePushdownLevel::PruningOnly,
            ),
            FlussLakePredicatePushdownDecision::new(
                FlussLakePredicateId::new(2),
                FlussLakePredicatePushdownLevel::Unsupported,
            ),
        ];
        let plan = FlussLakeReadPlan::new(
            Arc::new(Schema::empty()),
            Vec::new(),
            FlussLakeReadStatistics::default(),
            decisions.clone(),
        );

        assert_eq!(plan.predicate_pushdown_decisions(), decisions);
        assert!(FlussLakePredicatePushdownLevel::PruningOnly.requires_residual_evaluation());
        assert!(FlussLakePredicatePushdownLevel::Unsupported.requires_residual_evaluation());
        assert!(FlussLakePredicatePushdownLevel::PruningOnly.can_prune());
        assert!(!FlussLakePredicatePushdownLevel::Unsupported.can_prune());
    }

    #[test]
    fn rejects_empty_split_identity_and_descriptor() {
        assert!(matches!(
            FlussLakeReadSplit::try_new(
                String::new(),
                CURRENT_FLUSS_LAKE_SPLIT_VERSION,
                vec![1],
                FlussLakeReadStatistics::default()
            ),
            Err(FlussLakeError::InvalidSplit(_))
        ));
        assert!(matches!(
            FlussLakeReadSplit::try_new(
                "split".to_string(),
                CURRENT_FLUSS_LAKE_SPLIT_VERSION,
                Vec::new(),
                FlussLakeReadStatistics::default()
            ),
            Err(FlussLakeError::InvalidSplit(_))
        ));
    }

    #[test]
    fn split_encoding_round_trips_opaque_descriptor() {
        let split = FlussLakeReadSplit::try_new(
            "partition=dt%3D2026-07-28/bucket=3".to_string(),
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            testing_execution_descriptor(),
            FlussLakeReadStatistics::new(Some(42), None),
        )
        .unwrap();

        let decoded = FlussLakeReadSplit::decode(&split.encode().unwrap()).unwrap();

        assert_eq!(decoded, split);
    }

    #[test]
    fn rejects_unknown_split_version() {
        let split = FlussLakeReadSplit::try_new(
            "bucket-0".to_string(),
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            testing_execution_descriptor(),
            FlussLakeReadStatistics::default(),
        )
        .unwrap();
        let mut encoded = split.encode().unwrap();
        encoded[4..8].copy_from_slice(&(CURRENT_FLUSS_LAKE_SPLIT_VERSION + 1).to_le_bytes());

        assert!(matches!(
            FlussLakeReadSplit::decode(&encoded),
            Err(FlussLakeError::UnsupportedSplitVersion { version: 2 })
        ));
    }

    #[test]
    fn rejects_malformed_split_envelopes() {
        let split = FlussLakeReadSplit::try_new(
            "bucket-0".to_string(),
            CURRENT_FLUSS_LAKE_SPLIT_VERSION,
            testing_execution_descriptor(),
            FlussLakeReadStatistics::new(None, Some(20)),
        )
        .unwrap();
        let encoded = split.encode().unwrap();

        assert!(matches!(
            FlussLakeReadSplit::decode(&encoded[..encoded.len() - 1]),
            Err(FlussLakeError::InvalidSplit(_))
        ));

        let mut invalid_magic = encoded.clone();
        invalid_magic[0] = b'X';
        assert!(matches!(
            FlussLakeReadSplit::decode(&invalid_magic),
            Err(FlussLakeError::InvalidSplit(_))
        ));

        let mut unknown_flags = encoded;
        unknown_flags[16] = 1 << 7;
        assert!(matches!(
            FlussLakeReadSplit::decode(&unknown_flags),
            Err(FlussLakeError::InvalidSplit(_))
        ));
    }

    #[test]
    fn planner_and_executor_are_object_safe_engine_boundaries() {
        struct TestingUnionRead;

        impl FlussLakePlanner for TestingUnionRead {
            fn plan(&self, _request: FlussLakeScanSpec) -> FlussLakePlanFuture<'_> {
                Box::pin(async {
                    let split = FlussLakeReadSplit::try_new(
                        "bucket-0".to_string(),
                        CURRENT_FLUSS_LAKE_SPLIT_VERSION,
                        testing_execution_descriptor(),
                        FlussLakeReadStatistics::default(),
                    )?;
                    Ok(FlussLakeReadPlan::new(
                        Arc::new(Schema::empty()),
                        vec![split],
                        FlussLakeReadStatistics::default(),
                        Vec::new(),
                    ))
                })
            }
        }

        impl FlussLakeExecutor for TestingUnionRead {
            fn execute(
                &self,
                _split: FlussLakeReadSplit,
                _context: FlussLakeExecutionContext,
            ) -> FlussLakeResult<FlussLakeRecordBatchStream> {
                let batches =
                    stream::iter(vec![Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))]);
                Ok(Box::pin(batches) as FlussLakeRecordBatchStream)
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
            )
            .unwrap();

        assert!(futures::executor::block_on(batches.next()).unwrap().is_ok());
        assert!(futures::executor::block_on(batches.next()).is_none());
    }

    #[test]
    fn execution_context_debug_does_not_expose_credentials() {
        let mut credentials = HashMap::new();
        credentials.insert("s3.secret-key".to_string(), "TOP-SECRET".to_string());
        let context = FlussLakeExecutionContext::default()
            .with_lake_credentials(credentials)
            .with_memory_limit_bytes(1024);

        let debug = format!("{context:?}");

        assert_eq!(
            debug,
            "FlussLakeExecutionContext { has_fluss_connection: false, lake_credential_count: 1, memory_limit_bytes: Some(1024), idle_timeout: 60s }"
        );
        assert!(!debug.contains("TOP-SECRET"));
        assert!(!debug.contains("secret-key"));
    }
}
