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

use arrow::array::RecordBatchReader;
use arrow::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use fluss_lake::{FlussLakeError, PaimonBaselineReader, PaimonReadContext, RecordBatchStream};
use futures::future::BoxFuture;
use std::sync::Mutex;

pub(crate) struct ImportedBaseline(Mutex<Option<ArrowArrayStreamReader>>);

impl ImportedBaseline {
    pub unsafe fn take(raw: *mut FFI_ArrowArrayStream) -> Result<Option<Self>, crate::AbiError> {
        if raw.is_null() {
            return Ok(None);
        }
        let stream = unsafe { FFI_ArrowArrayStream::from_raw(raw) };
        let reader = ArrowArrayStreamReader::try_new(stream).map_err(crate::AbiError::invalid)?;
        Ok(Some(Self(Mutex::new(Some(reader)))))
    }
}

impl PaimonBaselineReader for ImportedBaseline {
    fn read<'a>(
        &'a self,
        context: PaimonReadContext<'a>,
    ) -> BoxFuture<'a, fluss_lake::Result<Option<RecordBatchStream>>> {
        Box::pin(async move {
            let reader = self.0.lock().unwrap().take().ok_or_else(|| {
                FlussLakeError::Internal("engine baseline already consumed".into())
            })?;
            let schema = reader.schema();
            if schema.fields().len() != context.schema.fields().len()
                || schema
                    .fields()
                    .iter()
                    .zip(context.schema.fields())
                    .any(|(a, b)| a.name() != b.name() || a.data_type() != b.data_type())
            {
                return Err(FlussLakeError::SchemaIncompatible(
                    "engine baseline must return the complete Fluss schema in order".into(),
                ));
            }
            // The engine supplies one snapshot current view for the whole bucket.
            // Never fall back after consuming any part of its stream.
            Ok(Some(Box::pin(futures::stream::iter(reader.map(|batch| {
                batch.map_err(|e| FlussLakeError::Internal(format!("engine baseline: {e}")))
            }))) as RecordBatchStream))
        })
    }
}
