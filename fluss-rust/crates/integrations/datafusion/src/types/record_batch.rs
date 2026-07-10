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

//! Adapts a Fluss source `RecordBatch` into DataFusion-friendly output.
//!
//! The source returns a full-schema batch; column-level projection is the only
//! reshaping the crate does here. Arrow batch assembly itself stays in `fluss`'s
//! helpers, so this module never rebuilds columns from scratch.

use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow::error::ArrowError;

use crate::error::{FlussDatafusionError, Result};

/// Normalizes a scan projection against the full table schema.
///
/// A `None` projection — or a full identity projection like `Some([0, 1, .., N-1])`,
/// which `SELECT *` can produce — is normalized to `None` so the source reads every
/// column. Returns the normalized projection paired with its output schema.
pub(crate) fn normalize_projection(
    projection: Option<&Vec<usize>>,
    schema: &SchemaRef,
) -> std::result::Result<(Option<Vec<usize>>, SchemaRef), ArrowError> {
    let full_count = schema.fields().len();
    let normalized = match projection {
        Some(indices) if indices.iter().copied().eq(0..full_count) => None,
        other => other.cloned(),
    };
    let projected_schema = match &normalized {
        None => schema.clone(),
        Some(indices) => Arc::new(schema.project(indices)?),
    };
    Ok((normalized, projected_schema))
}

/// Projects `batch` down to `projection` (column indices into the full schema).
///
/// `None` passes the batch through unchanged.
pub(crate) fn project_batch(
    batch: RecordBatch,
    projection: Option<&[usize]>,
) -> Result<RecordBatch> {
    match projection {
        None => Ok(batch),
        Some(indices) => batch
            .project(indices)
            .map_err(|e| FlussDatafusionError::SchemaMismatch(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};

    use super::*;

    fn sample_batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1])),
                Arc::new(StringArray::from(vec!["a"])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn none_projection_passes_through() {
        let batch = sample_batch();
        let out = project_batch(batch.clone(), None).unwrap();
        assert_eq!(out.num_columns(), 2);
    }

    #[test]
    fn projection_selects_columns_in_order() {
        let batch = sample_batch();
        let out = project_batch(batch, Some(&[1])).unwrap();
        assert_eq!(out.num_columns(), 1);
        assert_eq!(out.schema().field(0).name(), "name");
    }
}
