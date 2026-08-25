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

//! HTTP-independent contracts consumed by the REST write pipeline.

use crate::error::GatewayError;
use fluss::metadata::{TableInfo, TablePath};
use fluss::record::ChangeType;
use fluss::row::GenericRow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

pub type GatewayResult<T> = Result<T, GatewayError>;
pub type TableRef = TablePath;
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = GatewayResult<T>> + Send + 'a>>;

/// Request metadata passed through protocol and backend boundaries.
#[derive(Debug, Clone)]
pub struct RequestContext {
    request_id: Arc<str>,
    deadline: Instant,
}

impl RequestContext {
    pub fn new(request_id: impl Into<Arc<str>>, deadline: Instant) -> Self {
        Self {
            request_id: request_id.into(),
            deadline,
        }
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn ensure_active(&self) -> GatewayResult<()> {
        if Instant::now() >= self.deadline {
            Err(GatewayError::deadline_exceeded("request deadline exceeded"))
        } else {
            Ok(())
        }
    }
}

/// A fully decoded write request in input order.
#[derive(Debug)]
pub struct WriteRequest {
    table: TableRef,
    rows: Vec<GenericRow<'static>>,
    change_types: Vec<ChangeType>,
    partial_update_columns: Option<Vec<String>>,
}

impl WriteRequest {
    pub fn new(
        table: TableRef,
        rows: Vec<GenericRow<'static>>,
        change_types: Vec<ChangeType>,
        partial_update_columns: Option<Vec<String>>,
    ) -> GatewayResult<Self> {
        if rows.is_empty() {
            return Err(GatewayError::invalid_argument(
                "write request must contain at least one entry",
            ));
        }
        if rows.len() != change_types.len() {
            return Err(GatewayError::internal(format!(
                "write request has {} rows but {} change types",
                rows.len(),
                change_types.len()
            )));
        }
        if let Some(columns) = &partial_update_columns {
            if columns.is_empty() {
                return Err(GatewayError::invalid_argument(
                    "partial_update_columns must not be empty",
                ));
            }
            let mut sorted = columns.clone();
            sorted.sort_unstable();
            if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(GatewayError::invalid_argument(
                    "partial_update_columns must not contain duplicates",
                ));
            }
            if change_types
                .iter()
                .any(|change_type| *change_type != ChangeType::UpdateAfter)
            {
                return Err(GatewayError::invalid_argument(
                    "partial_update_columns can be used only with upsert entries",
                ));
            }
        }
        Ok(Self {
            table,
            rows,
            change_types,
            partial_update_columns,
        })
    }

    pub fn table(&self) -> &TableRef {
        &self.table
    }

    pub fn rows(&self) -> &[GenericRow<'static>] {
        &self.rows
    }

    pub fn change_types(&self) -> &[ChangeType] {
        &self.change_types
    }

    pub fn partial_update_columns(&self) -> Option<&[String]> {
        self.partial_update_columns.as_deref()
    }
}

/// Result of one ordered write batch.
#[derive(Debug)]
pub struct WriteResult {
    pub row_count: u64,
    pub failures: Vec<RowWriteError>,
}

impl WriteResult {
    pub fn success_count(&self) -> u64 {
        self.row_count
            .saturating_sub(u64::try_from(self.failures.len()).unwrap_or(u64::MAX))
    }

    pub fn error_count(&self) -> u64 {
        u64::try_from(self.failures.len()).unwrap_or(u64::MAX)
    }
}

/// Failure for one input row.
#[derive(Debug)]
pub struct RowWriteError {
    pub index: usize,
    pub error: GatewayError,
}

/// Backend operation required by the write REST pipeline.
pub trait FlussBackend: Send + Sync {
    fn write<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: WriteRequest,
    ) -> BackendFuture<'a, WriteResult>;
}

/// Authoritative source for the latest table schema.
pub trait TableInfoProvider: Send + Sync {
    fn latest_table_info<'a>(
        &'a self,
        ctx: &'a RequestContext,
        table: &'a TableRef,
    ) -> BackendFuture<'a, TableInfo>;
}
