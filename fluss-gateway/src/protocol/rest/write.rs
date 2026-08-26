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

//! Borrowed REST write input, schema-decoder caching, one-refresh retry, and backend dispatch.

use crate::backend::{
    FlussBackend, GatewayResult, RequestContext, TableInfoProvider, TableRef, WriteRequest,
    WriteResult,
};
use crate::error::GatewayError;
use crate::protocol::rest::codec::{RowDecodeError, RowShape, SchemaDecoder};
use crate::protocol::rest::parse_json_body;
use axum::body::Bytes;
use axum::http::HeaderMap;
use fluss::metadata::TableInfo;
use fluss::record::ChangeType;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// One strict write body borrowing every row object from the HTTP request bytes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteBody<'a> {
    #[serde(default)]
    pub(crate) partial_update_columns: Option<Vec<String>>,
    #[serde(borrow)]
    pub(crate) entries: Vec<WriteBodyEntry<'a>>,
}

/// One caller-correlated mutation. Exactly one operation must be present.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteBodyEntry<'a> {
    pub(crate) id: String,
    #[serde(borrow)]
    pub(crate) append: Option<&'a RawValue>,
    #[serde(borrow)]
    pub(crate) upsert: Option<&'a RawValue>,
    #[serde(borrow)]
    pub(crate) delete: Option<&'a RawValue>,
}

#[derive(Debug)]
struct CachedSchemaDecoder {
    table_id: i64,
    schema_id: i32,
    primary_keys: Arc<[String]>,
    decoder: SchemaDecoder,
}

impl CachedSchemaDecoder {
    fn version(&self) -> (i64, i32) {
        (self.table_id, self.schema_id)
    }
}

/// Latest schema decoder per table. Clones share the same process cache.
#[derive(Clone, Default)]
pub(crate) struct SchemaDecoderCache {
    entries: Arc<RwLock<HashMap<TableRef, Arc<CachedSchemaDecoder>>>>,
}

impl SchemaDecoderCache {
    fn current(&self, table: &TableRef) -> Option<Arc<CachedSchemaDecoder>> {
        self.entries
            .read()
            .expect("schema decoder cache lock poisoned")
            .get(table)
            .cloned()
    }

    fn install(
        &self,
        table: &TableRef,
        table_info: &TableInfo,
    ) -> GatewayResult<Arc<CachedSchemaDecoder>> {
        if table_info.get_table_path() != table {
            return Err(GatewayError::internal(format!(
                "loaded schema for table `{}` while decoding `{table}`",
                table_info.get_table_path()
            )));
        }
        if let Some(existing) = self.reusable(table, table_info)? {
            return Ok(existing);
        }

        let candidate = Arc::new(CachedSchemaDecoder {
            table_id: table_info.get_table_id(),
            schema_id: table_info.get_schema_id(),
            primary_keys: Arc::from(table_info.get_primary_keys().clone()),
            decoder: SchemaDecoder::new(table_info.get_row_type().clone())?,
        });
        let mut entries = self
            .entries
            .write()
            .expect("schema decoder cache lock poisoned");
        if let Some(existing) = entries.get(table)
            && let Some(reusable) = reusable_decoder(existing, table_info)?
        {
            return Ok(reusable);
        }
        entries.insert(table.clone(), Arc::clone(&candidate));
        Ok(candidate)
    }

    fn reusable(
        &self,
        table: &TableRef,
        table_info: &TableInfo,
    ) -> GatewayResult<Option<Arc<CachedSchemaDecoder>>> {
        let entries = self
            .entries
            .read()
            .expect("schema decoder cache lock poisoned");
        entries
            .get(table)
            .map(|existing| reusable_decoder(existing, table_info))
            .transpose()
            .map(Option::flatten)
    }
}

fn reusable_decoder(
    existing: &Arc<CachedSchemaDecoder>,
    table_info: &TableInfo,
) -> GatewayResult<Option<Arc<CachedSchemaDecoder>>> {
    if existing.table_id == table_info.get_table_id()
        && existing.schema_id == table_info.get_schema_id()
    {
        return Ok(Some(Arc::clone(existing)));
    }
    if existing.table_id == table_info.get_table_id()
        && existing.schema_id > table_info.get_schema_id()
    {
        return Ok(Some(Arc::clone(existing)));
    }
    Ok(None)
}

