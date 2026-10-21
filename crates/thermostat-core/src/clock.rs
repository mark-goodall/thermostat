//! Wall-clock time: a Unix-time anchor set by NTP, civil date conversion and
//! POSIX TZ strings (e.g. `GMT0BST,M3.5.0/1,M10.5.0`) for local time with DST.

use core::fmt;

/// Wall clock: Unix time anchored to the monotonic clock at the last sync.
#[derive(Debug, Clone, Copy, Default)]
pub struct WallClock {
    anchor: Option<(i64, u64)>, // (unix microseconds, monotonic microseconds)
    synced_at_mono_us: u64,
}

impl WallClock {
    pub const fn new() -> Self {
        Self {
            anchor: None,
            synced_at_mono_us: 0,
        }
    }

    /// Record that the Unix time was `unix_us` at monotonic time `mono_us`.
    pub fn set(&mut self, unix_us: i64, mono_us: u64) {
        self.anchor = Some((unix_us, mono_us));
        self.synced_at_mono_us = mono_us;
    }

    /// Unix time in microseconds, if synchronised.
    pub fn unix_us(&self, mono_us: u64) -> Option<i64> {
        self.anchor.map(|(u, m)| u + (mono_us as i64 - m as i64))
    }

    pub fn unix_s(&self, mono_us: u64) -> Option<i64> {
        self.unix_us(mono_us).map(|u| u.div_euclid(1_000_000))
    }

    /// Seconds since the last sync.
    pub fn since_sync_s(&self, mono_us: u64) -> Option<u64> {
        self.anchor
            .map(|_| mono_us.saturating_sub(self.synced_at_mono_us) / 1_000_000)
    }
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (yoe + era * 400 + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d)
}

/// 0 = Sunday .. 6 = Saturday.
pub fn weekday_from_days(z: i64) -> u32 {
    (z + 4).rem_euclid(7) as u32
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

/// Local broken-down time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = Sunday .. 6 = Saturday.
    pub weekday: u32,
    /// Offset from UTC in seconds (east positive).
    pub offset_s: i32,
    pub dst: bool,
}

impl DateTime {
    pub fn from_unix(unix_s: i64, offset_s: i32, dst: bool) -> Self {
        let local = unix_s + offset_s as i64;
        let days = local.div_euclid(86_400);
        let secs = local.rem_euclid(86_400) as u32;
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: secs / 3600,
            minute: secs / 60 % 60,
            second: secs % 60,
            weekday: weekday_from_days(days),
            offset_s,
            dst,
        }
    }

    /// Minutes since local midnight (handy for schedules).
    pub fn minute_of_day(&self) -> u32 {
        self.hour * 60 + self.minute
    }
}

/// RFC 3339, e.g. `2026-10-21T21:40:12+01:00`.
impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.offset_s < 0 { '-' } else { '+' };
        let off = self.offset_s.unsigned_abs();
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            sign,
            off / 3600,
            off / 60 % 60
        )
    }
}

/// DST transition rule `Mm.w.d[/time]`: day `d` (0 = Sunday) of week `w`
/// (1..5, 5 = last) of month `m`, at local wall time `time` (seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rule {
    month: u32,
    week: u32,
    weekday: u32,
    time_s: i32,
}

impl Rule {
    /// Day number (since epoch) of the transition in `year`.
    fn day(&self, year: i32) -> i64 {
        let first = days_from_civil(year, self.month, 1);
        let first_wd = weekday_from_days(first);
        let mut day = 1 + (self.weekday + 7 - first_wd) % 7 + (self.week - 1) * 7;
        let dim = days_in_month(year, self.month);
        while day > dim {
            day -= 7;
        }
        first + day as i64 - 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dst {
    /// Seconds east of UTC during DST.
    offset_s: i32,
    start: Rule,
    end: Rule,
}

/// A parsed POSIX TZ string (the format used by ESP-IDF and glibc `TZ`).
///
/// Supported: `STDoffset[DST[offset][,Mm.w.d[/time],Mm.w.d[/time]]]`, with
/// alphabetic or `<...>` quoted names. Julian-day rules (`Jn`, `n`) are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeZone {
    /// Seconds east of UTC outside DST.
    std_offset_s: i32,
    dst: Option<Dst>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TzError;

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn name(&mut self) -> Result<(), TzError> {
        let start = self.i;
        if self.eat(b'<') {
            while self.peek().is_some_and(|c| c != b'>') {
                self.i += 1;
            }
            if !self.eat(b'>') || self.i - start < 5 {
                return Err(TzError);
            }
        } else {
            while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                self.i += 1;
            }
            if self.i - start < 3 {
                return Err(TzError);
            }
        }
        Ok(())
    }

