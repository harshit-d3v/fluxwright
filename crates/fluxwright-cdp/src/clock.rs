//! Deterministic browser fake-clock controller (`page.clock`).

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::{Error, Result};

/// Browser-side fake clock controller script (`clock.js`).
pub const CLOCK_JS: &str = include_str!("clock.js");

/// A timestamp accepted by [`Clock`](crate) (`install`, `pause_at`, `set_fixed_time`, `set_system_time`).
#[derive(Debug, Clone, PartialEq)]
pub enum ClockTime {
    /// Milliseconds since the Unix epoch (`1970-01-01T00:00:00.000Z`).
    Ms(f64),
    /// An ISO 8601 or JavaScript `Date.parse`-compatible timestamp string.
    Iso(String),
}

impl From<f64> for ClockTime {
    fn from(ms: f64) -> Self {
        Self::Ms(ms)
    }
}

impl From<i64> for ClockTime {
    fn from(ms: i64) -> Self {
        Self::Ms(ms as f64)
    }
}

impl From<u64> for ClockTime {
    fn from(ms: u64) -> Self {
        Self::Ms(ms as f64)
    }
}

impl From<i32> for ClockTime {
    fn from(ms: i32) -> Self {
        Self::Ms(f64::from(ms))
    }
}

impl From<u32> for ClockTime {
    fn from(ms: u32) -> Self {
        Self::Ms(f64::from(ms))
    }
}

impl From<String> for ClockTime {
    fn from(iso: String) -> Self {
        Self::Iso(iso)
    }
}

impl From<&str> for ClockTime {
    fn from(iso: &str) -> Self {
        Self::Iso(iso.to_owned())
    }
}

impl From<SystemTime> for ClockTime {
    fn from(t: SystemTime) -> Self {
        let ms = match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs_f64() * 1000.0,
            Err(e) => -(e.duration().as_secs_f64() * 1000.0),
        };
        Self::Ms(ms)
    }
}

/// A non-negative duration accepted by `fast_forward` and `run_for`.
#[derive(Debug, Clone, PartialEq)]
pub enum ClockTicks {
    /// Duration in milliseconds.
    Ms(f64),
    /// Duration in `"MM:SS"` or `"HH:MM:SS"` format.
    Text(String),
}

impl From<f64> for ClockTicks {
    fn from(ms: f64) -> Self {
        Self::Ms(ms)
    }
}

impl From<i64> for ClockTicks {
    fn from(ms: i64) -> Self {
        Self::Ms(ms as f64)
    }
}

impl From<u64> for ClockTicks {
    fn from(ms: u64) -> Self {
        Self::Ms(ms as f64)
    }
}

impl From<i32> for ClockTicks {
    fn from(ms: i32) -> Self {
        Self::Ms(f64::from(ms))
    }
}

impl From<u32> for ClockTicks {
    fn from(ms: u32) -> Self {
        Self::Ms(f64::from(ms))
    }
}

impl From<String> for ClockTicks {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for ClockTicks {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

/// Options for `clock.install`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClockInstallOptions {
    /// Initial timestamp for the fake clock; defaults to real `Date.now()` when `None`.
    pub time: Option<ClockTime>,
}

/// Validates and converts a [`ClockTicks`] into non-negative milliseconds.
pub fn parse_clock_ticks(ticks: &ClockTicks) -> Result<f64> {
    match ticks {
        ClockTicks::Ms(ms) => {
            if !ms.is_finite() || *ms < 0.0 {
                return Err(Error::Other(format!(
                    "ClockTicks must be a non-negative finite number, got {ms}"
                )));
            }
            Ok(*ms)
        }
        ClockTicks::Text(raw) => {
            let parts: Vec<&str> = raw.split(':').collect();
            if parts.len() != 2 && parts.len() != 3 {
                return Err(Error::Other(format!(
                    "ClockTicks string must be \"MM:SS\" or \"HH:MM:SS\", got {raw:?}"
                )));
            }
            let nums: Vec<u64> = parts
                .iter()
                .map(|part| {
                    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                        return Err(Error::Other(format!(
                            "ClockTicks string must be \"MM:SS\" or \"HH:MM:SS\", got {raw:?}"
                        )));
                    }
                    part.parse::<u64>().map_err(|_| {
                        Error::Other(format!("ClockTicks value overflows u64: {raw:?}"))
                    })
                })
                .collect::<Result<_>>()?;
            let total_secs = if nums.len() == 2 {
                let (mm, ss) = (nums[0], nums[1]);
                if ss >= 60 {
                    return Err(Error::Other(format!(
                        "ClockTicks seconds must be < 60, got {raw:?}"
                    )));
                }
                mm.checked_mul(60).and_then(|m| m.checked_add(ss))
            } else {
                let (hh, mm, ss) = (nums[0], nums[1], nums[2]);
                if mm >= 60 || ss >= 60 {
                    return Err(Error::Other(format!(
                        "ClockTicks minutes and seconds must be < 60, got {raw:?}"
                    )));
                }
                hh.checked_mul(60)
                    .and_then(|h| h.checked_add(mm))
                    .and_then(|m| m.checked_mul(60))
                    .and_then(|s| s.checked_add(ss))
            };
            let secs = total_secs.ok_or_else(|| {
                Error::Other(format!("ClockTicks value overflows range: {raw:?}"))
            })?;
            let ms = secs
                .checked_mul(1000)
                .ok_or_else(|| Error::Other(format!("ClockTicks value overflows ms: {raw:?}")))?;
            Ok(ms as f64)
        }
    }
}

