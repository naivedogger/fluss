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

//! A stateless DataFusion integration crate that turns SQL into access to Apache
//! Fluss KV and Log tables. It exposes Fluss tables to DataFusion as
//! [`CatalogProvider`] / [`SchemaProvider`] / [`TableProvider`], so you can query
//! Fluss directly via `ctx.sql(...)`.
//!
//! # Supported query forms
//!
//! - **KV (primary-key) tables**: full-primary-key equality point lookup, pushed
//!   down as a single point lookup. Both single and composite primary keys are
//!   supported (a composite key requires an equality predicate on every key
//!   column, combined with `AND`).
//! - **Log tables**: bounded last-N scan that *requires* a `LIMIT` clause, with
//!   projection pushdown.
//!
//! Unsupported patterns fail conservatively: they raise an explicit error at plan
//! time (for example a KV predicate on a non-primary-key column, or a Log query
//! without `LIMIT`) instead of silently degrading to a full-table scan.
//!
//! # Example
//!
//! ```rust,no_run
//! use std::sync::Arc;
//!
//! use datafusion::execution::context::SessionContext;
//! use fluss::client::FlussConnection;
//! use fluss::config::Config;
//!
//! use fluss_datafusion::{FlussDatafusion, FlussDatafusionOptions, RegisterCatalogOptions};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // 1) Connect to Fluss.
//!     let mut config = Config::default();
//!     config.bootstrap_servers = "127.0.0.1:9123".to_string();
//!     let connection = Arc::new(FlussConnection::new(config).await?);
//!
//!     // 2) Build the integration (construct once, reuse across sessions; metadata stays live).
//!     let fd = FlussDatafusion::new(connection, FlussDatafusionOptions::default()).await?;
//!
//!     // 3) Each session creates its own context and installs the catalog.
//!     let ctx = SessionContext::new();
//!     fd.register_catalog(&ctx, "fluss", RegisterCatalogOptions::default())
//!         .await?;
//!
//!     // 4) Query normally. Table names look like <catalog>.<database>.<table>.
//!     let df = ctx
//!         .sql("SELECT id, name FROM fluss.fluss.my_table LIMIT 10")
//!         .await?;
//!     df.show().await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! [`CatalogProvider`]: datafusion::catalog::CatalogProvider
//! [`SchemaProvider`]: datafusion::catalog::SchemaProvider
//! [`TableProvider`]: datafusion::catalog::TableProvider

mod source;
mod catalog;
mod config;
mod error;
mod execution;
mod integration;
mod metadata;
mod sync_bridge;
mod table;
mod types;

pub use config::{FlussDatafusionOptions, RegisterCatalogOptions};
pub use error::{FlussDatafusionError, Result};
pub use integration::FlussDatafusion;
