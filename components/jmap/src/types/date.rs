/*
 * Copyright (c) 2020-2022, Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart JMAP Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::fmt::Display;

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct JMAPDate {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub tz_before_gmt: bool,
    pub tz_hour: u8,
    pub tz_minute: u8,
}

impl JMAPDate {
    pub fn parse(value: &str) -> Option<Self> {
        // 2004 - 06 - 28 T 23 : 43 : 45 . 000 Z
        // 1969 - 02 - 13 T 23 : 32 : 00 - 03 : 30
        //   0     1    2    3    4    5    6    7

        let mut pos = 0;
        let mut parts = [0u32; 8];
        let mut parts_sizes = [
            4u32, // Year (0)
            2u32, // Month (1)
            2u32, // Day (2)
            2u32, // Hour (3)
            2u32, // Minute (4)
            2u32, // Second (5)
            2u32, // TZ Hour (6)
            2u32, // TZ Minute (7)
        ];
        let mut skip_digits = false;
        let mut is_plus = true;

        for ch in value.as_bytes() {
            match ch {
                b'0'..=b'9' if !skip_digits => {
                    if parts_sizes[pos] > 0 {
                        parts_sizes[pos] -= 1;
                        parts[pos] += (*ch - b'0') as u32 * u32::pow(10, parts_sizes[pos]);
                    } else {
                        return None;
                    }
                }
                b'-' => {
                    if pos <= 1 {
                        pos += 1;
                    } else if pos == 5 {
                        pos += 1;
                        is_plus = false;
                        skip_digits = false;
                    } else {
                        return None;
                    }
                }
                b'T' => {
                    if pos == 2 {
                        pos += 1;
                    } else {
                        return None;
                    }
                }
                b':' => {
                    if [3, 4, 6].contains(&pos) {
                        pos += 1;
                    } else {
                        return None;
                    }
                }
                b'+' => {
                    if pos == 5 {
                        pos += 1;
                        skip_digits = false;
                    } else {
                        return None;
                    }
                }
                b'.' => {
                    if pos == 5 {
                        skip_digits = true;
                    } else {
                        return None;
                    }
                }

                _ => (),
            }
        }

        if pos >= 5 {
            JMAPDate {
                year: parts[0] as u16,
                month: parts[1] as u8,
                day: parts[2] as u8,
                hour: parts[3] as u8,
                minute: parts[4] as u8,
                second: parts[5] as u8,
                tz_hour: parts[6] as u8,
                tz_minute: parts[7] as u8,
                tz_before_gmt: !is_plus,
            }
            .into()
        } else {
            None
        }
    }

    pub fn from_timestamp(timestamp: i64) -> Self {
        // Ported from http://howardhinnant.github.io/date_algorithms.html#civil_from_days
        let (z, seconds) = ((timestamp / 86400) + 719468, timestamp % 86400);
        let era: i64 = (if z >= 0 { z } else { z - 146096 }) / 146097;
        let doe: u64 = (z - era * 146097) as u64; // [0, 146096]
        let yoe: u64 = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
        let y: i64 = (yoe as i64) + era * 400;
        let doy: u64 = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
        let mp = (5 * doy + 2) / 153; // [0, 11]
        let d: u64 = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
        let m: u64 = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
        let (h, mn, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);

        JMAPDate {
            year: (y + (if m <= 2 { 1 } else { 0 })) as u16,
            month: m as u8,
            day: d as u8,
            hour: h as u8,
            minute: mn as u8,
            second: s as u8,
            tz_before_gmt: false,
            tz_hour: 0,
            tz_minute: 0,
        }
    }

    pub fn is_valid(&self) -> bool {
        (0..=23).contains(&self.tz_hour)
            && (1970..=3000).contains(&self.year)
            && (0..=59).contains(&self.tz_minute)
            && (1..=12).contains(&self.month)
            && (1..=31).contains(&self.day)
            && (0..=23).contains(&self.hour)
            && (0..=59).contains(&self.minute)
            && (0..=59).contains(&self.second)
    }

    pub fn timestamp(&self) -> i64 {
        // Ported from https://github.com/protocolbuffers/upb/blob/22182e6e/upb/json_decode.c#L982-L992
        let month = self.month as u32;
        let year_base = 4800; /* Before min year, multiple of 400. */
        let m_adj = month.wrapping_sub(3); /* March-based month. */
        let carry = if m_adj > month { 1 } else { 0 };
        let adjust = if carry > 0 { 12 } else { 0 };
        let y_adj = self.year as i64 + year_base - carry;
        let month_days = ((m_adj.wrapping_add(adjust)) * 62719 + 769) / 2048;
        let leap_days = y_adj / 4 - y_adj / 100 + y_adj / 400;
        (y_adj * 365 + leap_days + month_days as i64 + (self.day as i64 - 1) - 2472632) * 86400
            + self.hour as i64 * 3600
            + self.minute as i64 * 60
            + self.second as i64
            + ((self.tz_hour as i64 * 3600 + self.tz_minute as i64 * 60)
                * if self.tz_before_gmt { 1 } else { -1 })
    }
}

impl Display for JMAPDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.tz_hour != 0 || self.tz_minute != 0 {
            write!(
                f,
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
                self.year,
                self.month,
                self.day,
                self.hour,
                self.minute,
                self.second,
                if self.tz_before_gmt && (self.tz_hour > 0 || self.tz_minute > 0) {
                    "-"
                } else {
                    "+"
                },
                self.tz_hour,
                self.tz_minute,
            )
        } else {
            write!(
                f,
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                self.year, self.month, self.day, self.hour, self.minute, self.second,
            )
        }
    }
}

impl serde::Serialize for JMAPDate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}

struct JMAPDateVisitor;

impl<'de> serde::de::Visitor<'de> for JMAPDateVisitor {
    type Value = JMAPDate;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a valid JMAP date")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(JMAPDate::parse(v).unwrap_or_default())
    }
}

impl<'de> serde::Deserialize<'de> for JMAPDate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(JMAPDateVisitor)
    }
}

#[cfg(test)]
mod tests {
    use crate::types::date::JMAPDate;

    #[test]
    fn parse_jmap_date() {
        for (input, expected_result) in [
            ("1997-11-21T09:55:06-06:00", "1997-11-21T09:55:06-06:00"),
            ("1997-11-21T09:55:06+00:00", "1997-11-21T09:55:06Z"),
            ("2021-01-01T09:55:06+02:00", "2021-01-01T09:55:06+02:00"),
            ("2004-06-28T23:43:45.000Z", "2004-06-28T23:43:45Z"),
            ("1997-11-21T09:55:06.123+00:00", "1997-11-21T09:55:06Z"),
            (
                "2021-01-01T09:55:06.4567+02:00",
                "2021-01-01T09:55:06+02:00",
            ),
        ] {
            let date = JMAPDate::parse(input).unwrap();
            assert_eq!(date.to_string(), expected_result);

            let timestamp = date.timestamp();
            assert_eq!(JMAPDate::from_timestamp(timestamp).timestamp(), timestamp);
        }
    }
}
