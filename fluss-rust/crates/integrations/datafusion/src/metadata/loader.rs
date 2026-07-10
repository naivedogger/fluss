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

//! Live metadata loader.

use arrow::datatypes::SchemaRef;

use crate::source::{FlussTableMeta, SharedFlussSource, TableRef};
use crate::error::Result;

#[derive(Clone)]
pub(crate) struct TableEntry {
    pub meta: FlussTableMeta,
    pub arrow_schema: SchemaRef,
}

pub(crate) struct MetadataLoader {
    source: SharedFlussSource,
}

impl std::fmt::Debug for MetadataLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetadataLoader").finish_non_exhaustive()
    }
}

impl MetadataLoader {
    pub(crate) fn new(source: SharedFlussSource) -> Self {
        Self { source }
    }

    pub(crate) fn source(&self) -> SharedFlussSource {
        self.source.clone()
    }

    pub(crate) async fn table_entry(&self, table: &TableRef) -> Result<TableEntry> {
        let meta = self.source.get_table_meta(table).await?;
        let arrow_schema = arrow_schema_of(&meta)?;
        Ok(TableEntry { meta, arrow_schema })
    }
}

fn arrow_schema_of(meta: &FlussTableMeta) -> Result<SchemaRef> {
    Ok(fluss::record::to_arrow_schema(meta.schema.row_type())?)
}
