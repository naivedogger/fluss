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

//! `ScalarValue` -> [`KeyValue`] conversion for primary-key lookup.

use datafusion::common::ScalarValue;

use crate::source::KeyValue;
use crate::error::{FlussDatafusionError, Result};

pub(crate) fn scalar_to_key_value(value: &ScalarValue) -> Result<KeyValue> {
    match value {
        ScalarValue::Boolean(Some(v)) => Ok(KeyValue::Boolean(*v)),
        ScalarValue::Int8(Some(v)) => Ok(KeyValue::Int8(*v)),
        ScalarValue::Int16(Some(v)) => Ok(KeyValue::Int16(*v)),
        ScalarValue::Int32(Some(v)) => Ok(KeyValue::Int32(*v)),
        ScalarValue::Int64(Some(v)) => Ok(KeyValue::Int64(*v)),
        ScalarValue::Utf8(Some(v)) => Ok(KeyValue::String(v.clone())),
        ScalarValue::Boolean(None)
        | ScalarValue::Int8(None)
        | ScalarValue::Int16(None)
        | ScalarValue::Int32(None)
        | ScalarValue::Int64(None)
        | ScalarValue::Utf8(None)
        | ScalarValue::Null => Err(FlussDatafusionError::TypeConversion(
            "NULL literal cannot be used as a primary-key value".to_string(),
        )),
        other => Err(FlussDatafusionError::TypeConversion(format!(
            "scalar type {} is not supported as a primary-key value",
            other.data_type()
        ))),
    }
}

pub(crate) fn scalar_to_partition_string(value: &ScalarValue) -> Result<String> {
    match value {
        ScalarValue::Utf8(Some(v)) => Ok(v.clone()),
        ScalarValue::Int8(Some(v)) => Ok(v.to_string()),
        ScalarValue::Int16(Some(v)) => Ok(v.to_string()),
        ScalarValue::Int32(Some(v)) => Ok(v.to_string()),
        ScalarValue::Int64(Some(v)) => Ok(v.to_string()),
        ScalarValue::Boolean(Some(v)) => Ok(v.to_string()),
        ScalarValue::Utf8(None)
        | ScalarValue::Int8(None)
        | ScalarValue::Int16(None)
        | ScalarValue::Int32(None)
        | ScalarValue::Int64(None)
        | ScalarValue::Boolean(None)
        | ScalarValue::Null => Err(FlussDatafusionError::TypeConversion(
            "NULL literal cannot be used as a partition value".to_string(),
        )),
        other => Err(FlussDatafusionError::TypeConversion(format!(
            "scalar type {} is not supported as a partition value",
            other.data_type()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_supported_scalar_types() {
        assert_eq!(
            scalar_to_key_value(&ScalarValue::Boolean(Some(true))).unwrap(),
            KeyValue::Boolean(true)
        );
        assert_eq!(
            scalar_to_key_value(&ScalarValue::Int32(Some(42))).unwrap(),
            KeyValue::Int32(42)
        );
        assert_eq!(
            scalar_to_key_value(&ScalarValue::Utf8(Some("us".to_string()))).unwrap(),
            KeyValue::String("us".to_string())
        );
    }

    #[test]
    fn rejects_null_literals() {
        let err = scalar_to_key_value(&ScalarValue::Int32(None)).unwrap_err();
        assert!(matches!(err, FlussDatafusionError::TypeConversion(_)));
    }

    #[test]
    fn rejects_unsupported_types() {
        let err = scalar_to_key_value(&ScalarValue::Float64(Some(1.0))).unwrap_err();
        assert!(matches!(err, FlussDatafusionError::TypeConversion(_)));
    }

    #[test]
    fn partition_string_converts_utf8_and_int() {
        assert_eq!(
            scalar_to_partition_string(&ScalarValue::Utf8(Some("US".to_string()))).unwrap(),
            "US"
        );
        assert_eq!(
            scalar_to_partition_string(&ScalarValue::Int32(Some(42))).unwrap(),
            "42"
        );
    }

    #[test]
    fn partition_string_rejects_null() {
        let err = scalar_to_partition_string(&ScalarValue::Utf8(None)).unwrap_err();
        assert!(matches!(err, FlussDatafusionError::TypeConversion(_)));
    }
}
