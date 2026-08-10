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

//! Per-read execution context.

use fluss::client::FlussConnection;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

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

    pub fn fluss_connection(&self) -> Option<&Arc<FlussConnection>> {
        self.fluss_connection.as_ref()
    }

    pub fn lake_credentials(&self) -> &HashMap<String, String> {
        &self.lake_credentials
    }

    pub fn memory_limit_bytes(&self) -> Option<usize> {
        self.memory_limit_bytes
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
            .finish()
    }
}