/// Complete write path shared by an HTTP handler and integration tests.
#[allow(dead_code)]
pub(crate) struct WriteService {
    backend: Arc<dyn FlussBackend>,
    table_info: Arc<dyn TableInfoProvider>,
    decoders: SchemaDecoderCache,
}

#[allow(dead_code)]
impl WriteService {
    pub(crate) fn new(
        backend: Arc<dyn FlussBackend>,
        table_info: Arc<dyn TableInfoProvider>,
    ) -> Self {
        Self {
            backend,
            table_info,
            decoders: SchemaDecoderCache::default(),
        }
    }

    /// Parses the HTTP body without copying row JSON, decodes the complete batch, and dispatches it.
    pub(crate) async fn write_json(
        &self,
        ctx: &RequestContext,
        table: TableRef,
        headers: &HeaderMap,
        body: &Bytes,
    ) -> GatewayResult<WriteResult> {
        let write_body: WriteBody<'_> = parse_json_body(headers, body)?;
        self.write(ctx, table, &write_body).await
    }

    async fn write(
        &self,
        ctx: &RequestContext,
        table: TableRef,
        body: &WriteBody<'_>,
    ) -> GatewayResult<WriteResult> {
        ctx.ensure_active()?;
        let (attempted, came_from_cache) = match self.decoders.current(&table) {
            Some(decoder) => (decoder, true),
            None => {
                let table_info = self.table_info.latest_table_info(ctx, &table).await?;
                (self.decoders.install(&table, &table_info)?, false)
            }
        };

        let request = match decode_write_request(&table, body, &attempted) {
            Ok(request) => request,
            Err(error) if error.is_schema_mismatch() && came_from_cache => {
                let refreshed = match self.decoders.current(&table) {
                    Some(current) if current.version() != attempted.version() => current,
                    _ => {
                        let table_info = self.table_info.latest_table_info(ctx, &table).await?;
                        self.decoders.install(&table, &table_info)?
                    }
                };
                if refreshed.version() == attempted.version() {
                    return Err(error.into_gateway_error());
                }
                decode_write_request(&table, body, &refreshed)
                    .map_err(WriteDecodeError::into_gateway_error)?
            }
            Err(error) => return Err(error.into_gateway_error()),
        };

        ctx.ensure_active()?;
        self.backend.write(ctx, request).await
    }
}

#[derive(Debug)]
struct WriteDecodeError {
    schema_mismatch: bool,
    error: GatewayError,
}

impl WriteDecodeError {
    fn invalid(error: GatewayError) -> Self {
        Self {
            schema_mismatch: false,
            error,
        }
    }

    fn schema_mismatch(error: GatewayError) -> Self {
        Self {
            schema_mismatch: true,
            error,
        }
    }

    fn is_schema_mismatch(&self) -> bool {
        self.schema_mismatch
    }

    fn into_gateway_error(self) -> GatewayError {
        self.error
    }
}

impl From<RowDecodeError> for WriteDecodeError {
    fn from(error: RowDecodeError) -> Self {
        Self {
            schema_mismatch: error.is_schema_mismatch(),
            error: error.into_gateway_error(),
        }
    }
}

