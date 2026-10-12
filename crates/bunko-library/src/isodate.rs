//! `datetime.fromisoformat` / `datetime.fromtimestamp` exactly as CPython
//! 3.12's C implementation (`Modules/_datetimemodule.c`) behaves.
//!
//! `normalize_updated_at` decides which series facts win a merge, so the
//! accepted grammar (any single separator character, basic and week formats,
//! `,` fractions, odd offsets like `+00:99`) and the float rounding of the
//! re-rendered stamp are a compatibility surface, ported line by line.

/// Days before each month in a non-leap year (index 1..=12).
const DAYS_BEFORE_MONTH: [i64; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
const DAYS_IN_MONTH: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
/// `_ymd2ord(1970, 1, 1)`.
const EPOCH_ORDINAL: i64 = 719_163;

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    if month == 2 && is_leap(year) {
        29
    } else {
        DAYS_IN_MONTH[month as usize]
    }
}

fn days_before_year(year: i64) -> i64 {
    let y = year - 1;
    y * 365 + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)
}

/// `_ymd2ord`: 0001-01-01 is day 1.
fn ymd_to_ord(year: i64, month: i64, day: i64) -> i64 {
    days_before_year(year)
        + DAYS_BEFORE_MONTH[month as usize]
        + i64::from(month > 2 && is_leap(year))
        + day
}

/// `_ord2ymd`.
fn ord_to_ymd(ordinal: i64) -> (i64, i64, i64) {
    const DI400Y: i64 = 146_097;
    const DI100Y: i64 = 36_524;
    const DI4Y: i64 = 1_461;
    let n = ordinal - 1;
    let (n400, n) = (n.div_euclid(DI400Y), n.rem_euclid(DI400Y));
    let mut year = n400 * 400 + 1;
    let (n100, n) = (n / DI100Y, n % DI100Y);
    let (n4, n) = (n / DI4Y, n % DI4Y);
    let (n1, mut n) = (n / 365, n % 365);
    year += n100 * 100 + n4 * 4 + n1;
    if n1 == 4 || n100 == 4 {
        return (year - 1, 12, 31);
    }
    let leap = n1 == 3 && (n4 != 24 || n100 == 3);
    let mut month = (n + 50) >> 5;
    let mut preceding = DAYS_BEFORE_MONTH[month as usize] + i64::from(month > 2 && leap);
    if preceding > n {
        month -= 1;
        preceding -= DAYS_IN_MONTH[month as usize] + i64::from(month == 2 && leap);
    }
    n -= preceding;
    (year, month, n + 1)
}

/// Monday = 0.
fn weekday(year: i64, month: i64, day: i64) -> i64 {
    (ymd_to_ord(year, month, day) + 6) % 7
}

fn iso_week1_monday(year: i64) -> i64 {
    let first_day = ymd_to_ord(year, 1, 1);
    let first_weekday = (first_day + 6) % 7;
    let mut week1_monday = first_day - first_weekday;
    if first_weekday > 3 {
        week1_monday += 7;
    }
    week1_monday
}

fn iso_to_ymd(iso_year: i64, iso_week: i64, iso_day: i64) -> Option<(i64, i64, i64)> {
    if !(1..=9999).contains(&iso_year) {
        return None;
    }
    if iso_week <= 0 || iso_week >= 53 {
        let mut out_of_range = true;
        if iso_week == 53 {
            let first_weekday = weekday(iso_year, 1, 1);
            if first_weekday == 3 || (first_weekday == 2 && is_leap(iso_year)) {
                out_of_range = false;
            }
        }
        if out_of_range {
            return None;
        }
    }
    if iso_day <= 0 || iso_day >= 8 {
        return None;
    }
    let day_1 = iso_week1_monday(iso_year);
    Some(ord_to_ymd(day_1 + (iso_week - 1) * 7 + iso_day - 1))
}

