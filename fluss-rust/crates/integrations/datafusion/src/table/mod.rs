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

//! `TableProvider` logic and predicate recognition.
//!
//! `predicate` recognizes the narrow KV pushdown shape; `kv` is the KV
//! `TableProvider` that turns a recognized full-primary-key equality into a
//! point-lookup plan and fails conservatively otherwise. `log` is the log
//! `TableProvider` that turns a required `LIMIT` into a bounded scan and fails
//! conservatively when the `LIMIT` is absent.

pub(crate) mod kv;
pub(crate) mod log;
pub(crate) mod predicate;
