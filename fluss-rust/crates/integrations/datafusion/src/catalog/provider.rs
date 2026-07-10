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

//! `CatalogProvider` over a Fluss cluster.

use std::sync::Arc;

use datafusion::catalog::{CatalogProvider, SchemaProvider};
use datafusion::error::Result as DfResult;

use crate::catalog::schema::FlussSchemaProvider;
use crate::metadata::MetadataLoader;
use crate::sync_bridge::{block_on_with_runtime, ACCESS_PANIC};

#[derive(Debug)]
pub(crate) struct FlussCatalogProvider {
    loader: Arc<MetadataLoader>,
}

impl FlussCatalogProvider {
    pub(crate) fn new(loader: Arc<MetadataLoader>) -> Self {
        Self { loader }
    }
}

impl CatalogProvider for FlussCatalogProvider {
    fn schema_names(&self) -> Vec<String> {
        let source = self.loader.source();
        block_on_with_runtime(
            async move { source.list_databases().await.unwrap_or_default() },
            ACCESS_PANIC,
        )
    }

    fn schema(&self, name: &str) -> Option<Arc<dyn SchemaProvider>> {
        let loader = self.loader.clone();
        let name = name.to_string();
        let exists = block_on_with_runtime(
            {
                let loader = loader.clone();
                let name = name.clone();
                async move {
                    matches!(loader.source().list_databases().await, Ok(dbs) if dbs.iter().any(|d| d == &name))
                }
            },
            ACCESS_PANIC,
        );
        if !exists {
            return None;
        }
        Some(Arc::new(FlussSchemaProvider::new(name, loader)) as Arc<dyn SchemaProvider>)
    }

    fn register_schema(
        &self,
        _name: &str,
        _schema: Arc<dyn SchemaProvider>,
    ) -> DfResult<Option<Arc<dyn SchemaProvider>>> {
        Ok(None)
    }

    fn deregister_schema(
        &self,
        _name: &str,
        _cascade: bool,
    ) -> DfResult<Option<Arc<dyn SchemaProvider>>> {
        Ok(None)
    }
}
