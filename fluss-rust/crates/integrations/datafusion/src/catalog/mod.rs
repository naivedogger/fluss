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

//! DataFusion catalog layer over Fluss metadata.

pub(crate) mod provider;
pub(crate) mod schema;

use std::sync::Arc;

use crate::catalog::provider::FlussCatalogProvider;
use crate::metadata::MetadataLoader;

/// Builds a catalog provider that serves every listing live from the loader.
///
/// Construction is a cheap, infallible wrap of the shared loader: there is no
/// pre-listing or I/O here, so this is neither async nor fallible.
pub(crate) fn build_catalog_provider(loader: Arc<MetadataLoader>) -> Arc<FlussCatalogProvider> {
    Arc::new(FlussCatalogProvider::new(loader))
}
