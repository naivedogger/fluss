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

use crate::task::TaskDescriptor;
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

const UNION_READ_TASK_MAGIC: [u8; 4] = *b"FLUR";
const UNION_READ_TASK_HEADER_SIZE: usize = 33;
const STATISTICS_ROWS_PRESENT: u8 = 1;
const STATISTICS_BYTES_PRESENT: u8 = 1 << 1;

/// Current version of the serialized UnionRead task descriptor envelope.
pub const CURRENT_UNION_READ_TASK_VERSION: u32 = 1;

/// Default idle timeout applied to bounded read execution.
///
/// A bounded read has exactly two exits: reaching its frozen stop boundary,
/// or a typed error. The stop boundary existed at plan time, so a fetch that
/// makes no progress for this long is an operational failure, not a reason
/// to wait forever. Override per execution with
/// [`UnionReadExecutionContext::with_idle_timeout`].
pub const DEFAULT_UNION_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Result type returned by UnionRead planning and execution APIs.
pub type UnionReadResult<T> = std::result::Result<T, UnionReadError>;

/// A finite stream of Arrow record batches produced by one UnionRead task.
///
/// Despite the `Stream` name, this represents a bounded batch result. The
/// stream terminates after the immutable task boundary has been consumed.
pub type SendableRecordBatchStream =
    Pin<Box<dyn Stream<Item = UnionReadResult<RecordBatch>> + Send>>;

/// Future returned while constructing a frozen UnionRead plan.
pub type UnionReadPlanFuture<'a> =
    Pin<Box<dyn Future<Output = UnionReadResult<UnionReadPlan>> + Send + 'a>>;

/// Errors surfaced by the UnionRead planning and execution contract.
#[derive(Debug, Error)]
pub enum UnionReadError {
    #[error("invalid UnionRead request: {0}")]
    InvalidRequest(String),

    #[error("invalid UnionRead task: {0}")]
    InvalidTask(String),

    #[error("unsupported UnionRead task descriptor version {version}")]
    UnsupportedTaskVersion { version: u32 },

    #[error("UnionRead planning failed: {0}")]
    Planning(String),

    #[error("UnionRead execution failed: {0}")]
    Execution(String),

    /// The data behind a frozen read boundary no longer exists.
    ///
    /// Raised when a frozen start offset lies before the server's earliest
    /// offset: log retention has removed data the result depends on, so a
    /// silent read would be silently incomplete. This error is **not
    /// retryable at task level** — the frozen offsets are gone and
    /// re-executing the same task can never succeed. The documented recovery
    /// is re-planning: a fresh [`UnionReadPlanner::plan`] freezes
    /// currently-valid boundaries, and the truncated range has typically been
    /// tiered into the lake by then, so the same rows are served from the
    /// lake side instead.
    #[error("UnionRead data unavailable: {0}")]
    DataUnavailable(String),
}

/// Bounded read mode requested by an upstream engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnionReadMode {
    /// Read a fixed lake snapshot together with its bounded Fluss log tail.
    #[default]
    Union,

    /// Read only the fixed lake snapshot.
    LakeOnly,
}

/// Stable identity assigned to an engine predicate for pushdown reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PredicateId(u32);

impl PredicateId {
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn value(self) -> u32 {
        self.0
    }
}

/// One engine predicate translated into Fluss's conservative pruning model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredicateInput {
    id: PredicateId,
    predicate: PruningPredicate,
}

impl PredicateInput {
    pub fn new(id: PredicateId, predicate: PruningPredicate) -> Self {
        Self { id, predicate }
    }

