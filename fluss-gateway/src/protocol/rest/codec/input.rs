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

//! Lossless JSON values used only while decoding one row.
//!
//! `serde_json::Value` cannot represent duplicate object fields, and without arbitrary precision it may
//! convert a large number through `f64`. Rows are therefore first retained as [`RawValue`] by the REST DTO,
//! then converted here into a small private tree that preserves number text and ordered duplicate fields.
//! Serde still owns JSON syntax validation, so the Gateway does not carry a second JSON parser. Because
//! recursively parsing [`RawValue`] subtrees can rescan bytes, each row also has an eight-times-size scan
//! budget.

use crate::error::GatewayError;
use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use std::fmt;

const MAX_NESTING_DEPTH: usize = 128;
const MAX_RECURSIVE_SCAN_MULTIPLIER: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InputValue<'a> {
    Null,
    Boolean(bool),
    ExactNumber(&'a str),
    String(String),
    Array(Vec<InputValue<'a>>),
    Object(Vec<(String, InputValue<'a>)>),
}

impl InputValue<'_> {
    pub(super) fn object_entries(&self) -> Option<&[(String, InputValue<'_>)]> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }
}

/// Parses one complete JSON value while retaining exact number text and duplicate object fields.
pub(super) fn parse_input_value(input: &[u8]) -> Result<InputValue<'_>, GatewayError> {
    let mut budget = ParseBudget::for_input(input.len());
    budget.consume(input.len())?;
    let raw: &RawValue = serde_json::from_slice(input)
        .map_err(|error| GatewayError::invalid_argument(format!("invalid JSON row: {error}")))?;
    parse_raw_value(raw, 0, &mut budget)
}

fn parse_raw_value<'a>(
    raw: &'a RawValue,
    depth: usize,
    budget: &mut ParseBudget,
) -> Result<InputValue<'a>, GatewayError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(GatewayError::invalid_argument(
            "JSON row nesting exceeds 128 levels",
        ));
    }
    budget.consume(raw.get().len())?;
    let text = raw.get().trim();
    let first =
        text.as_bytes().first().copied().ok_or_else(|| {
            GatewayError::invalid_argument("invalid JSON row: expected a JSON value")
        })?;
    match first {
        b'n' => Ok(InputValue::Null),
        b't' => Ok(InputValue::Boolean(true)),
        b'f' => Ok(InputValue::Boolean(false)),
        b'"' => serde_json::from_str(text)
            .map(InputValue::String)
            .map_err(invalid_nested_json),
        b'-' | b'0'..=b'9' => Ok(InputValue::ExactNumber(text)),
        b'[' => {
            let values: RawArray<'_> = serde_json::from_str(text).map_err(invalid_nested_json)?;
            values
                .0
                .into_iter()
                .map(|value| parse_raw_value(value, depth + 1, budget))
                .collect::<Result<Vec<_>, _>>()
                .map(InputValue::Array)
        }
        b'{' => {
            let entries: RawObject<'_> = serde_json::from_str(text).map_err(invalid_nested_json)?;
            entries
                .0
                .into_iter()
                .map(|(name, value)| {
                    parse_raw_value(value, depth + 1, budget).map(|value| (name, value))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(InputValue::Object)
        }
        _ => Err(GatewayError::invalid_argument(
            "invalid JSON row: expected a JSON value",
        )),
    }
}

struct ParseBudget {
    remaining: usize,
}

impl ParseBudget {
    fn for_input(input_len: usize) -> Self {
        Self {
            remaining: input_len.saturating_mul(MAX_RECURSIVE_SCAN_MULTIPLIER),
        }
    }

    fn consume(&mut self, bytes: usize) -> Result<(), GatewayError> {
        self.remaining = self.remaining.checked_sub(bytes).ok_or_else(|| {
            GatewayError::invalid_argument(
                "JSON row requires excessive recursive parsing for its size",
            )
        })?;
        Ok(())
    }
}

fn invalid_nested_json(error: serde_json::Error) -> GatewayError {
    GatewayError::invalid_argument(format!("invalid JSON row: {error}"))
}

struct RawArray<'a>(Vec<&'a RawValue>);

impl<'de> Deserialize<'de> for RawArray<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(RawArrayVisitor)
    }
}

struct RawArrayVisitor;

impl<'de> Visitor<'de> for RawArrayVisitor {
    type Value = RawArray<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(value) = sequence.next_element::<&'de RawValue>()? {
            values.push(value);
        }
        Ok(RawArray(values))
    }
}

struct RawObject<'a>(Vec<(String, &'a RawValue)>);

impl<'de> Deserialize<'de> for RawObject<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(RawObjectVisitor)
    }
}

struct RawObjectVisitor;

impl<'de> Visitor<'de> for RawObjectVisitor {
    type Value = RawObject<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
        while let Some(name) = map.next_key::<String>()? {
            let value = map.next_value::<&'de RawValue>()?;
            entries.push((name, value));
        }
        Ok(RawObject(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_exact_numbers_and_duplicate_fields() {
        let value =
            parse_input_value(br#"{"amount":9007199254740993.000000000000000001,"amount":2}"#)
                .unwrap();
        assert_eq!(
            value,
            InputValue::Object(vec![
                (
                    "amount".to_string(),
                    InputValue::ExactNumber("9007199254740993.000000000000000001")
                ),
                ("amount".to_string(), InputValue::ExactNumber("2")),
            ])
        );
    }

    #[test]
    fn rejects_trailing_data_and_excessive_nesting() {
        assert!(parse_input_value(b"1 2").is_err());
        let input = format!("{}0{}", "[".repeat(130), "]".repeat(130));
        assert!(parse_input_value(input.as_bytes()).is_err());
    }

    #[test]
    fn bounds_recursive_rescanning_without_rejecting_reasonable_nesting() {
        let reasonable = format!("{}0{}", "[".repeat(8), "]".repeat(8));
        assert!(parse_input_value(reasonable.as_bytes()).is_ok());

        let large_leaf = "x".repeat(256 * 1024);
        let amplified = format!("{}\"{large_leaf}\"{}", "[".repeat(16), "]".repeat(16));
        let error = parse_input_value(amplified.as_bytes()).unwrap_err();
        assert!(
            error
                .message()
                .contains("excessive recursive parsing for its size")
        );
    }
}
