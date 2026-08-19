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

use crate::error::{Error, Result};
use crate::metadata::RowType;
use crate::row::binary::{BinaryWriter, ValueWriter};
use crate::row::field_getter::FieldGetter;
use crate::row::paimon::PaimonBinaryRowWriter;
use crate::row::{Datum, GenericRow, InternalRow};
use byteorder::{LittleEndian, WriteBytesExt};
use std::cmp::Ordering;
use std::io::Write;

pub(crate) const STATISTICS_VERSION: u8 = 1;

/// Collects and serializes the V1 `LogRecordBatch` column statistics.
///
/// The serialized bytes match Java's `LogRecordBatchStatisticsWriter`:
///
/// ```text
/// version | column count | column indexes | null counts |
/// min AlignedRow size | min AlignedRow | max AlignedRow size | max AlignedRow
/// ```
pub(crate) struct LogRecordBatchStatisticsCollector {
    statistics_column_indexes: Vec<usize>,
    statistics_column_types: Vec<crate::metadata::DataType>,
    field_getters: Vec<FieldGetter>,
    value_writers: Vec<ValueWriter>,
    min_values: Vec<Option<Datum<'static>>>,
    max_values: Vec<Option<Datum<'static>>>,
    null_counts: Vec<i32>,
}

impl LogRecordBatchStatisticsCollector {
    pub(crate) fn new(row_type: &RowType, statistics_column_indexes: Vec<usize>) -> Result<Self> {
        let mut field_getters = Vec::with_capacity(statistics_column_indexes.len());
        let mut value_writers = Vec::with_capacity(statistics_column_indexes.len());
        let mut statistics_column_types = Vec::with_capacity(statistics_column_indexes.len());
        for &index in &statistics_column_indexes {
            let field = row_type.fields().get(index).ok_or_else(|| Error::IllegalArgument {
                message: format!(
                    "Statistics column index {index} is out of bounds for row type with {} fields",
                    row_type.fields().len()
                ),
            })?;
            if !field.data_type().is_supported_statistics_type() {
                return Err(Error::IllegalArgument {
                    message: format!(
                        "Column '{}' of type {} is not supported for statistics collection",
                        field.name(),
                        field.data_type()
                    ),
                });
            }
            field_getters.push(FieldGetter::create(field.data_type(), index));
            value_writers.push(PaimonBinaryRowWriter::create_value_writer(
                field.data_type(),
            )?);
            statistics_column_types.push(field.data_type().clone());
        }

        let column_count = statistics_column_indexes.len();
        if column_count > i16::MAX as usize {
            return Err(Error::IllegalArgument {
                message: format!("Too many statistics columns: {column_count}"),
            });
        }
        if let Some(index) = statistics_column_indexes
            .iter()
            .find(|&&index| index > i16::MAX as usize)
        {
            return Err(Error::IllegalArgument {
                message: format!("Statistics column index {index} exceeds i16 range"),
            });
        }
        Ok(Self {
            statistics_column_indexes,
            statistics_column_types,
            field_getters,
            value_writers,
            min_values: vec![None; column_count],
            max_values: vec![None; column_count],
            null_counts: vec![0; column_count],
        })
    }

    pub(crate) fn empty_like(&self, row_type: &RowType) -> Result<Self> {
        Self::new(row_type, self.statistics_column_indexes.clone())
    }

    pub(crate) fn process_row(&mut self, row: &dyn InternalRow) -> Result<()> {
        for statistics_index in 0..self.statistics_column_indexes.len() {
            let value = self.field_getters[statistics_index]
                .get_field(row)?
                .into_owned();
            if value.is_null() {
                self.null_counts[statistics_index] += 1;
                continue;
            }

            if self.min_values[statistics_index]
                .as_ref()
                .is_none_or(|minimum| {
                    compare_statistics_values(
                        &self.statistics_column_types[statistics_index],
                        &value,
                        minimum,
                    ) == Ordering::Less
                })
            {
                self.min_values[statistics_index] = Some(value.clone());
            }
            if self.max_values[statistics_index]
                .as_ref()
                .is_none_or(|maximum| {
                    compare_statistics_values(
                        &self.statistics_column_types[statistics_index],
                        &value,
                        maximum,
                    ) == Ordering::Greater
                })
            {
                self.max_values[statistics_index] = Some(value);
            }
        }
        Ok(())
    }