    pub fn id(&self) -> PredicateId {
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
pub enum PredicatePushdownLevel {
    /// UnionRead cannot use this predicate.
    Unsupported,

    /// UnionRead may use this predicate to prune data without false negatives.
    PruningOnly,
}

impl PredicatePushdownLevel {
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
pub struct PredicatePushdownDecision {
    predicate_id: PredicateId,
    level: PredicatePushdownLevel,
}

impl PredicatePushdownDecision {
    pub fn new(predicate_id: PredicateId, level: PredicatePushdownLevel) -> Self {
        Self {
            predicate_id,
            level,
        }
    }

    pub fn predicate_id(self) -> PredicateId {
        self.predicate_id
    }

    pub fn level(self) -> PredicatePushdownLevel {
        self.level
    }
}

/// Engine-neutral input to UnionRead planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnionReadRequest {
    table_path: TablePath,
    read_mode: UnionReadMode,
    output_projection: Option<Vec<usize>>,
    predicates: Vec<PredicateInput>,
    target_parallelism: Option<usize>,
}

impl UnionReadRequest {
    pub fn new(table_path: TablePath) -> Self {
        Self {
            table_path,
            read_mode: UnionReadMode::Union,
            output_projection: None,
            predicates: Vec::new(),
            target_parallelism: None,
        }
    }

    pub fn with_read_mode(mut self, read_mode: UnionReadMode) -> Self {
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
    pub fn with_output_projection(mut self, output_projection: Vec<usize>) -> Self {
        self.output_projection = Some(output_projection);
        self
    }

    /// Sets engine predicates translated into Fluss's pruning model.
    ///
    /// Predicate ids must be unique within the request. The engine must retain
    /// every original expression for residual evaluation.
    pub fn with_predicates(mut self, predicates: Vec<PredicateInput>) -> Self {
        self.predicates = predicates;
        self
    }

    pub fn with_target_parallelism(mut self, target_parallelism: usize) -> Self {
        self.target_parallelism = Some(target_parallelism);
        self
    }

    pub fn table_path(&self) -> &TablePath {
        &self.table_path
    }

    pub fn read_mode(&self) -> UnionReadMode {
        self.read_mode
    }

    pub fn output_projection(&self) -> Option<&[usize]> {
        self.output_projection.as_deref()
    }

    pub fn predicates(&self) -> &[PredicateInput] {
        &self.predicates
    }

    pub fn target_parallelism(&self) -> Option<usize> {
        self.target_parallelism
    }
}

/// Estimated work exposed to an upstream engine for scheduling and costing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UnionReadStatistics {
    estimated_rows: Option<u64>,
    estimated_bytes: Option<u64>,
}

impl UnionReadStatistics {
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

/// An opaque, independently executable bounded read task.
///
/// Engines may inspect the task id and statistics for scheduling, but must not
/// interpret the execution descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnionReadTask {
    task_id: String,
    descriptor_version: u32,
    execution_descriptor: Vec<u8>,
    statistics: UnionReadStatistics,
}

impl UnionReadTask {
    /// Creates an opaque task from a planner-produced execution descriptor.
    pub fn try_new(
        task_id: String,
        descriptor_version: u32,
        execution_descriptor: Vec<u8>,
        statistics: UnionReadStatistics,
    ) -> UnionReadResult<Self> {
        if descriptor_version != CURRENT_UNION_READ_TASK_VERSION {
            return Err(UnionReadError::UnsupportedTaskVersion {
                version: descriptor_version,
            });
        }
        if task_id.is_empty() {
            return Err(UnionReadError::InvalidTask(
                "task id must not be empty".to_string(),
            ));
        }
        if execution_descriptor.is_empty() {
            return Err(UnionReadError::InvalidTask(
                "execution descriptor must not be empty".to_string(),
            ));
        }
        TaskDescriptor::decode(&execution_descriptor)?;
        Ok(Self {
            task_id,
            descriptor_version,
            execution_descriptor,
            statistics,
        })
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn descriptor_version(&self) -> u32 {
        self.descriptor_version
    }

    /// Returns the opaque execution descriptor for transport to an executor.
    pub fn execution_descriptor(&self) -> &[u8] {
        &self.execution_descriptor
    }

    pub fn statistics(&self) -> UnionReadStatistics {
        self.statistics
    }

    /// Encodes this task for distribution to an execution worker.
    ///
    /// The wire envelope is versioned independently of the internal task
    /// implementation. Engines must treat the returned bytes as opaque.
    pub fn encode(&self) -> UnionReadResult<Vec<u8>> {
        let task_id = self.task_id.as_bytes();
        let task_id_len = u32::try_from(task_id.len()).map_err(|_| {
            UnionReadError::InvalidTask("task id exceeds the wire format limit".to_string())
        })?;
        let descriptor_len = u32::try_from(self.execution_descriptor.len()).map_err(|_| {
            UnionReadError::InvalidTask(
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

        let capacity = UNION_READ_TASK_HEADER_SIZE
            .checked_add(task_id.len())
            .and_then(|size| size.checked_add(self.execution_descriptor.len()))
            .ok_or_else(|| {
                UnionReadError::InvalidTask("encoded task size overflows usize".to_string())
            })?;
        let mut encoded = Vec::with_capacity(capacity);
        encoded.extend_from_slice(&UNION_READ_TASK_MAGIC);
        encoded.extend_from_slice(&self.descriptor_version.to_le_bytes());
        encoded.extend_from_slice(&task_id_len.to_le_bytes());
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
        encoded.extend_from_slice(task_id);
        encoded.extend_from_slice(&self.execution_descriptor);
        Ok(encoded)
    }

    /// Decodes an opaque task produced by [`UnionReadTask::encode`].
    pub fn decode(encoded: &[u8]) -> UnionReadResult<Self> {
        if encoded.len() < UNION_READ_TASK_HEADER_SIZE {
            return Err(UnionReadError::InvalidTask(format!(
                "task envelope is truncated: expected at least {UNION_READ_TASK_HEADER_SIZE} bytes, got {}",
                encoded.len()
            )));
        }
        if encoded[..UNION_READ_TASK_MAGIC.len()] != UNION_READ_TASK_MAGIC {
            return Err(UnionReadError::InvalidTask(
                "task envelope has an invalid magic header".to_string(),
            ));
        }

        let descriptor_version = read_u32(encoded, 4)?;
        if descriptor_version != CURRENT_UNION_READ_TASK_VERSION {
            return Err(UnionReadError::UnsupportedTaskVersion {
                version: descriptor_version,
            });
        }

        let task_id_len = read_u32(encoded, 8)? as usize;
        let descriptor_len = read_u32(encoded, 12)? as usize;
        let statistics_flags = encoded[16];
        let known_statistics_flags = STATISTICS_ROWS_PRESENT | STATISTICS_BYTES_PRESENT;
        if statistics_flags & !known_statistics_flags != 0 {
            return Err(UnionReadError::InvalidTask(format!(
                "task envelope contains unknown statistics flags 0x{statistics_flags:02x}"
            )));
        }

        let estimated_rows = (statistics_flags & STATISTICS_ROWS_PRESENT != 0)
            .then(|| read_u64(encoded, 17))
            .transpose()?;
        let estimated_bytes = (statistics_flags & STATISTICS_BYTES_PRESENT != 0)
            .then(|| read_u64(encoded, 25))
            .transpose()?;
        let expected_len = UNION_READ_TASK_HEADER_SIZE
            .checked_add(task_id_len)
            .and_then(|size| size.checked_add(descriptor_len))
            .ok_or_else(|| {
                UnionReadError::InvalidTask("decoded task size overflows usize".to_string())
            })?;
        if encoded.len() != expected_len {
            return Err(UnionReadError::InvalidTask(format!(
                "task envelope length mismatch: expected {expected_len} bytes, got {}",
                encoded.len()
            )));
        }

        let task_id_end = UNION_READ_TASK_HEADER_SIZE + task_id_len;
        let task_id = std::str::from_utf8(&encoded[UNION_READ_TASK_HEADER_SIZE..task_id_end])
            .map_err(|error| {
                UnionReadError::InvalidTask(format!("task id is not valid UTF-8: {error}"))
            })?
            .to_string();
        let execution_descriptor = encoded[task_id_end..expected_len].to_vec();
        Self::try_new(
            task_id,
            descriptor_version,
            execution_descriptor,
            UnionReadStatistics::new(estimated_rows, estimated_bytes),
        )
    }
}

fn read_u32(encoded: &[u8], offset: usize) -> UnionReadResult<u32> {
    let bytes = encoded
        .get(offset..offset + size_of::<u32>())
        .ok_or_else(|| UnionReadError::InvalidTask("task envelope is truncated".to_string()))?;
    Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| {
        UnionReadError::InvalidTask("invalid u32 in task envelope".to_string())
    })?))
}

fn read_u64(encoded: &[u8], offset: usize) -> UnionReadResult<u64> {
    let bytes = encoded
        .get(offset..offset + size_of::<u64>())
        .ok_or_else(|| UnionReadError::InvalidTask("task envelope is truncated".to_string()))?;
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| {
        UnionReadError::InvalidTask("invalid u64 in task envelope".to_string())
    })?))
}