/// Validates and converts a [`ClockTime`] into a JSON value for `clock.js`
/// (either numeric Unix ms or a validated date string for `Date.parse`).
pub fn parse_clock_time(time: &ClockTime) -> Result<Value> {
    match time {
        ClockTime::Ms(ms) => {
            if !ms.is_finite() {
                return Err(Error::Other(format!(
                    "ClockTime must be a finite number, got {ms}"
                )));
            }
            Ok(Value::from(*ms))
        }
        ClockTime::Iso(raw) => {
            let s = raw.trim();
            if s.is_empty() {
                return Err(Error::Other("ClockTime string must not be empty".into()));
            }
            if looks_like_iso_date(s) {
                return match parse_iso_8601(s)? {
                    Some(ms) => Ok(Value::from(ms)),
                    // ISO timestamp without timezone offset uses the page's local/emulated timezone.
                    None => Ok(Value::String(s.to_owned())),
                };
            }
            if !s.bytes().any(|b| b.is_ascii_digit()) {
                return Err(Error::Other(format!(
                    "invalid ClockTime string: {raw:?}"
                )));
            }
            Ok(Value::String(s.to_owned()))
        }
    }
}

fn looks_like_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 7
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant's civil-from-days inverse: days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let d = i64::from(d);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_two_digits(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    if b.len() == 2 && b[0].is_ascii_digit() && b[1].is_ascii_digit() {
        s.parse::<u32>().ok()
    } else {
        None
    }
}

