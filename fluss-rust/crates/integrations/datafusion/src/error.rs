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

use std::error::Error;
use std::fmt::{Display, Formatter};

pub type Result<T> = std::result::Result<T, FlussDatafusionError>;

/// Crate-local error model.
#[derive(Debug)]
pub enum FlussDatafusionError {
    DatabaseNotFound(String),
    TableNotFound(String),
    UnsupportedQueryPattern(String),
    LimitRequired(String),
    SchemaMismatch(String),
    TypeConversion(String),
    FlussClient(String),
    Internal(String),
}

impl Display for FlussDatafusionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DatabaseNotFound(name) => write!(f, "database not found: {name}"),
            Self::TableNotFound(name) => write!(f, "table not found: {name}"),
            Self::UnsupportedQueryPattern(msg) => write!(f, "unsupported query pattern: {msg}"),
            Self::LimitRequired(msg) => write!(f, "LIMIT required: {msg}"),
            Self::SchemaMismatch(msg) => write!(f, "schema mismatch: {msg}"),
            Self::TypeConversion(msg) => write!(f, "type conversion error: {msg}"),
            Self::FlussClient(msg) => write!(f, "fluss client error: {msg}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl Error for FlussDatafusionError {}

impl From<fluss::error::Error> for FlussDatafusionError {
    fn from(err: fluss::error::Error) -> Self {
        Self::FlussClient(err.to_string())
    }
}

impl From<arrow::error::ArrowError> for FlussDatafusionError {
    fn from(err: arrow::error::ArrowError) -> Self {
        Self::SchemaMismatch(err.to_string())
    }
}

impl From<FlussDatafusionError> for datafusion::error::DataFusionError {
    fn from(err: FlussDatafusionError) -> Self {
        datafusion::error::DataFusionError::External(Box::new(err))
    }
}
