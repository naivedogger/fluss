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

//! Internal Fluss access seam.
//!
//! [`FlussSource`] is the single trait the rest of the crate depends on for all
//! Fluss access (metadata discovery, KV point lookup, KV bounded scan, log
//! snapshot scan). It is a `pub(crate)` test seam.

use std::sync::Arc;

use arrow::array::RecordBatch;

use crate::error::Result;

pub(crate) mod fluss_client;

/// Identifies a Fluss table by `database.table`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub database: String,
    pub table: String,
}

impl TableRef {
    pub fn new(database: impl Into<String>, table: impl Into<String>) -> Self {
        Self {
            database: database.into(),
            table: table.into(),
        }
    }
}

impl std::fmt::Display for TableRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.database, self.table)
    }
}

impl From<&TableRef> for fluss::metadata::TablePath {
    fn from(value: &TableRef) -> Self {
        fluss::metadata::TablePath::new(value.database.clone(), value.table.clone())
    }
}

/// Minimal table metadata the crate needs.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FlussTableMeta {
    pub table_ref: TableRef,
    pub table_id: i64,
    pub schema_id: i32,
    pub schema: fluss::metadata::Schema,
    pub primary_keys: Vec<String>,
    pub bucket_keys: Vec<String>,
    pub num_buckets: i32,
    pub partition_keys: Vec<String>,
}

impl FlussTableMeta {
    pub fn has_primary_key(&self) -> bool {
        !self.primary_keys.is_empty()
    }
}

/// One partition of a partitioned Fluss table.
#[derive(Debug, Clone)]
pub struct FlussPartition {
    pub partition_id: i64,
    pub values: Vec<(String, String)>,
}

/// One scalar key field for a full-primary-key equality lookup.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyValue {
    Boolean(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    String(String),
}

/// A complete primary-key lookup key: one [`KeyValue`] per primary-key column.
pub type LookupKey = Vec<KeyValue>;

/// The single internal seam for all Fluss access.
#[async_trait::async_trait]
pub trait FlussSource: Send + Sync {
    async fn list_databases(&self) -> Result<Vec<String>>;
    async fn list_tables(&self, database: &str) -> Result<Vec<String>>;
    async fn get_table_meta(&self, table: &TableRef) -> Result<FlussTableMeta>;
    async fn lookup(&self, table: &TableRef, key: &LookupKey) -> Result<RecordBatch>;
    async fn prefix_lookup(
        &self,
        table: &TableRef,
        lookup_columns: &[String],
        key: &LookupKey,
    ) -> Result<RecordBatch>;
    async fn list_partitions(&self, table: &TableRef) -> Result<Vec<FlussPartition>>;
    async fn bounded_scan(
        &self,
        table: &TableRef,
        partition_id: Option<i64>,
        bucket: i32,
        projection: Option<&[usize]>,
        limit: usize,
    ) -> Result<Vec<RecordBatch>>;
    async fn log_scan(
        &self,
        table: &TableRef,
        partition_id: Option<i64>,
        bucket: i32,
        projection: Option<&[usize]>,
        row_limit: Option<usize>,
    ) -> Result<Vec<RecordBatch>>;
    async fn kv_full_scan(
        &self,
        table: &TableRef,
        partition_id: Option<i64>,
        bucket: i32,
        projection: Option<&[usize]>,
    ) -> Result<Vec<RecordBatch>>;
}

pub(crate) type SharedFlussSource = Arc<dyn FlussSource>;