/// A frozen plan whose tasks can be distributed and executed independently.
#[derive(Debug, Clone)]
pub struct UnionReadPlan {
    output_schema: SchemaRef,
    tasks: Vec<UnionReadTask>,
    statistics: UnionReadStatistics,
    predicate_pushdown_decisions: Vec<PredicatePushdownDecision>,
}

impl UnionReadPlan {
    /// Creates a frozen plan from planner-produced tasks.
    pub fn new(
        output_schema: SchemaRef,
        tasks: Vec<UnionReadTask>,
        statistics: UnionReadStatistics,
        predicate_pushdown_decisions: Vec<PredicatePushdownDecision>,
    ) -> Self {
        Self {
            output_schema,
            tasks,
            statistics,
            predicate_pushdown_decisions,
        }
    }

    pub fn output_schema(&self) -> &SchemaRef {
        &self.output_schema
    }

    pub fn tasks(&self) -> &[UnionReadTask] {
        &self.tasks
    }

    pub fn into_tasks(self) -> Vec<UnionReadTask> {
        self.tasks
    }

    pub fn statistics(&self) -> UnionReadStatistics {
        self.statistics
    }

    /// Returns conservative pushdown decisions correlated to request ids.
    ///
    /// These decisions never replace the engine's original residual filters.
    pub fn predicate_pushdown_decisions(&self) -> &[PredicatePushdownDecision] {
        &self.predicate_pushdown_decisions
    }
}

