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

//! `SchemaProvider` for a single Fluss database.

use std::sync::Arc;

use async_trait::async_trait;
use datafusion::catalog::{SchemaProvider, TableProvider};
use datafusion::error::Result as DfResult;

use crate::source::TableRef;
use crate::error::FlussDatafusionError;
use crate::metadata::MetadataLoader;
use crate::sync_bridge::{block_on_with_runtime, ACCESS_PANIC};
use crate::table::kv::FlussKvTableProvider;
use crate::table::log::FlussLogTableProvider;

#[derive(Debug)]
pub(crate) struct FlussSchemaProvider {
    database: String,
    loader: Arc<MetadataLoader>,
}

impl FlussSchemaProvider {
    pub(crate) fn new(database: String, loader: Arc<MetadataLoader>) -> Self {
        Self { database, loader }
    }
}

#[async_trait]
impl SchemaProvider for FlussSchemaProvider {
    fn table_names(&self) -> Vec<String> {
        let loader = self.loader.clone();
        let database = self.database.clone();
        block_on_with_runtime(
            async move { loader.source().list_tables(&database).await.unwrap_or_default() },
            ACCESS_PANIC,
        )
    }

    async fn table(&self, name: &str) -> DfResult<Option<Arc<dyn TableProvider>>> {
        let table_ref = TableRef::new(self.database.clone(), name.to_string());
        let entry = match self.loader.table_entry(&table_ref).await {
            Ok(entry) => entry,
            Err(FlussDatafusionError::TableNotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if entry.meta.has_primary_key() {
            let provider = FlussKvTableProvider::new(
                self.loader.source(),
                table_ref,
                entry.arrow_schema,
                entry.meta.primary_keys.clone(),
                entry.meta.bucket_keys.clone(),
                entry.meta.num_buckets,
                entry.meta.partition_keys.clone(),
            );
            Ok(Some(Arc::new(provider)))
        } else {
            let provider = FlussLogTableProvider::new(
                self.loader.source(),
                table_ref,
                entry.arrow_schema,
                entry.meta.num_buckets,
                entry.meta.partition_keys.clone(),
            );
            Ok(Some(Arc::new(provider)))
        }
    }

    fn table_exist(&self, name: &str) -> bool {
        let loader = self.loader.clone();
        let database = self.database.clone();
        let name = name.to_string();
        block_on_with_runtime(
            async move {
                matches!(
                    loader.source().list_tables(&database).await,
                    Ok(tables) if tables.iter().any(|t| t == &name)
                )
            },
            ACCESS_PANIC,
        )
    }
}
