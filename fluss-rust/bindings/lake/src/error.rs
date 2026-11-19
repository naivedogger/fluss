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

use fluss::error::{Error as CoreError, FlussError};
use fluss_lake::FlussLakeError;
use std::ffi::c_int;

// Keep in sync with FlussUnionErrorCode in fluss_union.h.
#[derive(Clone, Copy)]
#[repr(i32)]
pub(crate) enum ErrorCode {
    Client = -2,
    InvalidArgument = -1000,
    Cancelled = -1001,
    NotLakeReadable = -1002,
    PlanningFailed = -1003,
    DataUnavailable = -1004,
    SchemaIncompatible = -1005,
    ConnectionError = -1006,
    UnsupportedMergeEngine = -1007,
    IncompatibleSplitVersion = -1008,
    InvalidReadContext = -1009,
    Internal = -1010,
    MemoryLimitExceeded = -1011,
}

#[derive(Debug)]
pub(crate) struct AbiError {
    pub code: c_int,
    pub message: String,
}

impl AbiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code as c_int,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl ToString) -> Self {
        Self::new(ErrorCode::InvalidArgument, message.to_string())
    }

    pub fn internal(message: impl ToString) -> Self {
        Self::new(ErrorCode::Internal, message.to_string())
    }
}

impl From<CoreError> for AbiError {
    fn from(error: CoreError) -> Self {
        // Match the existing C++ binding's core error mapping.
        let code = match &error {
            CoreError::FlussAPIError { api_error } => api_error.code,
            CoreError::RpcError { .. } => FlussError::NetworkException.code(),
            CoreError::BufferExhausted { .. } => ErrorCode::MemoryLimitExceeded as c_int,
            _ => ErrorCode::Client as c_int,
        };
        let message = match error {
            CoreError::FlussAPIError { api_error } => api_error.message,
            error => error.to_string(),
        };
        Self { code, message }
    }
}

impl From<FlussLakeError> for AbiError {
    fn from(error: FlussLakeError) -> Self {
        if let FlussLakeError::Client { source, .. } = error {
            return AbiError::from(*source);
        }
        let code = match &error {
            FlussLakeError::MemoryLimitExceeded(_) => ErrorCode::MemoryLimitExceeded,
            FlussLakeError::NotLakeReadable(_) => ErrorCode::NotLakeReadable,
            FlussLakeError::PlanningFailed(_) => ErrorCode::PlanningFailed,
            FlussLakeError::DataUnavailable(_) => ErrorCode::DataUnavailable,
            FlussLakeError::SchemaIncompatible(_) => ErrorCode::SchemaIncompatible,
            FlussLakeError::ConnectionError(_) => ErrorCode::ConnectionError,
            FlussLakeError::UnsupportedMergeEngine(_) => ErrorCode::UnsupportedMergeEngine,
            FlussLakeError::IncompatibleSplitVersion(_) => ErrorCode::IncompatibleSplitVersion,
            FlussLakeError::InvalidReadContext(_) => ErrorCode::InvalidReadContext,
            _ => ErrorCode::Internal,
        };
        Self::new(code, error.to_string())
    }
}