    pub(crate) fn write_statistics(&self) -> Result<Vec<u8>> {
        let min_row = self.write_aligned_row(&self.min_values)?;
        let max_row = self.write_aligned_row(&self.max_values)?;

        let column_count = self.statistics_column_indexes.len() as i16;
        let mut output = Vec::with_capacity(
            3 + self.statistics_column_indexes.len() * 6 + 8 + min_row.len() + max_row.len(),
        );
        output.write_u8(STATISTICS_VERSION)?;
        output.write_i16::<LittleEndian>(column_count)?;
        for &index in &self.statistics_column_indexes {
            output.write_i16::<LittleEndian>(index as i16)?;
        }
        for &count in &self.null_counts {
            output.write_i32::<LittleEndian>(count)?;
        }
        output.write_i32::<LittleEndian>(i32::try_from(min_row.len()).map_err(|_| {
            Error::IllegalArgument {
                message: format!(
                    "Minimum statistics row is too large: {} bytes",
                    min_row.len()
                ),
            }
        })?)?;
        output.write_all(&min_row)?;
        output.write_i32::<LittleEndian>(i32::try_from(max_row.len()).map_err(|_| {
            Error::IllegalArgument {
                message: format!(
                    "Maximum statistics row is too large: {} bytes",
                    max_row.len()
                ),
            }
        })?)?;
        output.write_all(&max_row)?;
        Ok(output)
    }

    pub(crate) fn estimated_size_in_bytes(&self) -> usize {
        3 + self.statistics_column_indexes.len() * 6
            + 8
            + self.estimated_aligned_row_size(&self.min_values)
            + self.estimated_aligned_row_size(&self.max_values)
    }

