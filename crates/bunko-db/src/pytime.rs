//! The time formats 0.5.2 stores, and Python's `datetime.fromisoformat` parser.
//!
//! - SQLite `datetime('now')`: `YYYY-MM-DD HH:MM:SS`, UTC — produced in SQL, never here.
//! - `invites.expires_at`: Python `datetime.now().isoformat()` — LOCAL naive time (process
//!   TZ), `T` separator, `.ffffff` present unless the microsecond is 0. [`local_now`] and
//!   [`isoformat`] reproduce it so a Python 0.5.2 rollback reads Rust-written invites.
//! - `auth_tokens.*`: REAL epoch seconds ([`epoch_now`]).
//!
//! [`fromisoformat`] accepts what Python 3.12's `datetime.fromisoformat` accepts for the
//! inputs that occur here (calendar and ISO-week dates, extended or basic; any single
//! separator character; `HH[:MM[:SS[.f+]]]` or compact; `Z`/`±HH[:MM[:SS[.f]]]` offsets,
//! optionally after spaces).

use chrono::{Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

/// Seconds since the Unix epoch as a float (Python `time.time()`).
pub fn epoch_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python `datetime.now()`: local naive time at microsecond resolution.
pub fn local_now() -> NaiveDateTime {
    let now = Local::now().naive_local();
    let micros = now.nanosecond() / 1000 * 1000;
    now.with_nanosecond(micros).unwrap_or(now)
}

/// Python `datetime.isoformat()` of a naive datetime.
pub fn isoformat(dt: &NaiveDateTime) -> String {
    let micros = dt.nanosecond() / 1000;
    if micros == 0 {
        dt.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        format!("{}.{micros:06}", dt.format("%Y-%m-%dT%H:%M:%S"))
    }
}

/// `dt + duration`, `None` on overflow (Python raises `OverflowError`).
pub fn checked_add(dt: &NaiveDateTime, duration: Duration) -> Option<NaiveDateTime> {
    dt.checked_add_signed(duration)
}

/// A parsed ISO-8601 value: naive date-time plus the UTC offset in seconds, if one was
/// given (Python: an aware `datetime`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoDateTime {
    pub naive: NaiveDateTime,
    pub offset_seconds: Option<i32>,
}

impl IsoDateTime {
    /// The UTC naive equivalent (`astimezone(UTC).replace(tzinfo=None)`); itself if naive.
    pub fn to_utc_naive(&self) -> NaiveDateTime {
        match self.offset_seconds {
            Some(off) => self.naive - Duration::seconds(i64::from(off)),
            None => self.naive,
        }
    }
}

struct Cursor<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }
    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn digits(&mut self, n: usize) -> Option<u32> {
        let end = self.pos.checked_add(n)?;
        let part = self.s.get(self.pos..end)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.pos = end;
        Some(
            part.iter()
                .fold(0u32, |acc, d| acc * 10 + u32::from(d - b'0')),
        )
    }
    fn at_digit(&self) -> bool {
        self.peek().is_some_and(|b| b.is_ascii_digit())
    }
    fn done(&self) -> bool {
        self.pos >= self.s.len()
    }
}

/// Python 3.12 `datetime.fromisoformat(text)`; `None` where Python raises `ValueError`.
pub fn fromisoformat(text: &str) -> Option<IsoDateTime> {
    let bytes = text.as_bytes();
    let mut cur = Cursor { s: bytes, pos: 0 };
    let date = parse_date(&mut cur)?;
    if cur.done() {
        return Some(IsoDateTime {
            naive: date.and_time(NaiveTime::MIN),
            offset_seconds: None,
        });
    }
    // Any single character separates the date from the time (Python 3.11+).
    let rest = text.get(cur.pos..)?;
    let sep_len = rest.chars().next()?.len_utf8();
    let mut cur = Cursor {
        s: &bytes[cur.pos + sep_len..],
        pos: 0,
    };
    let (time, offset) = parse_time(&mut cur)?;
    if !cur.done() {
        return None;
    }
    Some(IsoDateTime {
        naive: date.and_time(time),
        offset_seconds: offset,
    })
}

fn parse_date(cur: &mut Cursor<'_>) -> Option<NaiveDate> {
    let year = cur.digits(4)? as i32;
    if year < 1 {
        return None;
    }
    let extended = cur.eat(b'-');
    if cur.eat(b'W') {
        let week = cur.digits(2)?;
        let mut weekday = 1;
        let has_day = if extended {
            cur.peek() == Some(b'-')
        } else {
            cur.at_digit()
        };
        if has_day {
            if extended {
                cur.eat(b'-');
            }
            weekday = cur.digits(1)?;
        }
        let wd = match weekday {
            1 => chrono::Weekday::Mon,
            2 => chrono::Weekday::Tue,
            3 => chrono::Weekday::Wed,
            4 => chrono::Weekday::Thu,
            5 => chrono::Weekday::Fri,
            6 => chrono::Weekday::Sat,
            7 => chrono::Weekday::Sun,
            _ => return None,
        };
        return NaiveDate::from_isoywd_opt(year, week, wd);
    }
    let month = cur.digits(2)?;
    if extended && !cur.eat(b'-') {
        return None;
    }
    let day = cur.digits(2)?;
    NaiveDate::from_ymd_opt(year, month, day)
}