    fn number(&mut self, max: u32) -> Result<u32, TzError> {
        let start = self.i;
        let mut v = 0u32;
        while let Some(c) = self.peek().filter(u8::is_ascii_digit) {
            v = v * 10 + (c - b'0') as u32;
            self.i += 1;
            if v > max {
                return Err(TzError);
            }
        }
        if self.i == start { Err(TzError) } else { Ok(v) }
    }

    /// `[+-]hh[:mm[:ss]]` in seconds.
    fn hms(&mut self, max_h: u32) -> Result<i32, TzError> {
        let neg = if self.eat(b'-') {
            true
        } else {
            self.eat(b'+');
            false
        };
        let mut s = self.number(max_h)? as i32 * 3600;
        if self.eat(b':') {
            s += self.number(59)? as i32 * 60;
            if self.eat(b':') {
                s += self.number(59)? as i32;
            }
        }
        Ok(if neg { -s } else { s })
    }

    fn rule(&mut self) -> Result<Rule, TzError> {
        if !self.eat(b'M') {
            return Err(TzError);
        }
        let month = self.number(12)?;
        if month == 0 || !self.eat(b'.') {
            return Err(TzError);
        }
        let week = self.number(5)?;
        if week == 0 || !self.eat(b'.') {
            return Err(TzError);
        }
        let weekday = self.number(6)?;
        let time_s = if self.eat(b'/') {
            self.hms(167)?
        } else {
            2 * 3600
        };
        Ok(Rule {
            month,
            week,
            weekday,
            time_s,
        })
    }
}

impl TimeZone {
    pub const UTC: TimeZone = TimeZone {
        std_offset_s: 0,
        dst: None,
    };

    pub fn parse(s: &str) -> Result<Self, TzError> {
        let mut p = Parser {
            s: s.as_bytes(),
            i: 0,
        };
        p.name()?;
        // POSIX offsets are west-positive: "EST5" is UTC-5.
        let std_offset_s = -p.hms(24)?;
        let mut tz = TimeZone {
            std_offset_s,
            dst: None,
        };
        if p.peek().is_some() {
            p.name()?;
            let offset_s = if p.peek().is_some_and(|c| c != b',') {
                -p.hms(24)?
            } else {
                std_offset_s + 3600
            };
            if !p.eat(b',') {
                return Err(TzError); // DST without rules: not supported
            }
            let start = p.rule()?;
            if !p.eat(b',') {
                return Err(TzError);
            }
            let end = p.rule()?;
            tz.dst = Some(Dst {
                offset_s,
                start,
                end,
            });
        }
        if p.peek().is_some() {
            return Err(TzError);
        }
        Ok(tz)
    }

    /// UTC offset (seconds east) and whether DST applies at `unix_s`.
    pub fn offset_at(&self, unix_s: i64) -> (i32, bool) {
        let Some(dst) = self.dst else {
            return (self.std_offset_s, false);
        };
        let (year, _, _) = civil_from_days((unix_s + self.std_offset_s as i64).div_euclid(86_400));
        // Transition instants in UTC: the start rule is in standard time,
        // the end rule in daylight time.
        let start =
            dst.start.day(year) * 86_400 + dst.start.time_s as i64 - self.std_offset_s as i64;
        let end = dst.end.day(year) * 86_400 + dst.end.time_s as i64 - dst.offset_s as i64;
        let in_dst = if start < end {
            unix_s >= start && unix_s < end
        } else {
            // Southern hemisphere: DST spans the new year.
            unix_s >= start || unix_s < end
        };
        if in_dst {
            (dst.offset_s, true)
        } else {
            (self.std_offset_s, false)
        }
    }