/// Runtime-only resources supplied while executing a frozen task.
///
/// Cancellation and metrics hooks will be added here as execution backends
/// are introduced. These resources intentionally do not belong to the
/// serializable task descriptor: tasks are cached, logged and persisted by
/// engines, so anything secret or environment-bound must arrive through this
/// context instead.
#[derive(Clone, Default)]
pub struct UnionReadExecutionContext {
    fluss_connection: Option<Arc<FlussConnection>>,
    lake_credentials: HashMap<String, String>,
    memory_limit_bytes: Option<usize>,
    idle_timeout: Option<Duration>,
}

impl UnionReadExecutionContext {
    pub fn with_fluss_connection(mut self, fluss_connection: Arc<FlussConnection>) -> Self {
        self.fluss_connection = Some(fluss_connection);
        self
    }

    /// Sets the secret lake catalog options withheld from task descriptors.
    ///
    /// Keys use the same names as the lake catalog options (for Paimon, the
    /// `table.datalake.paimon.` property suffixes such as `s3.secret-key`).
    /// At execution time these values override any equally-named option
    /// carried by the task, so credentials rotated after planning take
    /// effect without re-planning.
    pub fn with_lake_credentials(mut self, lake_credentials: HashMap<String, String>) -> Self {
        self.lake_credentials = lake_credentials;
        self
    }

    pub fn with_memory_limit_bytes(mut self, memory_limit_bytes: usize) -> Self {
        self.memory_limit_bytes = Some(memory_limit_bytes);
        self
    }

    /// Overrides [`DEFAULT_UNION_READ_IDLE_TIMEOUT`] for this execution.
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
        self.idle_timeout.unwrap_or(DEFAULT_UNION_READ_IDLE_TIMEOUT)
    }
}

impl Debug for UnionReadExecutionContext {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        // Credential keys and values must never reach logs; only the count
        // is safe to expose.
        formatter
            .debug_struct("UnionReadExecutionContext")
            .field("has_fluss_connection", &self.fluss_connection.is_some())
            .field("lake_credential_count", &self.lake_credentials.len())
            .field("memory_limit_bytes", &self.memory_limit_bytes)
            .field("idle_timeout", &self.idle_timeout())
            .finish()
    }
}

/// Plans engine-neutral, independently executable bounded read tasks.
///
/// Planning resolves mutable table state into immutable task descriptions.
/// Engines own scan construction and scheduling, but do not interpret task
/// descriptors or implement lake/log stitch semantics.
pub trait UnionReadPlanner: Send + Sync {
    fn plan(&self, request: UnionReadRequest) -> UnionReadPlanFuture<'_>;
}