/// A NUL-terminated view, as the C code reads past the logical end.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn at(&self, index: usize) -> u8 {
        self.0.get(index).copied().unwrap_or(0)
    }
}

fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

/// `parse_digits`: returns the new position, or None.
fn parse_digits(s: &Bytes<'_>, mut pos: usize, var: &mut i64, count: usize) -> Option<usize> {
    for _ in 0..count {
        let byte = s.at(pos);
        pos += 1;
        if !is_digit(byte) {
            return None;
        }
        *var = *var * 10 + i64::from(byte - b'0');
    }
    Some(pos)
}

fn find_separator(s: &Bytes<'_>, len: usize) -> Option<usize> {
    if len == 7 {
        return Some(7);
    }
    if s.at(4) == b'-' {
        if s.at(5) == b'W' {
            if len < 8 {
                return None;
            }
            if len > 8 && s.at(8) == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && is_digit(s.at(10)) {
                    return Some(8);
                }
                return Some(10);
            }
            return Some(8);
        }
        return Some(10);
    }
    if s.at(4) == b'W' {
        let mut idx = 7;
        while idx < len && is_digit(s.at(idx)) {
            idx += 1;
        }
        if idx < 9 {
            return Some(idx);
        }
        return Some(if idx % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

fn parse_date(s: &Bytes<'_>, len: usize) -> Option<(i64, i64, i64)> {
    let mut year = 0;
    let mut pos = parse_digits(s, 0, &mut year, 4)?;
    let uses_separator = s.at(pos) == b'-';
    if uses_separator {
        pos += 1;
    }
    if s.at(pos) == b'W' {
        pos += 1;
        let mut week = 0;
        let mut day = 0;
        pos = parse_digits(s, pos, &mut week, 2)?;
        if pos < len {
            if uses_separator {
                let byte = s.at(pos);
                pos += 1;
                if byte != b'-' {
                    return None;
                }
            }
            parse_digits(s, pos, &mut day, 1)?;
        } else {
            day = 1;
        }
        return iso_to_ymd(year, week, day);
    }
    let mut month = 0;
    let mut day = 0;
    pos = parse_digits(s, pos, &mut month, 2)?;
    if uses_separator {
        let byte = s.at(pos);
        pos += 1;
        if byte != b'-' {
            return None;
        }
    }
    parse_digits(s, pos, &mut day, 2)?;
    Some((year, month, day))
}

/// `parse_hh_mm_ss_ff`: `Err` = negative code, `Ok(rest)` with rest = 1 when
/// not at the end of the string.
fn parse_hh_mm_ss_ff(
    s: &Bytes<'_>,
    start: usize,
    end: usize,
    vals: &mut [i64; 4],
) -> Result<bool, ()> {
    *vals = [0; 4];
    let mut pos = start;
    let mut has_separator = true;
    for (index, slot) in vals.iter_mut().take(3).enumerate() {
        pos = parse_digits(s, pos, slot, 2).ok_or(())?;
        let c = s.at(pos);
        pos += 1;
        if index == 0 {
            has_separator = c == b':';
        }
        if pos >= end {
            return Ok(c != 0);
        } else if has_separator && c == b':' {
            continue;
        } else if c == b'.' || c == b',' {
            break;
        } else if !has_separator {
            pos -= 1;
        } else {
            return Err(());
        }
    }
    let remaining = end.saturating_sub(pos);
    let to_parse = remaining.min(6);
    pos = parse_digits(s, pos, &mut vals[3], to_parse).ok_or(())?;
    const CORRECTION: [i64; 5] = [100_000, 10_000, 1_000, 100, 10];
    if to_parse < 6 {
        // CPython indexes `correction[to_parse - 1]`; `to_parse == 0` reads
        // out of bounds there and can only arise with nothing after the
        // separator, which `parse_digits(.., 0)` turns into a 0 microsecond.
        if to_parse > 0 {
            vals[3] *= CORRECTION[to_parse - 1];
        }
    }
    while is_digit(s.at(pos)) {
        pos += 1;
    }
    Ok(s.at(pos) != 0)
}

/// Parsed time: (h, m, s, us, Some((offset_seconds, offset_us)) when aware).
type TimeParts = (i64, i64, i64, i64, Option<(i64, i64)>);

fn parse_time(s: &Bytes<'_>, start: usize, len: usize) -> Result<TimeParts, ()> {
    let end = start + len;
    let mut tz_pos = start;
    loop {
        let byte = s.at(tz_pos);
        if byte == b'Z' || byte == b'+' || byte == b'-' {
            break;
        }
        tz_pos += 1;
        if tz_pos >= end {
            break;
        }
    }
    let mut vals = [0i64; 4];
    let rest = parse_hh_mm_ss_ff(s, start, tz_pos, &mut vals)?;
    if tz_pos == end {
        if rest {
            return Err(());
        }
        return Ok((vals[0], vals[1], vals[2], vals[3], None));
    }
    if s.at(tz_pos) == b'Z' {
        if s.at(tz_pos + 1) != 0 {
            return Err(());
        }
        return Ok((vals[0], vals[1], vals[2], vals[3], Some((0, 0))));
    }
    let sign = if s.at(tz_pos) == b'-' { -1 } else { 1 };
    let mut tz = [0i64; 4];
    let rest = parse_hh_mm_ss_ff(s, tz_pos + 1, end, &mut tz)?;
    if rest {
        return Err(());
    }
    let offset = sign * (tz[0] * 3600 + tz[1] * 60 + tz[2]);
    Ok((
        vals[0],
        vals[1],
        vals[2],
        vals[3],
        Some((offset, tz[3] * sign)),
    ))
}

/// `datetime.fromisoformat(text)` as microseconds since the Unix epoch (UTC),
/// an offsetless value read as UTC (what `normalize_updated_at` does next).
/// `None` = `ValueError`.
pub fn fromisoformat_utc_micros(text: &str) -> Option<i64> {
    if text.chars().count() < 7 {
        return None;
    }
    let bytes = Bytes(text.as_bytes());
    let len = text.len();
    let separator = find_separator(&bytes, len)?;
    let (year, month, day) = parse_date(&bytes, separator)?;
    let mut time: TimeParts = (0, 0, 0, 0, None);
    if len > separator {
        let lead = bytes.at(separator);
        let skip = if lead & 0x80 == 0 {
            1
        } else {
            match lead & 0xf0 {
                0xe0 => 3,
                0xf0 => 4,
                _ => 2,
            }
        };
        let start = separator + skip;
        time = parse_time(&bytes, start, len.saturating_sub(start)).ok()?;
    }
    let (hour, minute, second, micro, tz) = time;
    // `new_datetime_ex` range checks.
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return None;
    }
    if !(0..=999_999).contains(&micro) {
        return None;
    }
    let offset_us = match tz {
        None => 0,
        Some((seconds, micros)) => {
            let total = seconds * 1_000_000 + micros;
            // `timezone(offset)` requires strictly less than 24 hours.
            if total.abs() >= 86_400 * 1_000_000 {
                return None;
            }
            total
        }
    };
    let days = ymd_to_ord(year, month, day) - EPOCH_ORDINAL;
    let local = ((days * 86_400 + hour * 3600 + minute * 60 + second) * 1_000_000) + micro;
    Some(local - offset_us)
}

/// `timedelta.total_seconds()` of a microsecond count: the correctly rounded
/// float of `micros / 10**6`.
pub fn micros_to_seconds(micros: i64) -> f64 {
    let sign = if micros < 0 { "-" } else { "" };
    let abs = micros.unsigned_abs();
    format!("{sign}{}.{:06}", abs / 1_000_000, abs % 1_000_000)
        .parse()
        .unwrap_or(f64::NAN)
}

fn round_half_even(x: f64) -> f64 {
    let rounded = x.round();
    if (x - rounded).abs() == 0.5 {
        2.0 * (x / 2.0).round()
    } else {
        rounded
    }
}

/// `_PyTime_ObjectToTimeval(seconds, ROUND_HALF_EVEN)`: (whole seconds, microseconds).
fn float_to_timeval(seconds: f64) -> Option<(i64, i64)> {
    if !seconds.is_finite() {
        return None;
    }
    let mut intpart = seconds.trunc();
    let mut floatpart = (seconds - intpart) * 1e6;
    floatpart = round_half_even(floatpart);
    if floatpart >= 1e6 {
        floatpart -= 1e6;
        intpart += 1.0;
    } else if floatpart < 0.0 {
        floatpart += 1e6;
        intpart -= 1.0;
    }
    if intpart.abs() > 1e15 {
        return None;
    }
    Some((intpart as i64, floatpart as i64))
}

/// UTC civil fields of `datetime.fromtimestamp(seconds, tz=UTC)`; `None` when
/// CPython would raise (year outside 1..=9999).
pub fn utc_fields(seconds: f64) -> Option<(i64, i64, i64, i64, i64, i64, i64)> {
    let (whole, micros) = float_to_timeval(seconds)?;
    let days = whole.div_euclid(86_400);
    let secs = whole.rem_euclid(86_400);
    let ordinal = days + EPOCH_ORDINAL;
    if ordinal < 1 {
        return None;
    }
    let (year, month, day) = ord_to_ymd(ordinal);
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some((
        year,
        month,
        day,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        micros,
    ))
}

/// `iso_stamp(seconds)`: `Date.prototype.toISOString()` shape. The year is NOT
/// zero-padded below 1000, because glibc's `strftime("%Y")` (which 0.5.2 used)
/// does not pad it.
pub fn iso_stamp(seconds: f64) -> Option<String> {
    let (year, month, day, hour, minute, second, micros) = utc_fields(seconds)?;
    Some(format!(
        "{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        micros / 1000
    ))
}

/// `datetime.fromtimestamp(seconds, UTC).strftime("%Y-%m-%dT%H:%M:%SZ")`
/// (the manifest's `modified`).
pub fn iso_seconds_stamp(seconds: f64) -> Option<String> {
    let (year, month, day, hour, minute, second, _) = utc_fields(seconds)?;
    Some(format!(
        "{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinals_round_trip() {
        for ordinal in [1, 59, 60, 365, 366, 719_163, 730_120, 3_652_059] {
            let (y, m, d) = ord_to_ymd(ordinal);
            assert_eq!(ymd_to_ord(y, m, d), ordinal, "{ordinal} -> {y}-{m}-{d}");
        }
        assert_eq!(ord_to_ymd(EPOCH_ORDINAL), (1970, 1, 1));
        assert_eq!(ord_to_ymd(ymd_to_ord(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(ord_to_ymd(ymd_to_ord(2024, 3, 1)), (2024, 3, 1));
        assert_eq!(ord_to_ymd(ymd_to_ord(2023, 12, 31)), (2023, 12, 31));
    }

    #[test]
    fn parses_common_shapes() {
        let base = fromisoformat_utc_micros("2026-08-18T19:36:24.324+00:00").unwrap();
        assert_eq!(
            iso_stamp(micros_to_seconds(base)).unwrap(),
            "2026-08-18T19:36:24.324Z"
        );
        assert_eq!(
            fromisoformat_utc_micros("20260818T193624.324+0000"),
            Some(base)
        );
        assert_eq!(
            fromisoformat_utc_micros("2026-08-18 19:36:24,324"),
            Some(base)
        );
        assert!(fromisoformat_utc_micros("Aug 16 2020").is_none());
        assert!(fromisoformat_utc_micros("2026-02-30").is_none());
    }
}
