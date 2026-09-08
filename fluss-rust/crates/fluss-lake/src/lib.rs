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
//! Union read is a bounded batch query. This stage freezes source contexts
//! and plans logical read splits without executing them.
//!
//! Prepared contexts can be consumed by engines or the default split planner.

#![doc = include_str!("../README.md")]

mod bucket_pruning;
mod error;
#[cfg(feature = "paimon")]
mod paimon;
mod partition;
mod plan;
mod planner;
mod planning;
mod pruning;
mod read_context;
mod split;
mod split_descriptor;
mod table;

pub use error::{FlussLakeError, Result};
pub use plan::{FlussLakePlanStatistics, FlussLakeReadPlan};
pub use read_context::{FlussLakeLogRange, FlussLakeReadContext};
pub(crate) use split::CURRENT_FLUSS_LAKE_SPLIT_VERSION;
pub use split::{FlussLakePartitionIdentity, FlussLakeReadSplit};
pub use table::{FlussLakeScan, FlussLakeTable};