    pub fn local(&self, unix_s: i64) -> DateTime {
        let (offset, dst) = self.offset_at(unix_s);
        DateTime::from_unix(unix_s, offset, dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix(y: i32, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60
    }

    #[test]
    fn civil_roundtrip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        for z in [-1_000_000i64, -1, 0, 1, 10_957, 19_000, 24_855, 1_000_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(weekday_from_days(0), 4); // Thursday
        assert_eq!(weekday_from_days(days_from_civil(2026, 10, 21)), 3); // Wednesday
    }

    #[test]
    fn formats_rfc3339() {
        let t = DateTime::from_unix(unix(2026, 10, 21, 20, 40) + 12, 3600, true);
        assert_eq!(std::format!("{t}"), "2026-10-21T21:40:12+01:00");
        assert_eq!(t.minute_of_day(), 21 * 60 + 40);
        let t = DateTime::from_unix(0, -5 * 3600 - 1800, false);
        assert_eq!(std::format!("{t}"), "1969-12-31T18:30:00-05:30");
    }

    #[test]
    fn uk_time() {
        let tz = TimeZone::parse("GMT0BST,M3.5.0/1,M10.5.0").unwrap();
        // 2026: BST from Sun 29 March 01:00 UTC to Sun 25 October 01:00 UTC.
        assert_eq!(tz.offset_at(unix(2026, 3, 29, 0, 59)), (0, false));
        assert_eq!(tz.offset_at(unix(2026, 3, 29, 1, 0)), (3600, true));
        assert_eq!(tz.offset_at(unix(2026, 10, 25, 0, 59)), (3600, true));
        assert_eq!(tz.offset_at(unix(2026, 10, 25, 1, 0)), (0, false));
        assert_eq!(
            std::format!("{}", tz.local(unix(2026, 7, 1, 12, 0))),
            "2026-07-01T13:00:00+01:00"
        );
    }

    #[test]
    fn us_eastern_and_southern_hemisphere() {
        let ny = TimeZone::parse("EST5EDT,M3.2.0,M11.1.0").unwrap();
        // 2026: 8 March 02:00 EST (07:00 UTC) .. 1 November 02:00 EDT (06:00 UTC).
        assert_eq!(ny.offset_at(unix(2026, 3, 8, 6, 59)), (-5 * 3600, false));
        assert_eq!(ny.offset_at(unix(2026, 3, 8, 7, 0)), (-4 * 3600, true));
        assert_eq!(ny.offset_at(unix(2026, 11, 1, 6, 0)), (-5 * 3600, false));

        let syd = TimeZone::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        // DST from 4 October 2026 02:00 AEST (3 Oct 16:00 UTC) to 5 April 2026 03:00 AEDT.
        assert_eq!(syd.offset_at(unix(2026, 1, 15, 0, 0)), (11 * 3600, true));
        assert_eq!(syd.offset_at(unix(2026, 4, 4, 15, 59)), (11 * 3600, true));
        assert_eq!(syd.offset_at(unix(2026, 4, 4, 16, 0)), (10 * 3600, false));
        assert_eq!(syd.offset_at(unix(2026, 10, 3, 16, 0)), (11 * 3600, true));
    }

    #[test]
    fn fixed_and_quoted_zones() {
        assert_eq!(
            TimeZone::parse("UTC0").unwrap().offset_at(1_000),
            (0, false)
        );
        assert_eq!(
            TimeZone::parse("<+0530>-5:30").unwrap().offset_at(0),
            (5 * 3600 + 1800, false)
        );
        assert_eq!(
            TimeZone::parse("CET-1CEST,M3.5.0,M10.5.0/3")
                .unwrap()
                .offset_at(unix(2026, 7, 1, 0, 0)),
            (7200, true)
        );
        for bad in [
            "",
            "U0",
            "UTC",
            "GMT0BST",
            "GMT0BST,M3.5.0",
            "GMT0BST,M13.5.0,M10.5.0",
            "GMT0BST,J60,J300",
            "UTC0x",
            "UTC99",
        ] {
            assert_eq!(TimeZone::parse(bad), Err(TzError), "{bad}");
        }
    }

    #[test]
    fn last_week_rule_in_short_month() {
        // Last Sunday of February 2026 is the 22nd.
        let r = Rule {
            month: 2,
            week: 5,
            weekday: 0,
            time_s: 0,
        };
        assert_eq!(civil_from_days(r.day(2026)), (2026, 2, 22));
    }

    #[test]
    fn wall_clock() {
        let mut c = WallClock::new();
        assert_eq!(c.unix_s(5), None);
        c.set(1_700_000_000_000_000, 10_000_000);
        assert_eq!(c.unix_s(12_500_000), Some(1_700_000_002));
        assert_eq!(c.since_sync_s(70_000_000), Some(60));
    }
}
