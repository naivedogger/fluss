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

//! Public FIP-48 error surface.

use thiserror::Error;

/// Result type returned by UnionRead planning and execution APIs.
pub type Result<T> = std::result::Result<T, FlussLakeError>;

/// Errors surfaced by the UnionRead planning and execution contract.
#[derive(Debug, Error)]
pub enum FlussLakeError {
    /// Requested table is not configured for lake reads.
    #[error("table is not lake-readable: {0}")]
    NotLakeReadable(String),

    /// Planning failed for a reason other than invalid input.
    #[error("UnionRead planning failed: {0}")]
    PlanningFailed(String),

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

    /// Split descriptor version is newer than this reader supports.
    #[error("incompatible UnionRead split version: {0}")]
    IncompatibleSplitVersion(String),

    /// Internal error that should not escape to engines.
    #[error("UnionRead internal error: {0}")]
    Internal(String),
}