fn decode_write_request(
    table: &TableRef,
    body: &WriteBody<'_>,
    schema: &CachedSchemaDecoder,
) -> Result<WriteRequest, WriteDecodeError> {
    if body.entries.is_empty() {
        return Err(WriteDecodeError::invalid(GatewayError::invalid_argument(
            "write request must contain at least one entry",
        )));
    }
    validate_partial_update_columns(body, schema)?;

    let mut ids = HashSet::with_capacity(body.entries.len());
    let mut rows = Vec::with_capacity(body.entries.len());
    let mut change_types = Vec::with_capacity(body.entries.len());
    for entry in &body.entries {
        if !ids.insert(entry.id.as_str()) {
            return Err(WriteDecodeError::invalid(GatewayError::invalid_argument(
                format!("duplicate write entry ID `{}`", entry.id),
            )));
        }
        let operation_count = usize::from(entry.append.is_some())
            + usize::from(entry.upsert.is_some())
            + usize::from(entry.delete.is_some());
        if operation_count != 1 {
            return Err(WriteDecodeError::invalid(GatewayError::invalid_argument(
                format!(
                    "entry `{}` must contain exactly one of append, upsert, or delete",
                    entry.id
                ),
            )));
        }

        let label = format!("entry `{}`", entry.id);
        let (row, change_type) = if let Some(value) = entry.append {
            if body.partial_update_columns.is_some() {
                return Err(WriteDecodeError::invalid(GatewayError::invalid_argument(
                    "partial_update_columns can be used only with upsert entries",
                )));
            }
            (
                schema
                    .decoder
                    .decode_row(&label, value.get().as_bytes(), RowShape::Complete)?,
                ChangeType::AppendOnly,
            )
        } else if let Some(value) = entry.upsert {
            let shape = if body.partial_update_columns.is_some() {
                RowShape::Sparse(&schema.primary_keys)
            } else {
                RowShape::Complete
            };
            (
                schema
                    .decoder
                    .decode_row(&label, value.get().as_bytes(), shape)?,
                ChangeType::UpdateAfter,
            )
        } else {
            if body.partial_update_columns.is_some() {
                return Err(WriteDecodeError::invalid(GatewayError::invalid_argument(
                    "partial_update_columns can be used only with upsert entries",
                )));
            }
            let value = entry.delete.expect("one operation is present");
            (
                schema.decoder.decode_row(
                    &label,
                    value.get().as_bytes(),
                    RowShape::Sparse(&schema.primary_keys),
                )?,
                ChangeType::Delete,
            )
        };
        rows.push(row);
        change_types.push(change_type);
    }

    WriteRequest::new(
        table.clone(),
        rows,
        change_types,
        body.partial_update_columns.clone(),
    )
    .map_err(WriteDecodeError::invalid)
}

