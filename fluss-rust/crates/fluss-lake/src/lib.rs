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

//! Engine-neutral bounded lake and log read kernel for Apache Fluss.
//!
//! Union read is a bounded batch query. Results are delivered lazily as a
//! finite stream of Arrow record batches.

mod executor;
#[cfg(feature = "paimon")]
mod paimon;
mod pk_overlay;
mod planner;
#[doc(hidden)]
pub mod planning;
mod pruning;
mod task;
mod union_read;

pub use executor::FlussUnionReadExecutor;
pub use planner::FlussUnionReadPlanner;
pub use union_read::{
    CURRENT_UNION_READ_TASK_VERSION, DEFAULT_UNION_READ_IDLE_TIMEOUT, PredicateId, PredicateInput,
    PredicatePushdownDecision, PredicatePushdownLevel, SendableRecordBatchStream, UnionReadError,
    UnionReadExecutionContext, UnionReadExecutor, UnionReadMode, UnionReadPlan,
    UnionReadPlanFuture, UnionReadPlanner, UnionReadRequest, UnionReadResult, UnionReadStatistics,
    UnionReadTask,
};