fn parse_time(cur: &mut Cursor<'_>) -> Option<(NaiveTime, Option<i32>)> {
    let hour = cur.digits(2)?;
    let mut minute = 0;
    let mut second = 0;
    let mut micros = 0;
    let colon = cur.eat(b':');
    if colon || cur.at_digit() {
        minute = cur.digits(2)?;
        let colon2 = if colon { cur.eat(b':') } else { false };
        if colon2 || (!colon && cur.at_digit()) {
            second = cur.digits(2)?;
            if cur.eat(b'.') || cur.eat(b',') {
                if !cur.at_digit() {
                    return None;
                }
                let mut n = 0;
                while cur.at_digit() {
                    let d = u32::from(cur.peek()? - b'0');
                    if n < 6 {
                        micros = micros * 10 + d;
                    }
                    n += 1;
                    cur.pos += 1;
                }
                for _ in n.min(6)..6 {
                    micros *= 10;
                }
            }
        } else if colon && cur.peek() == Some(b':') {
            return None;
        }
    }
    let time = NaiveTime::from_hms_micro_opt(hour, minute, second, micros)?;
    if second > 59 {
        return None;
    }
    while cur.eat(b' ') {}
    if cur.done() {
        return Some((time, None));
    }
    if cur.eat(b'Z') {
        return Some((time, Some(0)));
    }
    let sign = if cur.eat(b'+') {
        1
    } else if cur.eat(b'-') {
        -1
    } else {
        return None;
    };
    let oh = cur.digits(2)?;
    let mut om = 0;
    let mut os = 0;
    let colon = cur.eat(b':');
    if colon || cur.at_digit() {
        om = cur.digits(2)?;
        if (colon && cur.eat(b':')) || (!colon && cur.at_digit()) {
            os = cur.digits(2)?;
            if cur.eat(b'.') {
                if !cur.at_digit() {
                    return None;
                }
                while cur.at_digit() {
                    cur.pos += 1;
                }
            }
        }
    }
    if oh > 23 || om > 59 || os > 59 {
        return None;
    }
    let off = sign * (oh * 3600 + om * 60 + os) as i32;
    Some((time, Some(off)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso(s: &str) -> Option<String> {
        fromisoformat(s).map(|p| {
            let mut out = isoformat(&p.naive);
            if let Some(off) = p.offset_seconds {
                out.push_str(&format!("{off:+}"));
            }
            out
        })
    }

    #[test]
    fn accepts_what_python_accepts() {
        // Expected values recorded from Python 3.12 `datetime.fromisoformat`.
        assert_eq!(iso("2026-10-01").as_deref(), Some("2026-10-01T00:00:00"));
        assert_eq!(iso("2026-10-01T10").as_deref(), Some("2026-10-01T10:00:00"));
        assert_eq!(
            iso("2026-10-01T10:00").as_deref(),
            Some("2026-10-01T10:00:00")
        );
        assert_eq!(
            iso("2026-10-01 10:00:00").as_deref(),
            Some("2026-10-01T10:00:00")
        );
        assert_eq!(
            iso("2026-10-01T10:00:00.5").as_deref(),
            Some("2026-10-01T10:00:00.500000")
        );
        assert_eq!(
            iso("2026-10-01T10:00:00,5").as_deref(),
            Some("2026-10-01T10:00:00.500000")
        );
        assert_eq!(iso("20261001").as_deref(), Some("2026-10-01T00:00:00"));
        assert_eq!(iso("20261001T1000").as_deref(), Some("2026-10-01T10:00:00"));
        assert_eq!(
            iso("2026-10-01T10:00:00+0530").as_deref(),
            Some("2026-10-01T10:00:00+19800")
        );
        assert_eq!(
            iso("2026-10-01T10:00:00+05").as_deref(),
            Some("2026-10-01T10:00:00+18000")
        );
        assert_eq!(iso("2026-W40-1").as_deref(), Some("2026-09-28T00:00:00"));
        assert_eq!(
            iso("2026-10-01x10:00").as_deref(),
            Some("2026-10-01T10:00:00")
        );
        assert_eq!(
            iso("2026-10-01\u{e9}10:00").as_deref(),
            Some("2026-10-01T10:00:00")
        );
        assert_eq!(
            iso("2026-10-01T10:00:00.1234567").as_deref(),
            Some("2026-10-01T10:00:00.123456")
        );
        assert_eq!(
            iso("2026-10-01T1000").as_deref(),
            Some("2026-10-01T10:00:00")
        );
        assert_eq!(
            iso("2026-10-01T10:00:00Z").as_deref(),
            Some("2026-10-01T10:00:00+0")
        );
        assert_eq!(
            iso("2026-10-01T10:00:00 +05:00").as_deref(),
            Some("2026-10-01T10:00:00+18000")
        );
        assert_eq!(
            iso("2026-10-01T10:00:00-00:00").as_deref(),
            Some("2026-10-01T10:00:00+0")
        );
        assert_eq!(
            iso("2026-10-08T14:03:22.123456").as_deref(),
            Some("2026-10-08T14:03:22.123456")
        );
    }

    #[test]
    fn refuses_what_python_refuses() {
        for bad in [
            "2026-10-01T24:00",
            "2026-1-01",
            " 2026-10-01",
            "2026-274",
            "2026-10-01T10:00:00.",
            "",
            "garbage",
            "2026-02-30",
            "0000-01-01",
        ] {
            assert!(fromisoformat(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn isoformat_omits_zero_micros() {
        let dt = NaiveDate::from_ymd_opt(2026, 10, 8)
            .unwrap()
            .and_hms_opt(14, 3, 22)
            .unwrap();
        assert_eq!(isoformat(&dt), "2026-10-08T14:03:22");
        let dt = dt.with_nanosecond(5_000).unwrap();
        assert_eq!(isoformat(&dt), "2026-10-08T14:03:22.000005");
    }
}