/// Executes one immutable UnionRead task as a finite Arrow batch stream.
///
/// `execute` returns synchronously with a lazy stream: structural problems
/// (undecodable descriptors, unknown kinds, missing required context) fail
/// fast in the call itself, while environment work — opening connections,
/// files and subscriptions — happens on first poll, so environment failures
/// surface as the first stream item. This adapts directly to synchronous
/// engine interfaces such as DataFusion's `ExecutionPlan::execute`.
///
/// The returned stream is bounded even though it is consumed asynchronously,
/// and it has exactly two exits: reaching the frozen task boundary, or a
/// typed error. A read that stops making progress for longer than the
/// context's idle timeout fails instead of waiting forever, and never
/// returns a silent partial result.
///
/// Execution may use runtime resources from the context, but task semantics
/// must come entirely from the frozen task descriptor.
pub trait UnionReadExecutor: Send + Sync {
    fn execute(
        &self,
        task: UnionReadTask,
        context: UnionReadExecutionContext,
    ) -> UnionReadResult<SendableRecordBatchStream>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{AppendLogTaskDescriptor, TaskDescriptor};
    use arrow::datatypes::Schema;
    use fluss::metadata::DataTypes;
    use fluss::predicate::{ComparisonOperator, FieldRef, PruningPredicate};
    use futures::{StreamExt, stream};

    fn testing_execution_descriptor() -> Vec<u8> {
        TaskDescriptor::AppendLog(
            AppendLogTaskDescriptor::try_new(
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
        let request = UnionReadRequest::new(TablePath::new("fluss", "orders"))
            .with_read_mode(UnionReadMode::LakeOnly)
            .with_output_projection(vec![2, 4])
            .with_predicates(vec![PredicateInput::new(
                PredicateId::new(7),
                PruningPredicate::comparison(
                    ComparisonOperator::GreaterThan,
                    FieldRef::new(2, "amount", DataTypes::bigint()),
                    100_i64,
                ),
            )])
            .with_target_parallelism(8);

        assert_eq!(request.table_path(), &TablePath::new("fluss", "orders"));
        assert_eq!(request.read_mode(), UnionReadMode::LakeOnly);
        assert_eq!(request.output_projection(), Some([2, 4].as_slice()));
        assert_eq!(request.predicates()[0].id(), PredicateId::new(7));
        assert_eq!(request.target_parallelism(), Some(8));
    }

    #[test]
    fn task_descriptor_is_opaque_to_consumers() {
        let statistics = UnionReadStatistics::new(Some(10), Some(100));
        let descriptor = testing_execution_descriptor();
        let task =
            UnionReadTask::try_new("bucket-0".to_string(), 1, descriptor.clone(), statistics)
                .unwrap();
        let plan = UnionReadPlan::new(
            Arc::new(Schema::empty()),
            vec![task.clone()],
            statistics,
            Vec::new(),
        );

        assert_eq!(task.task_id(), "bucket-0");
        assert_eq!(task.descriptor_version(), 1);
        assert_eq!(task.execution_descriptor(), descriptor);
        assert_eq!(plan.tasks(), &[task]);
        assert_eq!(plan.statistics(), statistics);
    }

    #[test]
    fn pushdown_decisions_never_remove_engine_residuals() {
        let decisions = vec![
            PredicatePushdownDecision::new(
                PredicateId::new(1),
                PredicatePushdownLevel::PruningOnly,
            ),
            PredicatePushdownDecision::new(
                PredicateId::new(2),
                PredicatePushdownLevel::Unsupported,
            ),
        ];
        let plan = UnionReadPlan::new(
            Arc::new(Schema::empty()),
            Vec::new(),
            UnionReadStatistics::default(),
            decisions.clone(),
        );

        assert_eq!(plan.predicate_pushdown_decisions(), decisions);
        assert!(PredicatePushdownLevel::PruningOnly.requires_residual_evaluation());
        assert!(PredicatePushdownLevel::Unsupported.requires_residual_evaluation());
        assert!(PredicatePushdownLevel::PruningOnly.can_prune());
        assert!(!PredicatePushdownLevel::Unsupported.can_prune());
    }

    #[test]
    fn rejects_empty_task_identity_and_descriptor() {
        assert!(matches!(
            UnionReadTask::try_new(String::new(), 1, vec![1], UnionReadStatistics::default()),
            Err(UnionReadError::InvalidTask(_))
        ));
        assert!(matches!(
            UnionReadTask::try_new(
                "task".to_string(),
                1,
                Vec::new(),
                UnionReadStatistics::default()
            ),
            Err(UnionReadError::InvalidTask(_))
        ));
    }

    #[test]
    fn task_encoding_round_trips_opaque_descriptor() {
        let task = UnionReadTask::try_new(
            "partition=dt%3D2026-07-28/bucket=3".to_string(),
            CURRENT_UNION_READ_TASK_VERSION,
            testing_execution_descriptor(),
            UnionReadStatistics::new(Some(42), None),
        )
        .unwrap();

        let decoded = UnionReadTask::decode(&task.encode().unwrap()).unwrap();

        assert_eq!(decoded, task);
    }

    #[test]
    fn rejects_unknown_task_version() {
        let task = UnionReadTask::try_new(
            "bucket-0".to_string(),
            CURRENT_UNION_READ_TASK_VERSION,
            testing_execution_descriptor(),
            UnionReadStatistics::default(),
        )
        .unwrap();
        let mut encoded = task.encode().unwrap();
        encoded[4..8].copy_from_slice(&(CURRENT_UNION_READ_TASK_VERSION + 1).to_le_bytes());

        assert!(matches!(
            UnionReadTask::decode(&encoded),
            Err(UnionReadError::UnsupportedTaskVersion { version: 2 })
        ));
    }

    #[test]
    fn rejects_malformed_task_envelopes() {
        let task = UnionReadTask::try_new(
            "bucket-0".to_string(),
            CURRENT_UNION_READ_TASK_VERSION,
            testing_execution_descriptor(),
            UnionReadStatistics::new(None, Some(20)),
        )
        .unwrap();
        let encoded = task.encode().unwrap();

        assert!(matches!(
            UnionReadTask::decode(&encoded[..encoded.len() - 1]),
            Err(UnionReadError::InvalidTask(_))
        ));

        let mut invalid_magic = encoded.clone();
        invalid_magic[0] = b'X';
        assert!(matches!(
            UnionReadTask::decode(&invalid_magic),
            Err(UnionReadError::InvalidTask(_))
        ));

        let mut unknown_flags = encoded;
        unknown_flags[16] = 1 << 7;
        assert!(matches!(
            UnionReadTask::decode(&unknown_flags),
            Err(UnionReadError::InvalidTask(_))
        ));
    }

    #[test]
    fn planner_and_executor_are_object_safe_engine_boundaries() {
        struct TestingUnionRead;

        impl UnionReadPlanner for TestingUnionRead {
            fn plan(&self, _request: UnionReadRequest) -> UnionReadPlanFuture<'_> {
                Box::pin(async {
                    let task = UnionReadTask::try_new(
                        "bucket-0".to_string(),
                        CURRENT_UNION_READ_TASK_VERSION,
                        testing_execution_descriptor(),
                        UnionReadStatistics::default(),
                    )?;
                    Ok(UnionReadPlan::new(
                        Arc::new(Schema::empty()),
                        vec![task],
                        UnionReadStatistics::default(),
                        Vec::new(),
                    ))
                })
            }
        }

        impl UnionReadExecutor for TestingUnionRead {
            fn execute(
                &self,
                _task: UnionReadTask,
                _context: UnionReadExecutionContext,
            ) -> UnionReadResult<SendableRecordBatchStream> {
                let batches =
                    stream::iter(vec![Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))]);
                Ok(Box::pin(batches) as SendableRecordBatchStream)
            }
        }

