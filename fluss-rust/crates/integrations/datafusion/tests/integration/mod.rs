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

//! Integration test modules for fluss-datafusion.
//!
//! - `utils`: shared SQL-path helpers (always compiled).
//! - `setup`: shared real-cluster bootstrap + table DDL/DML (`integration_tests`).
//! - `e2e`: real-cluster end-to-end SQL through the real backend (`integration_tests`).
//! - `kv_bounded_scan`: real-cluster proof a KV table supports a bounded `LIMIT`
//!   scan and a full-PK point lookup (`integration_tests`).
//! - `kv_prefix_lookup`: real-cluster proof a KV table whose bucket key is a
//!   strict PK prefix supports prefix lookup, while a full-PK equality still uses
//!   point lookup (`integration_tests`).
//! - `kv_full_scan`: real-cluster proof a KV table supports a full-table scan
//!   with no filter and no LIMIT, merging the CDC changelog (`integration_tests`).
//! - `live_metadata`: real-cluster proof that post-registration DDL is visible
//!   live in the same session (`integration_tests`).
//! - `type_coverage`: real-cluster proof that the SQL read path decodes EVERY
//!   scalar Fluss type (plus nested array/map/row) across the KV point-lookup,
//!   KV full-scan, and log-scan decode paths (`integration_tests`).

pub mod utils;

#[cfg(feature = "integration_tests")]
pub mod setup;

#[cfg(feature = "integration_tests")]
pub mod e2e;

#[cfg(feature = "integration_tests")]
pub mod kv_bounded_scan;

#[cfg(feature = "integration_tests")]
pub mod kv_prefix_lookup;

#[cfg(feature = "integration_tests")]
pub mod kv_full_scan;

#[cfg(feature = "integration_tests")]
pub mod live_metadata;

#[cfg(feature = "integration_tests")]
pub mod type_coverage;
