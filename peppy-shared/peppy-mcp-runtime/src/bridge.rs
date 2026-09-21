//! Conversions between canonical exposure JSON and Rust values, shared by
//! every bridge.
//!
//! The canonical mapping is fixed by the exposure format: `time` is an RFC
//! 3339 string with nanosecond precision, `bytes` and `u8` arrays are
//! base64, `u64` and `i64` are decimal strings, and everything else is the
//! matching JSON scalar. Bridges call these helpers instead of re-deriving
//! the rules, so the mapping lives in exactly one place per direction: the
//! catalog derives the schemas, this module moves the values.
//!
//! Errors are plain strings naming the offending field; bridges surface
//! them as tool errors or drop the snapshot they belong to.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The field when present; `None` when absent. Canonical JSON omits
/// optional fields instead of writing `null`.
pub fn optional<'a>(input: &'a Value, name: &str) -> Option<&'a Value> {
    input.as_object().and_then(|object| object.get(name))
}

/// The field, or an error naming it.
pub fn require<'a>(input: &'a Value, name: &str) -> Result<&'a Value, String> {
    optional(input, name).ok_or_else(|| format!("`{name}` is missing"))
}

pub fn value_bool(value: &Value, name: &str) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("`{name}` is not a boolean"))
}

pub fn value_string(value: &Value, name: &str) -> Result<String, String> {
    value_str(value, name).map(str::to_string)
}

/// The string a field holds, borrowed: for a writer that copies it onto the
/// wire itself and has no use for an owned copy in between.
pub fn value_str<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("`{name}` is not a string"))
}

pub fn value_f64(value: &Value, name: &str) -> Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| format!("`{name}` is not a number"))
}

pub fn value_array<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("`{name}` is not an array"))
}

macro_rules! unsigned_value_accessor {
    ($fn_name:ident, $ty:ty) => {
        pub fn $fn_name(value: &Value, name: &str) -> Result<$ty, String> {
            value
                .as_u64()
                .and_then(|wide| <$ty>::try_from(wide).ok())
                .ok_or_else(|| {
                    format!(
                        "`{name}` is not an integer between {} and {}",
                        <$ty>::MIN,
                        <$ty>::MAX
                    )
                })
        }
    };
}

macro_rules! signed_value_accessor {
    ($fn_name:ident, $ty:ty) => {
        pub fn $fn_name(value: &Value, name: &str) -> Result<$ty, String> {
            value
                .as_i64()
                .and_then(|wide| <$ty>::try_from(wide).ok())
                .ok_or_else(|| {
                    format!(
                        "`{name}` is not an integer between {} and {}",
                        <$ty>::MIN,
                        <$ty>::MAX
                    )
                })
        }
    };
}

unsigned_value_accessor!(value_u8, u8);
unsigned_value_accessor!(value_u16, u16);
unsigned_value_accessor!(value_u32, u32);
signed_value_accessor!(value_i8, i8);
signed_value_accessor!(value_i16, i16);
signed_value_accessor!(value_i32, i32);

/// A `u64` carried as a canonical decimal string, because JSON numbers lose
/// precision above 2^53. Canonical form is the catalog's
/// [`U64_DECIMAL_PATTERN`](peppy_mcp_catalog::U64_DECIMAL_PATTERN), the same
/// pattern the published input schemas carry.
pub fn value_u64_decimal(value: &Value, name: &str) -> Result<u64, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("`{name}` is not a decimal string"))?;
    if !peppy_mcp_catalog::is_canonical_u64_decimal(text) {
        return Err(format!("`{name}` is not a canonical decimal string"));
    }
    text.parse()
        .map_err(|_| format!("`{name}` is not a decimal string in u64 range"))
}

/// An `i64` carried as a canonical decimal string.
pub fn value_i64_decimal(value: &Value, name: &str) -> Result<i64, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("`{name}` is not a decimal string"))?;
    if !peppy_mcp_catalog::is_canonical_i64_decimal(text) {
        return Err(format!("`{name}` is not a canonical decimal string"));
    }
    text.parse()
        .map_err(|_| format!("`{name}` is not a decimal string in i64 range"))
}

/// Base64-carried bytes.
pub fn value_bytes(value: &Value, name: &str) -> Result<Vec<u8>, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("`{name}` is not a base64 string"))?;
    BASE64
        .decode(text.as_bytes())
        .map_err(|_| format!("`{name}` is not valid base64"))
}

pub fn bytes_to_base64(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

/// A finite float as a JSON number; NaN and infinities have no JSON
/// rendering and refuse the whole message.
pub fn float_to_json(value: f64) -> Result<Value, String> {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| format!("{value} is not a finite number"))
}

const SECONDS_PER_DAY: u64 = 86_400;

/// Renders a time as RFC 3339 with the full nanosecond precision the wire
/// carries, always in UTC. Times before the Unix epoch clamp to it; Peppy
/// timestamps are non-negative epoch offsets.
pub fn time_to_rfc3339(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs();
    let nanos = since_epoch.subsec_nanos();
    let (year, month, day) = civil_from_days((seconds / SECONDS_PER_DAY) as i64);
    let seconds_of_day = seconds % SECONDS_PER_DAY;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{nanos:09}Z",
        seconds_of_day / 3600,
        seconds_of_day % 3600 / 60,
        seconds_of_day % 60,
    )
}

/// Parses the canonical time rendering: `YYYY-MM-DDThh:mm:ss[.fraction]Z`.
/// Only UTC is accepted; offsets other than `Z` are refused.
pub fn value_time(value: &Value, name: &str) -> Result<SystemTime, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("`{name}` is not an RFC 3339 string"))?;
    parse_rfc3339_utc(text)
        .ok_or_else(|| format!("`{name}` is not an RFC 3339 UTC timestamp (`...Z`)"))
}