fn validate_partial_update_columns(
    body: &WriteBody<'_>,
    schema: &CachedSchemaDecoder,
) -> Result<(), WriteDecodeError> {
    let Some(columns) = &body.partial_update_columns else {
        return Ok(());
    };
    if columns.is_empty() {
        return Err(WriteDecodeError::invalid(GatewayError::invalid_argument(
            "partial_update_columns must not be empty",
        )));
    }
    let known: HashSet<&str> = schema
        .decoder
        .row_type()
        .fields()
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    let mut selected = HashSet::with_capacity(columns.len());
    for column in columns {
        if !selected.insert(column.as_str()) {
            return Err(WriteDecodeError::invalid(GatewayError::invalid_argument(
                format!("duplicate partial-update column `{column}`"),
            )));
        }
        if !known.contains(column.as_str()) {
            return Err(WriteDecodeError::schema_mismatch(
                GatewayError::invalid_argument(format!(
                    "partial-update column `{column}` is not part of the table schema"
                )),
            ));
        }
    }
    for primary_key in schema.primary_keys.iter() {
        if !selected.contains(primary_key.as_str()) {
            return Err(WriteDecodeError::schema_mismatch(
                GatewayError::invalid_argument(format!(
                    "partial_update_columns must include primary-key column `{primary_key}`"
                )),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendFuture;
    use axum::http::{HeaderValue, header};
    use fluss::metadata::{DataTypes, Schema, TableDescriptor};
    use fluss::row::Datum;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    struct TestBackend {
        latest: RwLock<TableInfo>,
        metadata_error: Option<&'static str>,
        write_error: Option<&'static str>,
        metadata_loads: AtomicUsize,
        requests: Mutex<Vec<WriteRequest>>,
    }

    impl TestBackend {
        fn new(latest: TableInfo) -> Self {
            Self {
                latest: RwLock::new(latest),
                metadata_error: None,
                write_error: None,
                metadata_loads: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn with_metadata_error(mut self, message: &'static str) -> Self {
            self.metadata_error = Some(message);
            self
        }

        fn with_write_error(mut self, message: &'static str) -> Self {
            self.write_error = Some(message);
            self
        }

        fn metadata_loads(&self) -> usize {
            self.metadata_loads.load(Ordering::SeqCst)
        }

        fn requests(&self) -> std::sync::MutexGuard<'_, Vec<WriteRequest>> {
            self.requests.lock().expect("request list lock poisoned")
        }
    }

    impl FlussBackend for TestBackend {
        fn write<'a>(
            &'a self,
            _ctx: &'a RequestContext,
            req: WriteRequest,
        ) -> BackendFuture<'a, WriteResult> {
            Box::pin(async move {
                let row_count = req.rows().len() as u64;
                self.requests
                    .lock()
                    .expect("request list lock poisoned")
                    .push(req);
                if let Some(message) = self.write_error {
                    return Err(GatewayError::unavailable(message));
                }
                Ok(WriteResult {
                    row_count,
                    failures: Vec::new(),
                })
            })
        }
    }

    impl TableInfoProvider for TestBackend {
        fn latest_table_info<'a>(
            &'a self,
            _ctx: &'a RequestContext,
            _table: &'a TableRef,
        ) -> BackendFuture<'a, TableInfo> {
            Box::pin(async move {
                self.metadata_loads.fetch_add(1, Ordering::SeqCst);
                if let Some(message) = self.metadata_error {
                    return Err(GatewayError::unavailable(message));
                }
                Ok(self
                    .latest
                    .read()
                    .expect("latest table info lock poisoned")
                    .clone())
            })
        }
    }

    fn table() -> TableRef {
        TableRef::new("fluss", "users")
    }

    fn table_info(schema_id: i32, include_name: bool) -> TableInfo {
        let mut schema = Schema::builder().column("id", DataTypes::int().as_non_nullable());
        if include_name {
            schema = schema.column("name", DataTypes::string());
        }
        let schema = schema.primary_key(vec!["id"]).build().unwrap();
        let descriptor = TableDescriptor::builder()
            .schema(schema)
            .distributed_by(Some(1), vec!["id".to_string()])
            .build()
            .unwrap();
        TableInfo::of(table(), 11, schema_id, descriptor, 0, 0)
    }

    fn log_table_info() -> TableInfo {
        let schema = Schema::builder()
            .column("sequence", DataTypes::bigint().as_non_nullable())
            .build()
            .unwrap();
        let descriptor = TableDescriptor::builder()
            .schema(schema)
            .distributed_by(Some(1), Vec::new())
            .build()
            .unwrap();
        TableInfo::of(TableRef::new("fluss", "events"), 12, 1, descriptor, 0, 0)
    }

    fn table_info_with_required_name(schema_id: i32) -> TableInfo {
        let schema = Schema::builder()
            .column("id", DataTypes::int().as_non_nullable())
            .column("name", DataTypes::string().as_non_nullable())
            .primary_key(vec!["id"])
            .build()
            .unwrap();
        let descriptor = TableDescriptor::builder()
            .schema(schema)
            .distributed_by(Some(1), vec!["id".to_string()])
            .build()
            .unwrap();
        TableInfo::of(table(), 11, schema_id, descriptor, 0, 0)
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers
    }

    fn context() -> RequestContext {
        RequestContext::new("request-1", Instant::now() + Duration::from_secs(5))
    }

    fn service(backend: Arc<TestBackend>) -> WriteService {
        WriteService::new(backend.clone(), backend)
    }

    #[tokio::test]
    async fn raw_http_body_reaches_backend_without_losing_bigint_precision() {
        let backend = Arc::new(TestBackend::new(log_table_info()));
        let service = service(backend.clone());
        let table = TableRef::new("fluss", "events");
        let result = service
            .write_json(
                &context(),
                table.clone(),
                &headers(),
                &Bytes::from_static(
                    br#"{"entries":[{"id":"a","append":{"sequence":9007199254740993}}]}"#,
                ),
            )
            .await
            .unwrap();

        assert_eq!(result.success_count(), 1);
        assert_eq!(backend.metadata_loads(), 1);
        let requests = backend.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].table(), &table);
        assert_eq!(requests[0].change_types(), &[ChangeType::AppendOnly]);
        assert_eq!(
            requests[0].rows()[0].values,
            vec![Datum::Int64(9_007_199_254_740_993)]
        );
    }

    #[tokio::test]
    async fn stale_schema_refreshes_once_and_redecodes_the_entire_batch() {
        let backend = Arc::new(TestBackend::new(table_info(2, true)));
        let service = service(backend.clone());
        service
            .decoders
            .install(&table(), &table_info(1, false))
            .unwrap();
        let result = service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(
                    br#"{"entries":[
                        {"id":"old-shape","upsert":{"id":1}},
                        {"id":"new-shape","upsert":{"id":2,"name":"Ada"}}
                    ]}"#,
                ),
            )
            .await
            .unwrap();

        assert_eq!(result.success_count(), 2);
        assert_eq!(backend.metadata_loads(), 1);
        {
            let requests = backend.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].rows()[0].values.len(), 2);
            assert_eq!(requests[0].rows()[0].values[1], Datum::Null);
            assert_eq!(
                requests[0].rows()[1].values[1],
                Datum::String("Ada".to_string().into())
            );
        }

        service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(
                    br#"{"entries":[{"id":"cached","upsert":{"id":3,"name":"Grace"}}]}"#,
                ),
            )
            .await
            .unwrap();
        assert_eq!(backend.metadata_loads(), 1);
        assert_eq!(backend.requests().len(), 2);
    }

    #[tokio::test]
    async fn unchanged_schema_id_is_not_rebuilt_or_retried() {
        let schema = table_info(1, false);
        let backend = Arc::new(TestBackend::new(schema.clone()));
        let service = service(backend.clone());
        let cached = service.decoders.install(&table(), &schema).unwrap();
        let error = service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(br#"{"entries":[{"id":"a","upsert":{"id":1,"ghost":"x"}}]}"#),
            )
            .await
            .unwrap_err();

        assert!(error.message().contains("unknown column `ghost`"));
        assert_eq!(backend.metadata_loads(), 1);
        assert!(backend.requests().is_empty());
        let current = service.decoders.current(&table()).unwrap();
        assert!(Arc::ptr_eq(&cached, &current));
    }

    #[tokio::test]
    async fn a_missing_required_column_can_succeed_after_schema_refresh() {
        let backend = Arc::new(TestBackend::new(table_info(2, false)));
        let service = service(backend.clone());
        service
            .decoders
            .install(&table(), &table_info_with_required_name(1))
            .unwrap();

        let result = service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(br#"{"entries":[{"id":"a","upsert":{"id":1}}]}"#),
            )
            .await
            .unwrap();

        assert_eq!(result.success_count(), 1);
        assert_eq!(backend.metadata_loads(), 1);
        assert_eq!(backend.requests()[0].rows()[0].values.len(), 1);
    }

    #[tokio::test]
    async fn value_errors_do_not_refresh_metadata_or_call_backend() {
        let schema = table_info(1, false);
        let backend = Arc::new(TestBackend::new(schema.clone()));
        let service = service(backend.clone());
        service.decoders.install(&table(), &schema).unwrap();
        let error = service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(br#"{"entries":[{"id":"a","upsert":{"id":"not-an-int"}}]}"#),
            )
            .await
            .unwrap_err();

        assert!(error.message().contains("expects INT"));
        assert_eq!(backend.metadata_loads(), 0);
        assert!(backend.requests().is_empty());
    }

    #[tokio::test]
    async fn schema_refresh_failure_is_returned_without_calling_backend() {
        let backend = Arc::new(
            TestBackend::new(table_info(2, true)).with_metadata_error("metadata unavailable"),
        );
        let service = service(backend.clone());
        service
            .decoders
            .install(&table(), &table_info(1, false))
            .unwrap();

        let error = service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(br#"{"entries":[{"id":"a","upsert":{"id":1,"name":"Ada"}}]}"#),
            )
            .await
            .unwrap_err();

        assert!(error.message().contains("metadata unavailable"));
        assert_eq!(backend.metadata_loads(), 1);
        assert!(backend.requests().is_empty());
    }

    #[tokio::test]
    async fn refreshed_schema_is_not_retried_twice() {
        let backend = Arc::new(TestBackend::new(table_info(2, true)));
        let service = service(backend.clone());
        service
            .decoders
            .install(&table(), &table_info(1, false))
            .unwrap();

        let error = service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(br#"{"entries":[{"id":"a","upsert":{"id":1,"ghost":"x"}}]}"#),
            )
            .await
            .unwrap_err();

        assert!(error.message().contains("unknown column `ghost`"));
        assert_eq!(backend.metadata_loads(), 1);
        assert!(backend.requests().is_empty());
    }

    #[tokio::test]
    async fn partial_update_can_succeed_after_schema_refresh() {
        let backend = Arc::new(TestBackend::new(table_info(2, true)));
        let service = service(backend.clone());
        service
            .decoders
            .install(&table(), &table_info(1, false))
            .unwrap();

        let result = service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(
                    br#"{
                        "partial_update_columns":["id","name"],
                        "entries":[{"id":"a","upsert":{"id":1,"name":"Ada"}}]
                    }"#,
                ),
            )
            .await
            .unwrap();

        assert_eq!(result.success_count(), 1);
        assert_eq!(backend.metadata_loads(), 1);
        let requests = backend.requests();
        assert_eq!(
            requests[0].partial_update_columns(),
            Some(["id".to_string(), "name".to_string()].as_slice())
        );
    }

    #[tokio::test]
    async fn delete_and_partial_update_use_sparse_rows_and_preserve_operation_order() {
        let schema = table_info(1, true);
        let backend = Arc::new(TestBackend::new(schema));
        let service = service(backend.clone());
        service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(
                    br#"{"entries":[
                        {"id":"upsert","upsert":{"id":1,"name":"Ada"}},
                        {"id":"delete","delete":{"id":2}}
                    ]}"#,
                ),
            )
            .await
            .unwrap();

        {
            let requests = backend.requests();
            assert_eq!(
                requests[0].change_types(),
                &[ChangeType::UpdateAfter, ChangeType::Delete]
            );
            assert_eq!(requests[0].rows()[1].values[1], Datum::Null);
        }

        backend.requests().clear();
        service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(
                    br#"{
                        "partial_update_columns":["id","name"],
                        "entries":[{"id":"partial","upsert":{"id":3,"name":null}}]
                    }"#,
                ),
            )
            .await
            .unwrap();
        let requests = backend.requests();
        assert_eq!(
            requests[0].partial_update_columns(),
            Some(["id".to_string(), "name".to_string()].as_slice())
        );
        assert_eq!(requests[0].rows()[0].values[1], Datum::Null);
    }

    #[tokio::test]
    async fn backend_request_failure_is_propagated() {
        let backend =
            Arc::new(TestBackend::new(table_info(1, false)).with_write_error("write unavailable"));
        let service = service(backend.clone());
        let error = service
            .write_json(
                &context(),
                table(),
                &headers(),
                &Bytes::from_static(br#"{"entries":[{"id":"first","upsert":{"id":1}}]}"#),
            )
            .await
            .unwrap_err();

        assert!(error.message().contains("write unavailable"));
        assert_eq!(backend.requests().len(), 1);
    }

    #[test]
    fn cache_replaces_a_recreated_table_even_when_its_schema_id_restarts() {
        let cache = SchemaDecoderCache::default();
        let old = table_info(7, false);
        cache.install(&table(), &old).unwrap();
        let mut recreated = table_info(1, true);
        recreated.table_id = 99;
        cache.install(&table(), &recreated).unwrap();

        let current = cache.current(&table()).unwrap();
        assert_eq!(current.version(), (99, 1));
        assert_eq!(current.decoder.row_type().fields().len(), 2);
    }

    #[test]
    fn partial_update_columns_validate_against_the_cached_schema() {
        let schema = table_info(1, true);
        let cached = SchemaDecoderCache::default()
            .install(&table(), &schema)
            .unwrap();
        for (json, mismatch) in [
            (
                r#"{"partial_update_columns":[],"entries":[{"id":"a","upsert":{"id":1}}]}"#,
                false,
            ),
            (
                r#"{"partial_update_columns":["name"],"entries":[{"id":"a","upsert":{"id":1}}]}"#,
                true,
            ),
            (
                r#"{"partial_update_columns":["id","ghost"],"entries":[{"id":"a","upsert":{"id":1}}]}"#,
                true,
            ),
            (
                r#"{"partial_update_columns":["id","id"],"entries":[{"id":"a","upsert":{"id":1}}]}"#,
                false,
            ),
        ] {
            let body: WriteBody<'_> = serde_json::from_str(json).unwrap();
            let error = decode_write_request(&table(), &body, &cached).unwrap_err();
            assert_eq!(error.is_schema_mismatch(), mismatch, "{json}");
        }
    }

    #[test]
    fn operation_shape_and_duplicate_ids_are_rejected_before_decoding() {
        let cached = SchemaDecoderCache::default()
            .install(&table(), &table_info(1, false))
            .unwrap();
        for json in [
            r#"{"entries":[{"id":"a"}]}"#,
            r#"{"entries":[{"id":"a","upsert":{"id":1},"delete":{"id":1}}]}"#,
            r#"{"entries":[{"id":"a","upsert":{"id":1}},{"id":"a","upsert":{"id":2}}]}"#,
        ] {
            let body: WriteBody<'_> = serde_json::from_str(json).unwrap();
            let error = decode_write_request(&table(), &body, &cached).unwrap_err();
            assert!(!error.is_schema_mismatch(), "{json}");
        }
    }
}
