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

//! Custom DataFusion `ExecutionPlan`s over the [`FlussSource`] seam.
//!
//! `lookup` is the KV point-lookup plan; `prefix_lookup` is the KV bucket-key
//! prefix-lookup plan; `log_scan` holds `FlussLogScanExec`, the bounded-scan plan
//! shared by both log and KV `LIMIT` scans (its `EXPLAIN` label is parameterized
//! per table type); `kv_full_scan` holds `FlussKvFullScanExec`, the unbounded KV
//! full-table scan (changelog merge); `stream` adapts async futures into a
//! `SendableRecordBatchStream`. All reach Fluss only through the source.

pub(crate) mod kv_full_scan;
pub(crate) mod log_scan;
pub(crate) mod lookup;
pub(crate) mod prefix_lookup;
pub(crate) mod stream;