fn parse_rfc3339_utc(text: &str) -> Option<SystemTime> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;

    let mut date_parts = date.split('-');
    let year: i64 = parse_fixed_digits(date_parts.next()?, 4)?;
    let month: u32 = parse_fixed_digits(date_parts.next()?, 2)? as u32;
    let day: u32 = parse_fixed_digits(date_parts.next()?, 2)? as u32;
    if date_parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }

    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (time, None),
    };
    let mut clock_parts = clock.split(':');
    let hour: u64 = parse_fixed_digits(clock_parts.next()?, 2)? as u64;
    let minute: u64 = parse_fixed_digits(clock_parts.next()?, 2)? as u64;
    let second: u64 = parse_fixed_digits(clock_parts.next()?, 2)? as u64;
    if clock_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let nanos = match fraction {
        None => 0,
        Some(fraction) => {
            if fraction.is_empty()
                || fraction.len() > 9
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            let padded = format!("{fraction:0<9}");
            padded.parse::<u32>().ok()?
        }
    };

    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    let seconds = days as u64 * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second;
    Some(UNIX_EPOCH + Duration::new(seconds, nanos))
}

fn parse_fixed_digits(text: &str, digits: usize) -> Option<i64> {
    if text.len() != digits || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * shifted_month + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn times_render_as_rfc3339_with_nanosecond_precision() {
        let time = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
        assert_eq!(time_to_rfc3339(time), "2023-11-14T22:13:20.123456789Z");
        assert_eq!(
            time_to_rfc3339(UNIX_EPOCH),
            "1970-01-01T00:00:00.000000000Z"
        );
        let leap_day = UNIX_EPOCH + Duration::new(1_582_934_400, 0);
        assert_eq!(time_to_rfc3339(leap_day), "2020-02-29T00:00:00.000000000Z");
    }

    #[test]
    fn canonical_time_strings_parse_back_to_the_same_instant() {
        for (seconds, nanos) in [
            (0, 0),
            (951_782_400, 1),
            (1_700_000_000, 123_456_789),
            (4_102_444_799, 999_999_999),
        ] {
            let time = UNIX_EPOCH + Duration::new(seconds, nanos);
            let rendered = time_to_rfc3339(time);
            let parsed = value_time(&json!(rendered), "stamp").expect("canonical rendering parses");
            assert_eq!(parsed, time, "for {rendered}");
        }
    }

    #[test]
    fn time_parsing_accepts_short_fractions_and_refuses_non_utc() {
        let parsed = value_time(&json!("2023-11-14T22:13:20.5Z"), "stamp").expect("parses");
        assert_eq!(
            parsed,
            UNIX_EPOCH + Duration::new(1_700_000_000, 500_000_000)
        );
        let parsed = value_time(&json!("2023-11-14T22:13:20Z"), "stamp").expect("parses");
        assert_eq!(parsed, UNIX_EPOCH + Duration::new(1_700_000_000, 0));

        for bad in [
            "2023-11-14T22:13:20",
            "2023-11-14T22:13:20+00:00",
            "2023-11-14 22:13:20Z",
            "2023-13-14T22:13:20Z",
            "2023-02-29T00:00:00Z",
            "2023-11-14T24:00:00Z",
            "2023-11-14T22:13:20.1234567890Z",
            "not a time",
        ] {
            assert!(
                value_time(&json!(bad), "stamp").is_err(),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn decimal_strings_enforce_the_canonical_form() {
        assert_eq!(value_u64_decimal(&json!("0"), "n").expect("parses"), 0);
        assert_eq!(
            value_u64_decimal(&json!("18446744073709551615"), "n").expect("parses"),
            u64::MAX
        );
        for bad in [
            json!("007"),
            json!(""),
            json!(7),
            json!("18446744073709551616"),
        ] {
            assert!(
                value_u64_decimal(&bad, "n").is_err(),
                "{bad} should be refused"
            );
        }

        assert_eq!(value_i64_decimal(&json!("-1"), "n").expect("parses"), -1);
        assert_eq!(
            value_i64_decimal(&json!("-9223372036854775808"), "n").expect("parses"),
            i64::MIN
        );
        for bad in [json!("-0"), json!("--1"), json!("-07")] {
            assert!(
                value_i64_decimal(&bad, "n").is_err(),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn bytes_round_trip_through_base64() {
        let bytes = [0u8, 1, 254, 255];
        let rendered = bytes_to_base64(&bytes);
        assert_eq!(
            value_bytes(&json!(rendered), "data").expect("decodes"),
            bytes
        );
        assert!(value_bytes(&json!("not base64!"), "data").is_err());
    }

    #[test]
    fn ranged_integer_accessors_enforce_their_ranges() {
        assert_eq!(value_u8(&json!(255), "n").expect("fits"), 255);
        assert!(value_u8(&json!(256), "n").is_err());
        assert_eq!(value_i8(&json!(-128), "n").expect("fits"), -128);
        assert!(value_i8(&json!(-129), "n").is_err());
        assert!(value_u16(&json!(-1), "n").is_err());
    }

    #[test]
    fn non_finite_floats_have_no_json_rendering() {
        assert_eq!(float_to_json(1.5).expect("finite"), json!(1.5));
        assert!(float_to_json(f64::NAN).is_err());
        assert!(float_to_json(f64::INFINITY).is_err());
    }

    #[test]
    fn optional_distinguishes_absent_from_present() {
        let input = json!({ "there": 1 });
        assert!(optional(&input, "there").is_some());
        assert!(optional(&input, "missing").is_none());
        assert!(
            require(&input, "missing")
                .unwrap_err()
                .contains("`missing` is missing")
        );
    }
}