    fn write_aligned_row(&self, values: &[Option<Datum<'static>>]) -> Result<Vec<u8>> {
        let row = GenericRow::from_data(
            values
                .iter()
                .map(|value| value.clone().unwrap_or(Datum::Null))
                .collect::<Vec<_>>(),
        );
        let mut writer = PaimonBinaryRowWriter::new(values.len());
        writer.reset();
        for (index, value_writer) in self.value_writers.iter().enumerate() {
            value_writer.write_value(&mut writer, index, &row.values[index])?;
        }
        writer.complete();
        Ok(writer.to_bytes().to_vec())
    }

    fn estimated_aligned_row_size(&self, values: &[Option<Datum<'static>>]) -> usize {
        let arity = values.len();
        let fixed_size = ((arity + 71) / 64) * 8 + arity * 8;
        fixed_size
            + values
                .iter()
                .zip(&self.statistics_column_types)
                .map(|(value, data_type)| match (value, data_type) {
                    (
                        Some(Datum::String(value)),
                        crate::metadata::DataType::String(_) | crate::metadata::DataType::Char(_),
                    ) if value.len() > 7 => value.len().div_ceil(8) * 8,
                    (Some(Datum::Decimal(_)), crate::metadata::DataType::Decimal(decimal_type))
                        if !crate::row::Decimal::is_compact_precision(decimal_type.precision()) =>
                    {
                        16
                    }
                    (
                        Some(Datum::TimestampNtz(_)),
                        crate::metadata::DataType::Timestamp(timestamp_type),
                    ) if !crate::row::TimestampNtz::is_compact(timestamp_type.precision()) => 8,
                    (
                        Some(Datum::TimestampLtz(_)),
                        crate::metadata::DataType::TimestampLTz(timestamp_type),
                    ) if !crate::row::TimestampLtz::is_compact(timestamp_type.precision()) => 8,
                    _ => 0,
                })
                .sum::<usize>()
    }
}

fn compare_statistics_values(
    data_type: &crate::metadata::DataType,
    left: &Datum<'_>,
    right: &Datum<'_>,
) -> Ordering {
    match (data_type, left, right) {
        (crate::metadata::DataType::Float(_), Datum::Float32(left), Datum::Float32(right)) => {
            java_f32_cmp(left.into_inner(), right.into_inner())
        }
        (crate::metadata::DataType::Double(_), Datum::Float64(left), Datum::Float64(right)) => {
            java_f64_cmp(left.into_inner(), right.into_inner())
        }
        _ => left.cmp(right),
    }
}

// Match Java's Float.compare/Double.compare semantics, including -0.0 and NaN.
fn java_f32_cmp(left: f32, right: f32) -> Ordering {
    if left < right {
        return Ordering::Less;
    }
    if left > right {
        return Ordering::Greater;
    }
    java_f32_bits(left).cmp(&java_f32_bits(right))
}

fn java_f64_cmp(left: f64, right: f64) -> Ordering {
    if left < right {
        return Ordering::Less;
    }
    if left > right {
        return Ordering::Greater;
    }
    java_f64_bits(left).cmp(&java_f64_bits(right))
}

fn java_f32_bits(value: f32) -> i32 {
    if value.is_nan() {
        0x7fc0_0000u32 as i32
    } else {
        value.to_bits() as i32
    }
}

fn java_f64_bits(value: f64) -> i64 {
    if value.is_nan() {
        0x7ff8_0000_0000_0000u64 as i64
    } else {
        value.to_bits() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{DataField, DataTypes};
    use crate::row::{Date, Decimal, Time, TimestampLtz, TimestampNtz};
    use bigdecimal::BigDecimal;
    use byteorder::{ByteOrder, LittleEndian};
    use std::str::FromStr;

    #[test]
    fn writes_java_compatible_statistics_layout() {
        let row_type = RowType::new(vec![
            DataField::new("id", DataTypes::int(), None),
            DataField::new("name", DataTypes::string(), None),
        ]);
        let mut collector = LogRecordBatchStatisticsCollector::new(&row_type, vec![0, 1]).unwrap();
        collector
            .process_row(&GenericRow::from_data(vec![
                Datum::Int32(6),
                Datum::String("high-6".into()),
            ]))
            .unwrap();
        collector
            .process_row(&GenericRow::from_data(vec![
                Datum::Int32(1),
                Datum::String("low-1".into()),
            ]))
            .unwrap();
        collector
            .process_row(&GenericRow::from_data(vec![Datum::Int32(7), Datum::Null]))
            .unwrap();

        let statistics = collector.write_statistics().unwrap();
        assert_eq!(statistics[0], STATISTICS_VERSION);
        assert_eq!(LittleEndian::read_i16(&statistics[1..3]), 2);
        assert_eq!(LittleEndian::read_i16(&statistics[3..5]), 0);
        assert_eq!(LittleEndian::read_i16(&statistics[5..7]), 1);
        assert_eq!(LittleEndian::read_i32(&statistics[7..11]), 0);
        assert_eq!(LittleEndian::read_i32(&statistics[11..15]), 1);

        let min_size = LittleEndian::read_i32(&statistics[15..19]) as usize;
        assert_eq!(min_size, 24);
        let min_row = &statistics[19..19 + min_size];
        assert_eq!(LittleEndian::read_i32(&min_row[8..12]), 1);
        assert_eq!(&min_row[16..22], b"high-6");
        assert_eq!(min_row[23], 0x86);

        let max_size_offset = 19 + min_size;
        let max_size =
            LittleEndian::read_i32(&statistics[max_size_offset..max_size_offset + 4]) as usize;
        assert_eq!(max_size, 24);
        let max_row = &statistics[max_size_offset + 4..max_size_offset + 4 + max_size];
        assert_eq!(LittleEndian::read_i32(&max_row[8..12]), 7);
        assert_eq!(&max_row[16..21], b"low-1");
        assert_eq!(max_row[23], 0x85);
    }

    #[test]
    fn uses_java_float_ordering_for_statistics() {
        assert_eq!(java_f32_cmp(-0.0, 0.0), Ordering::Less);
        assert_eq!(java_f32_cmp(f32::NAN, f32::INFINITY), Ordering::Greater);
        assert_eq!(java_f32_cmp(f32::NAN, -f32::NAN), Ordering::Equal);

        assert_eq!(java_f64_cmp(-0.0, 0.0), Ordering::Less);
        assert_eq!(java_f64_cmp(f64::NAN, f64::INFINITY), Ordering::Greater);
        assert_eq!(java_f64_cmp(f64::NAN, -f64::NAN), Ordering::Equal);
    }

    #[test]
    fn writes_char_statistics_as_aligned_strings() -> Result<()> {
        let row_type = RowType::new(vec![DataField::new("code", DataTypes::char(8), None)]);
        let mut collector = LogRecordBatchStatisticsCollector::new(&row_type, vec![0])?;
        collector.process_row(&GenericRow::from_data(vec![Datum::String("z".into())]))?;
        collector.process_row(&GenericRow::from_data(vec![Datum::String("a".into())]))?;

        let statistics = collector.write_statistics()?;
        let min_size_offset = 3 + 2 + 4;
        let min_size =
            LittleEndian::read_i32(&statistics[min_size_offset..min_size_offset + 4]) as usize;
        let min_row = &statistics[min_size_offset + 4..min_size_offset + 4 + min_size];
        assert_eq!(min_row[8], b'a');
        assert_eq!(min_row[15], 0x81);

        let max_size_offset = min_size_offset + 4 + min_size;
        let max_size =
            LittleEndian::read_i32(&statistics[max_size_offset..max_size_offset + 4]) as usize;
        let max_row = &statistics[max_size_offset + 4..max_size_offset + 4 + max_size];
        assert_eq!(max_row[8], b'z');
        assert_eq!(max_row[15], 0x81);
        Ok(())
    }

    #[test]
    fn matches_java_statistics_bytes_for_scalar_types() -> Result<()> {
        let row_type = RowType::new(vec![
            DataField::new("bool", DataTypes::boolean(), None),
            DataField::new("tiny", DataTypes::tinyint(), None),
            DataField::new("small", DataTypes::smallint(), None),
            DataField::new("int", DataTypes::int(), None),
            DataField::new("big", DataTypes::bigint(), None),
            DataField::new("float", DataTypes::float(), None),
            DataField::new("double", DataTypes::double(), None),
            DataField::new("string", DataTypes::string(), None),
            DataField::new("compact_decimal", DataTypes::decimal(10, 2), None),
            DataField::new("decimal", DataTypes::decimal(30, 5), None),
            DataField::new("date", DataTypes::date(), None),
            DataField::new("time", DataTypes::time_with_precision(3), None),
            DataField::new("timestamp3", DataTypes::timestamp_with_precision(3), None),
            DataField::new("timestamp9", DataTypes::timestamp_with_precision(9), None),
            DataField::new("ltz3", DataTypes::timestamp_ltz_with_precision(3), None),
            DataField::new("ltz9", DataTypes::timestamp_ltz_with_precision(9), None),
        ]);
        let mut collector = LogRecordBatchStatisticsCollector::new(&row_type, (0..16).collect())?;
        collector.process_row(&GenericRow::from_data(vec![
            Datum::Bool(true),
            Datum::Int8(-7),
            Datum::Int16(-300),
            Datum::Int32(-1000),
            Datum::Int64(-10_000_000_000),
            Datum::from(-1.5_f32),
            Datum::from(-2.5_f64),
            Datum::String("long-string".into()),
            Datum::Decimal(Decimal::from_unscaled_long(-12_345, 10, 2)?),
            Datum::Decimal(Decimal::from_big_decimal(
                BigDecimal::from_str("-12345678901234567890.12345").unwrap(),
                30,
                5,
            )?),
            Datum::Date(Date::new(-10)),
            Datum::Time(Time::new(12_345)),
            Datum::TimestampNtz(TimestampNtz::new(-1_000)),
            Datum::TimestampNtz(TimestampNtz::from_millis_nanos(-1_000, 123_456)?),
            Datum::TimestampLtz(TimestampLtz::new(-2_000)),
            Datum::TimestampLtz(TimestampLtz::from_millis_nanos(-2_000, 654_321)?),
        ]))?;
        collector.process_row(&GenericRow::from_data(vec![
            Datum::Bool(false),
            Datum::Int8(8),
            Datum::Int16(400),
            Datum::Int32(2_000),
            Datum::Int64(20_000_000_000),
            Datum::from(3.5_f32),
            Datum::from(4.5_f64),
            Datum::String("z".into()),
            Datum::Decimal(Decimal::from_unscaled_long(67_890, 10, 2)?),
            Datum::Decimal(Decimal::from_big_decimal(
                BigDecimal::from_str("98765432109876543210.54321").unwrap(),
                30,
                5,
            )?),
            Datum::Date(Date::new(20)),
            Datum::Time(Time::new(54_321)),
            Datum::TimestampNtz(TimestampNtz::new(3_000)),
            Datum::TimestampNtz(TimestampNtz::from_millis_nanos(3_000, 987_654)?),
            Datum::TimestampLtz(TimestampLtz::new(4_000)),
            Datum::TimestampLtz(TimestampLtz::from_millis_nanos(4_000, 111_111)?),
        ]))?;

        // Fixture generated by Java's LogRecordBatchStatisticsCollector and
        // LogRecordBatchStatisticsWriter using the same schema and rows.
        let java_fixture = decode_hex(concat!(
            "01100000000100020003000400050006000700080009000a000b000c000d000e000f",
            "00000000000000000000000000000000000000000000000000000000000000000000",
            "00000000000000000000000000000000000000000000000000000000000000b80000",
            "0000000000000000000000000000000000f900000000000000d4fe00000000000018",
            "fcffff00000000001cf4abfdffffff0000c0bf0000000000000000000004c00b0000",
            "0088000000c7cfffffffffffff0b00000098000000f6ffffff000000003930000000",
            "00000018fcffffffffffff40e20100a800000030f8fffffffffffff1fb0900b00000",
            "006c6f6e672d737472696e670000000000fefa91f0c959bbc21d2087000000000018",
            "fcffffffffffff30f8ffffffffffffa8000000000000000000000001000000000000",
            "0008000000000000009001000000000000d00700000000000000c817a80400000000",
            "0060400000000000000000000012407a0000000000008132090100000000000b0000",
            "0088000000140000000000000031d4000000000000b80b00000000000006120f0098",
            "000000a00f00000000000007b20100a0000000082b707af4f0a7dd68a27100000000",
            "00b80b000000000000a00f000000000000"
        ));
        assert_eq!(collector.estimated_size_in_bytes(), java_fixture.len());
        assert_eq!(collector.write_statistics()?, java_fixture);
        Ok(())
    }

    fn decode_hex(hex: &str) -> Vec<u8> {
        assert_eq!(hex.len() % 2, 0);
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(pair, 16).unwrap()
            })
            .collect()
    }
}
