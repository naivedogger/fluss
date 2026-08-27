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

//! Shared temporal parsing and formatting for both codec directions.

const DAYS_PER_GREGORIAN_CYCLE: i64 = 146_097;
const DAYS_FROM_CIVIL_EPOCH_TO_UNIX_EPOCH: i64 = 719_468;

pub(super) struct ParsedTimestamp {
    pub(super) days: i64,
    pub(super) seconds_of_day: i64,
    pub(super) frac_nanos: i64,
    pub(super) offset_seconds: Option<i32>,
}

pub(super) fn parse_date(text: &str) -> Option<i64> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (year, month_and_day) = unsigned.split_once('-')?;
    let (month, day) = month_and_day.split_once('-')?;
    if year.len() < 4
        || month.len() != 2
        || day.len() != 2
        || month_and_day.matches('-').count() != 1
    {
        return None;
    }
    let unsigned_year: i64 = digits_only(year)?.parse().ok()?;
    let year = if negative {
        unsigned_year.checked_neg()?
    } else {
        unsigned_year
    };
    let month: u32 = digits_only(month)?.parse().ok()?;
    let day: u32 = digits_only(day)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    days_from_civil(year, month, day)
}

pub(super) fn parse_time(text: &str) -> Option<(i64, i64)> {
    let (clock, fraction) = match text.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (text, None),
    };
    let bytes = clock.as_bytes();
    let (hour, minute, second) = match bytes.len() {
        5 if bytes[2] == b':' => (clock.get(0..2)?, clock.get(3..5)?, "0"),
        8 if bytes[2] == b':' && bytes[5] == b':' => {
            (clock.get(0..2)?, clock.get(3..5)?, clock.get(6..8)?)
        }
        _ => return None,
    };
    let hour: i64 = digits_only(hour)?.parse().ok()?;
    let minute: i64 = digits_only(minute)?.parse().ok()?;
    let second: i64 = digits_only(second)?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let frac_nanos = match fraction {
        None => 0,
        Some(fraction) => {
            if fraction.is_empty() || fraction.len() > 9 {
                return None;
            }
            let digits: i64 = digits_only(fraction)?.parse().ok()?;
            digits * 10_i64.pow(9 - fraction.len() as u32)
        }
    };
    Some((hour * 3_600 + minute * 60 + second, frac_nanos))
}

pub(super) fn parse_timestamp(text: &str) -> Option<ParsedTimestamp> {
    let (date, rest) = text.split_once('T')?;
    let days = parse_date(date)?;
    let (time_part, offset_seconds) = split_zone(rest)?;
    let (seconds_of_day, frac_nanos) = parse_time(time_part)?;
    Some(ParsedTimestamp {
        days,
        seconds_of_day,
        frac_nanos,
        offset_seconds,
    })
}

pub(super) fn format_date(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    if year < 0 {
        format!("-{:04}-{month:02}-{day:02}", -year)
    } else if year > 9_999 {
        format!("+{year:04}-{month:02}-{day:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02}")
    }
}

pub(super) fn format_time(seconds_of_day: i64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds_of_day / 3_600,
        seconds_of_day % 3_600 / 60,
        seconds_of_day % 60
    )
}

pub(super) fn format_fraction(frac_nanos: i64, digits: u32) -> String {
    if digits == 0 {
        return String::new();
    }
    let scaled = frac_nanos / 10_i64.pow(9 - digits);
    format!(".{scaled:0width$}", width = digits as usize)
}

fn split_zone(text: &str) -> Option<(&str, Option<i32>)> {
    if let Some(time_part) = text.strip_suffix('Z') {
        return Some((time_part, Some(0)));
    }
    let Some(position) = text.rfind(['+', '-']) else {
        return Some((text, None));
    };
    let (time_part, zone) = text.split_at(position);
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let zone = &zone[1..];
    let (hours, minutes) = zone.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: i32 = digits_only(hours)?.parse().ok()?;
    let minutes: i32 = digits_only(minutes)?.parse().ok()?;
    if hours > 18 || minutes > 59 || (hours == 18 && minutes != 0) {
        return None;
    }
    Some((time_part, Some(sign * (hours * 3_600 + minutes * 60))))
}

fn digits_only(text: &str) -> Option<&str> {
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
        Some(text)
    } else {
        None
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let adjusted_year = i128::from(year) - i128::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i128::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i128::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::try_from(
        era * i128::from(DAYS_PER_GREGORIAN_CYCLE) + day_of_era
            - i128::from(DAYS_FROM_CIVIL_EPOCH_TO_UNIX_EPOCH),
    )
    .ok()
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + DAYS_FROM_CIVIL_EPOCH_TO_UNIX_EPOCH;
    let era = shifted.div_euclid(DAYS_PER_GREGORIAN_CYCLE);
    let day_of_era = shifted - era * DAYS_PER_GREGORIAN_CYCLE;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_dates_and_round_trips_day_counts() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("1969-12-31"), Some(-1));
        assert_eq!(parse_date("2026-02-30"), None);
        assert_eq!(parse_date("-0001-12-31"), Some(-719_529));
        assert_eq!(
            format_date(parse_date("+10000-01-01").unwrap()),
            "+10000-01-01"
        );
        for days in [
            i32::MIN as i64,
            -719_529,
            -719_468,
            -1,
            0,
            1,
            20_484,
            2_932_896,
            i32::MAX as i64,
        ] {
            assert_eq!(parse_date(&format_date(days)), Some(days), "{days}");
        }
    }

    #[test]
    fn parses_time_and_zone_boundaries() {
        assert_eq!(parse_time("12:34:56.789"), Some((45_296, 789_000_000)));
        assert_eq!(parse_time("24:00:00"), None);
        assert_eq!(
            parse_timestamp("2026-01-31T12:00:00+18:00")
                .unwrap()
                .offset_seconds,
            Some(18 * 3_600)
        );
        assert!(parse_timestamp("2026-01-31T12:00:00+18:01").is_none());
        assert_eq!(
            parse_timestamp("+10000-01-01T00:00:00Z")
                .unwrap()
                .offset_seconds,
            Some(0)
        );
        assert!(parse_timestamp("2026-01-31 12:00:00Z").is_none());
    }
}