        let service = TestingUnionRead;
        let planner: &dyn UnionReadPlanner = &service;
        let executor: &dyn UnionReadExecutor = &service;
        let plan = futures::executor::block_on(
            planner.plan(UnionReadRequest::new(TablePath::new("fluss", "orders"))),
        )
        .unwrap();
        let mut batches = executor
            .execute(
                plan.tasks()[0].clone(),
                UnionReadExecutionContext::default(),
            )
            .unwrap();

        assert!(futures::executor::block_on(batches.next()).unwrap().is_ok());
        assert!(futures::executor::block_on(batches.next()).is_none());
    }

    #[test]
    fn execution_context_debug_does_not_expose_credentials() {
        let mut credentials = HashMap::new();
        credentials.insert("s3.secret-key".to_string(), "TOP-SECRET".to_string());
        let context = UnionReadExecutionContext::default()
            .with_lake_credentials(credentials)
            .with_memory_limit_bytes(1024);

        let debug = format!("{context:?}");

        assert_eq!(
            debug,
            "UnionReadExecutionContext { has_fluss_connection: false, lake_credential_count: 1, memory_limit_bytes: Some(1024), idle_timeout: 60s }"
        );
        assert!(!debug.contains("TOP-SECRET"));
        assert!(!debug.contains("secret-key"));
    }
}