/// Validates ISO 8601 date / date-time strings.
/// Returns `Ok(Some(utc_ms))` when timezone is explicit (or date-only UTC midnight),
/// `Ok(None)` when valid ISO local date-time without a timezone designator,
/// or `Err` on malformed date/time/timezone fields.
fn parse_iso_8601(s: &str) -> Result<Option<f64>> {
    let err = || Error::Other(format!("invalid ClockTime ISO 8601 string: {s:?}"));
    let (date_part, time_and_tz) = match s.find(['T', 't', ' ']) {
        Some(idx) => (&s[..idx], Some(&s[idx + 1..])),
        None => (s, None),
    };
    let date_segs: Vec<&str> = date_part.split('-').collect();
    if date_segs.len() != 3 || date_segs[0].len() != 4 || !date_segs[0].bytes().all(|b| b.is_ascii_digit()) {
        return Err(err());
    }
    let year: i64 = date_segs[0].parse().map_err(|_| err())?;
    let month = parse_two_digits(date_segs[1]).ok_or_else(err)?;
    let day = parse_two_digits(date_segs[2]).ok_or_else(err)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Err(err());
    }

    let Some(rest) = time_and_tz else {
        let days = days_from_civil(year, month, day);
        return Ok(Some((days as f64) * 86_400_000.0));
    };

    if rest.is_empty() {
        return Err(err());
    }

    // Split optional timezone suffix: `Z`, `z`, `+HH:MM`, `-HH:MM`, `+HHMM`, `-HHMM`, `+HH`, `-HH`.
    let (time_part, tz_offset_ms) = if let Some(stripped) = rest.strip_suffix(['Z', 'z']) {
        (stripped, Some(0_i64))
    } else if let Some(pos) = rest.rfind(['+', '-']) {
        let tz = &rest[pos..];
        let sign = if tz.starts_with('-') { -1_i64 } else { 1_i64 };
        let tz_digits = &tz[1..];
        let (tz_h, tz_m) = if let Some((h, m)) = tz_digits.split_once(':') {
            (
                parse_two_digits(h).ok_or_else(err)?,
                parse_two_digits(m).ok_or_else(err)?,
            )
        } else if tz_digits.len() == 4 {
            (
                parse_two_digits(&tz_digits[..2]).ok_or_else(err)?,
                parse_two_digits(&tz_digits[2..]).ok_or_else(err)?,
            )
        } else if tz_digits.len() == 2 {
            (parse_two_digits(tz_digits).ok_or_else(err)?, 0)
        } else {
            return Err(err());
        };
        if tz_h > 23 || tz_m > 59 {
            return Err(err());
        }
        let offset_ms = sign * (i64::from(tz_h) * 3600 + i64::from(tz_m) * 60) * 1000;
        (&rest[..pos], Some(offset_ms))
    } else {
        (rest, None)
    };

    let (hms, frac_ms) = match time_part.split_once('.') {
        Some((hms, frac)) => {
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return Err(err());
            }
            let mut ms_digits = [0_u8; 3];
            for (i, b) in frac.bytes().take(3).enumerate() {
                ms_digits[i] = b - b'0';
            }
            let ms = i64::from(ms_digits[0]) * 100 + i64::from(ms_digits[1]) * 10 + i64::from(ms_digits[2]);
            (hms, ms)
        }
        None => (time_part, 0_i64),
    };

    let t_segs: Vec<&str> = hms.split(':').collect();
    if t_segs.len() != 2 && t_segs.len() != 3 {
        return Err(err());
    }
    let hour = parse_two_digits(t_segs[0]).ok_or_else(err)?;
    let minute = parse_two_digits(t_segs[1]).ok_or_else(err)?;
    let second = if t_segs.len() == 3 {
        parse_two_digits(t_segs[2]).ok_or_else(err)?
    } else {
        0
    };
    let valid_midnight_24 = hour == 24 && minute == 0 && second == 0 && frac_ms == 0;
    if !valid_midnight_24 && (hour > 23 || minute > 59 || second > 59) {
        return Err(err());
    }

    let Some(offset_ms) = tz_offset_ms else {
        return Ok(None);
    };

    let days = days_from_civil(year, month, day);
    let time_ms = (i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)) * 1000 + frac_ms;
    let utc_ms = days * 86_400_000 + time_ms - offset_ms;
    Ok(Some(utc_ms as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ticks_accepts_numbers_and_clock_strings() {
        assert_eq!(parse_clock_ticks(&ClockTicks::from(0)).unwrap(), 0.0);
        assert_eq!(parse_clock_ticks(&ClockTicks::from(1500)).unwrap(), 1500.0);
        assert_eq!(parse_clock_ticks(&ClockTicks::from(250.5)).unwrap(), 250.5);
        assert_eq!(parse_clock_ticks(&ClockTicks::from("00:00")).unwrap(), 0.0);
        assert_eq!(parse_clock_ticks(&ClockTicks::from("05:30")).unwrap(), 330_000.0);
        assert_eq!(parse_clock_ticks(&ClockTicks::from("90:00")).unwrap(), 5_400_000.0);
        assert_eq!(parse_clock_ticks(&ClockTicks::from("01:02:03")).unwrap(), 3_723_000.0);
    }

    #[test]
    fn parse_ticks_rejects_negative_nan_and_malformed() {
        assert!(parse_clock_ticks(&ClockTicks::from(-1)).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from(-0.5)).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from(f64::NAN)).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from(f64::INFINITY)).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from("")).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from("1000")).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from("05:60")).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from("01:60:00")).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from("-01:00")).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from("1:2:3:4")).is_err());
        assert!(parse_clock_ticks(&ClockTicks::from("bad")).is_err());
    }

    #[test]
    fn parse_time_accepts_ms_iso_and_system_time() {
        assert_eq!(parse_clock_time(&ClockTime::from(0)).unwrap(), Value::from(0.0));
        assert_eq!(
            parse_clock_time(&ClockTime::from("2024-02-02T08:00:00Z")).unwrap(),
            Value::from(1_706_860_800_000.0)
        );
        assert_eq!(
            parse_clock_time(&ClockTime::from("2024-02-02T10:00:00.250+02:00")).unwrap(),
            Value::from(1_706_860_800_250.0)
        );
        assert_eq!(
            parse_clock_time(&ClockTime::from("2024-02-02")).unwrap(),
            Value::from(1_706_832_000_000.0)
        );
        assert_eq!(
            parse_clock_time(&ClockTime::from("2024-02-02T08:00:00")).unwrap(),
            Value::String("2024-02-02T08:00:00".to_owned())
        );
        let st = UNIX_EPOCH + std::time::Duration::from_millis(12345);
        assert_eq!(parse_clock_time(&ClockTime::from(st)).unwrap(), Value::from(12345.0));
    }

    #[test]
    fn parse_time_rejects_nan_empty_and_malformed_iso() {
        assert!(parse_clock_time(&ClockTime::from(f64::NAN)).is_err());
        assert!(parse_clock_time(&ClockTime::from(f64::INFINITY)).is_err());
        assert!(parse_clock_time(&ClockTime::from("")).is_err());
        assert!(parse_clock_time(&ClockTime::from("   ")).is_err());
        assert!(parse_clock_time(&ClockTime::from("not-a-date")).is_err());
        assert!(parse_clock_time(&ClockTime::from("2024-02-30T00:00:00Z")).is_err());
        assert!(parse_clock_time(&ClockTime::from("2024-13-01")).is_err());
        assert!(parse_clock_time(&ClockTime::from("2024-02-02T25:00:00Z")).is_err());
        assert!(parse_clock_time(&ClockTime::from("2024-02-02T10:00:00+25:00")).is_err());
    }
}
