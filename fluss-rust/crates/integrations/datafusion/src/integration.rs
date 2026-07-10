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

use std::sync::Arc;

use datafusion::execution::context::SessionContext;
use fluss::client::FlussConnection;

use crate::source::SharedFlussSource;
use crate::source::fluss_client::RealFlussSource;
use crate::catalog::build_catalog_provider;
use crate::config::{FlussDatafusionOptions, RegisterCatalogOptions};
use crate::error::Result;
use crate::metadata::MetadataLoader;

/// Shared, stateless integration entry point for Fluss DataFusion.
///
/// Built once around a connection; a SQL session then registers Fluss catalog
/// support into its own `SessionContext` via
/// [`FlussDatafusion::register_catalog`]. The catalog is fully live: it holds no
/// metadata cache, so DDL is visible in the same session immediately.
pub struct FlussDatafusion {
    inner: Arc<Inner>,
}

struct Inner {
    /// Shared, session-agnostic metadata loader (a thin, cache-free pass-through
    /// over the source). One instance per integration, reused across every
    /// `SessionContext`. The loader owns the single `FlussSource` handle; the rest
    /// of the crate reaches Fluss only through it.
    loader: Arc<MetadataLoader>,
}

impl FlussDatafusion {
    /// Builds the integration around a live Fluss connection.
    ///
    /// The connection is wrapped into an internal `Arc<dyn FlussSource>`; the
    /// crate never depends on `FlussConnection` outside this boundary.
    pub async fn new(
        connection: Arc<FlussConnection>,
        _options: FlussDatafusionOptions,
    ) -> Result<Self> {
        let source: SharedFlussSource = Arc::new(RealFlussSource::new(connection));
        Ok(Self::from_source(source))
    }

    fn from_source(source: SharedFlussSource) -> Self {
        let loader = Arc::new(MetadataLoader::new(source));
        Self {
            inner: Arc::new(Inner { loader }),
        }
    }

    /// Registers the Fluss catalog into a session context.
    ///
    /// Builds a `CatalogProvider` backed by the shared loader and installs it via
    /// `ctx.register_catalog`. The provider lists databases/tables live on every
    /// call, so DDL is visible in the same session without re-registering.
    pub async fn register_catalog(
        &self,
        ctx: &SessionContext,
        catalog_name: &str,
        _options: RegisterCatalogOptions,
    ) -> Result<()> {
        let provider = build_catalog_provider(self.inner.loader.clone());
        ctx.register_catalog(catalog_name, provider);
        Ok(())
    }
}
