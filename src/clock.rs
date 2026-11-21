//! Time: sample stamps, local time and the d/M/yyyy labels.
//!
//! Every sample carries MONOTONIC, BOOTTIME and REALTIME together with the local UTC offset in
//! force at that instant. Labels, day keys and stored stamps are then pure functions of the
//! sample: tests build whole timelines at chosen times without touching the process's time zone,
//! and a DST change moves only the samples after it.

use serde::{Deserialize, Serialize};

const NS: i64 = 1_000_000_000;

unsafe extern "C" {
    // POSIX; the libc crate binds it only for Windows.
    fn tzset();
}

/// One instant on all three clocks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moment {
    pub mono_ns: i64,
    pub boot_ns: i64,
    pub real_ns: i64,
    /// Local offset from UTC at `real_ns`, in seconds (localtime_r's tm_gmtoff).
    pub offset_s: i32,
}

impl Moment {
    pub fn now() -> Moment {
        let mono_ns = clock_ns(libc::CLOCK_MONOTONIC);
        let boot_ns = clock_ns(libc::CLOCK_BOOTTIME);
        let real_ns = clock_ns(libc::CLOCK_REALTIME);
        Moment {
            mono_ns,
            boot_ns,
            real_ns,
            offset_s: local_offset(real_ns.div_euclid(NS)),
        }
    }

    pub fn mono_s(&self) -> f64 {
        self.mono_ns as f64 / 1e9
    }

    pub fn unix_s(&self) -> i64 {
        self.real_ns.div_euclid(NS)
    }

    /// Time spent suspended since boot: BOOTTIME counts it and MONOTONIC does not.
    pub fn asleep_ns(&self) -> i64 {
        self.boot_ns - self.mono_ns
    }

    pub fn civil(&self) -> Civil {
        Civil::at(self.unix_s(), self.offset_s)
    }

    /// The local calendar day.
    pub fn date(&self) -> Date {
        self.civil().date
    }

    pub fn stamp(&self) -> Stamp {
        Stamp::at(self.unix_s(), self.offset_s)
    }

    /// `d/M/yyyy HH:mm`, local.
    pub fn label(&self) -> String {
        self.civil().label()
    }

    /// `yyyy-mm-dd`, local.
    pub fn day_key(&self) -> String {
        self.date().key()
    }

    /// The same clocks `secs` later, with no suspend in between.
    pub fn plus(&self, secs: f64) -> Moment {
        let ns = (secs * 1e9).round() as i64;
        Moment {
            mono_ns: self.mono_ns + ns,
            boot_ns: self.boot_ns + ns,
            real_ns: self.real_ns + ns,
            offset_s: self.offset_s,
        }
    }
}

fn clock_ns(id: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer, and these three clocks exist on every Linux it runs on.
    unsafe { libc::clock_gettime(id, &mut ts) };
    ts.tv_sec * NS + ts.tv_nsec
}

/// The local UTC offset at `unix_s`, from the system zone.
pub fn local_offset(unix_s: i64) -> i32 {
    let t = unix_s as libc::time_t;
    // SAFETY: an all-zero `tm` is a valid value, and both pointers are valid for the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = !unsafe { libc::localtime_r(&t, &mut tm) }.is_null();
    if ok { tm.tm_gmtoff as i32 } else { 0 }
}

/// Re-reads TZ and /etc/localtime. glibc's localtime_r loads the zone only once, so a long-lived
/// service calls this now and then to follow a zone change.
pub fn reload_zone() {
    // SAFETY: no arguments; the service is single-threaded, so nothing reads the zone meanwhile.
    unsafe { tzset() };
}

/// A calendar day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    pub y: i32,
    pub m: u32,
    pub d: u32,
}

impl Date {
    pub fn new(y: i32, m: u32, d: u32) -> Option<Date> {
        if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
            return None;
        }
        Some(Date { y, m, d })
    }

    /// Days since 1970-01-01.
    pub fn days(&self) -> i64 {
        days_from_civil(self.y as i64, self.m, self.d)
    }

    pub fn from_days(days: i64) -> Date {
        let (y, m, d) = civil_from_days(days);
        Date { y: y as i32, m, d }
    }

    pub fn add_days(&self, n: i64) -> Date {
        Date::from_days(self.days() + n)
    }

    /// `yyyy-mm-dd`, the day key used in state.json and daily.jsonl.
    pub fn key(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.y, self.m, self.d)
    }

    /// `yyyy-mm`, for the records archive.
    pub fn month_key(&self) -> String {
        format!("{:04}-{:02}", self.y, self.m)
    }

    /// `d/M/yyyy`, no leading zeros.
    pub fn label(&self) -> String {
        format!("{}/{}/{}", self.d, self.m, self.y)
    }

    pub fn parse_key(s: &str) -> Option<Date> {
        let s = s.trim();
        let b = s.as_bytes();
        if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        Date::new(
            digits(s.get(0..4)?)? as i32,
            digits(s.get(5..7)?)?,
            digits(s.get(8..10)?)?,
        )
    }

    /// Parses `d/M/yyyy`, the only date form the CLI accepts.
    pub fn parse_dmy(s: &str) -> Option<Date> {
        let mut parts = s.trim().split('/');
        let (d, m, y) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || !(1..=2).contains(&d.len()) || !(1..=2).contains(&m.len()) {
            return None;
        }
        if y.len() != 4 {
            return None;
        }
        Date::new(digits(y)? as i32, digits(m)?, digits(d)?)
    }
}

fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's civil-date algorithms: exact for the proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// A wall-clock time in some zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub date: Date,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Civil {
    pub fn at(unix_s: i64, offset_s: i32) -> Civil {
        let t = unix_s + offset_s as i64;
        let sod = t.rem_euclid(86_400) as u32;
        Civil {
            date: Date::from_days(t.div_euclid(86_400)),
            hour: sod / 3600,
            minute: sod / 60 % 60,
            second: sod % 60,
        }
    }

    /// `d/M/yyyy HH:mm`.
    pub fn label(&self) -> String {
        format!("{} {:02}:{:02}", self.date.label(), self.hour, self.minute)
    }

    fn iso(&self) -> String {
        format!(
            "{}T{:02}:{:02}:{:02}",
            self.date.key(),
            self.hour,
            self.minute,
            self.second
        )
    }

    fn parse_iso(s: &str) -> Option<Civil> {
        let b = s.as_bytes();
        if b.len() < 19 || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
            return None;
        }
        // get(), not indexing: a stray multi-byte character must not panic a reader.
        let (hour, minute, second) = (
            digits(s.get(11..13)?)?,
            digits(s.get(14..16)?)?,
            digits(s.get(17..19)?)?,
        );
        if hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        Some(Civil {
            date: Date::parse_key(s.get(0..10)?)?,
            hour,
            minute,
            second,
        })
    }

    fn seconds_of_day(&self) -> i64 {
        (self.hour * 3600 + self.minute * 60 + self.second) as i64
    }
}

/// A stored time: `{"utc":"2026-12-03T14:05:10Z","local":"2026-12-03T14:05:10+00:00"}` (§11.2).
/// Both forms are kept because session logs are UTC and her clock is local; mixing the two has
/// produced phantom timelines before.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stamp {
    pub utc: String,
    pub local: String,
}

impl Stamp {
    pub fn at(unix_s: i64, offset_s: i32) -> Stamp {
        let sign = if offset_s < 0 { '-' } else { '+' };
        let off = offset_s.unsigned_abs();
        Stamp {
            utc: format!("{}Z", Civil::at(unix_s, 0).iso()),
            local: format!(
                "{}{sign}{:02}:{:02}",
                Civil::at(unix_s, offset_s).iso(),
                off / 3600,
                off / 60 % 60
            ),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.utc.is_empty() && self.local.is_empty()
    }

    /// Seconds since the epoch, from the UTC form.
    pub fn unix(&self) -> Option<i64> {
        let c = Civil::parse_iso(&self.utc)?;
        Some(c.date.days() * 86_400 + c.seconds_of_day())
    }

    /// The local wall time as she saw it.
    pub fn local_civil(&self) -> Option<Civil> {
        Civil::parse_iso(&self.local)
    }

    /// `d/M/yyyy HH:mm`, or "" for an empty or malformed stamp.
    pub fn label(&self) -> String {
        self.local_civil().map(|c| c.label()).unwrap_or_default()
    }

    /// `d/M/yyyy`, or "".
    pub fn date_label(&self) -> String {
        self.local_civil()
            .map(|c| c.date.label())
            .unwrap_or_default()
    }

    /// The local day, `yyyy-mm-dd`, or "".
    pub fn day_key(&self) -> String {
        self.local_civil().map(|c| c.date.key()).unwrap_or_default()
    }
}

/// Synthetic clocks for tests and replays: a timeline starting at a chosen local time, advanced
/// by hand, with suspends that move BOOTTIME and REALTIME but not MONOTONIC.
#[derive(Clone, Debug)]
pub struct Timeline {
    now: Moment,
}

impl Timeline {
    /// Starts at `date hour:minute:second` local time, with the given UTC offset.
    pub fn starting(date: Date, hour: u32, minute: u32, second: u32, offset_s: i32) -> Timeline {
        let unix =
            date.days() * 86_400 + (hour * 3600 + minute * 60 + second) as i64 - offset_s as i64;
        Timeline {
            now: Moment {
                // An arbitrary uptime, so nothing mistakes the start for a fresh boot.
                mono_ns: 1000 * NS,
                boot_ns: 1000 * NS,
                real_ns: unix * NS,
                offset_s,
            },
        }
    }

    pub fn now(&self) -> Moment {
        self.now
    }

    /// Moves all three clocks on by `secs`.
    pub fn advance(&mut self, secs: f64) -> Moment {
        self.now = self.now.plus(secs);
        self.now
    }

    /// A suspend of `secs`: BOOTTIME and REALTIME move on, MONOTONIC stands still.
    pub fn suspend(&mut self, secs: f64) -> Moment {
        let ns = (secs * 1e9).round() as i64;
        self.now.boot_ns += ns;
        self.now.real_ns += ns;
        self.now
    }

    /// The wall clock set by `secs` (a mistyped `date`, a time server's answer): REALTIME moves,
    /// MONOTONIC and BOOTTIME do not.
    pub fn step_wall(&mut self, secs: f64) -> Moment {
        self.now.real_ns += (secs * 1e9).round() as i64;
        self.now
    }

    /// A zone change, such as a DST transition.
    pub fn set_offset(&mut self, offset_s: i32) {
        self.now.offset_s = offset_s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for days in (-800_000..800_000).step_by(997) {
            let d = Date::from_days(days);
            assert_eq!(d.days(), days, "{d:?}");
            assert!(Date::new(d.y, d.m, d.d).is_some());
        }
        assert_eq!(Date::new(1970, 1, 1).unwrap().days(), 0);
        assert_eq!(Date::new(2026, 11, 20).unwrap().days(), 20_777);
        let leap = Date::new(2024, 2, 29).unwrap();
        assert_eq!(leap.add_days(365), Date::new(2025, 2, 28).unwrap());
        assert_eq!(leap.add_days(366), Date::new(2025, 3, 1).unwrap());
        assert_eq!(leap.add_days(-60).label(), "31/12/2023");
    }

    #[test]
    fn stamp_forms() {
        // 3/12/2026 14:05:10 UTC.
        let unix = Date::new(2026, 12, 3).unwrap().days() * 86_400 + 14 * 3600 + 5 * 60 + 10;
        let s = Stamp::at(unix, 0);
        assert_eq!(s.utc, "2026-12-03T14:05:10Z");
        assert_eq!(s.local, "2026-12-03T14:05:10+00:00");
        assert_eq!(s.label(), "3/12/2026 14:05");
        assert_eq!(s.date_label(), "3/12/2026");
        assert_eq!(s.unix(), Some(unix));

        let s = Stamp::at(unix, 3600);
        assert_eq!(s.local, "2026-12-03T15:05:10+01:00");
        assert_eq!(s.label(), "3/12/2026 15:05");

        // West of UTC, across midnight: the local day is the day before.
        let s = Stamp::at(unix - 14 * 3600, -5 * 3600 - 30 * 60);
        assert_eq!(s.local, "2026-12-02T18:35:10-05:30");
        assert_eq!(s.day_key(), "2026-12-02");
        assert_eq!(s.utc, "2026-12-03T00:05:10Z");

        assert_eq!(Stamp::default().label(), "");
        assert_eq!(
            Stamp {
                utc: "junk".into(),
                local: "junk".into()
            }
            .unix(),
            None
        );
    }

    #[test]
    fn labels_drop_leading_zeros() {
        let m = Timeline::starting(Date::new(2026, 1, 5).unwrap(), 9, 7, 0, 0).now();
        assert_eq!(m.label(), "5/1/2026 09:07");
        assert_eq!(m.date().label(), "5/1/2026");
        assert_eq!(m.day_key(), "2026-01-05");
        assert_eq!(m.date().month_key(), "2026-01");
    }

    #[test]
    fn parse_dates() {
        assert_eq!(Date::parse_dmy("3/12/2026"), Date::new(2026, 12, 3));
        assert_eq!(Date::parse_dmy("20/11/2026"), Date::new(2026, 11, 20));
        assert_eq!(Date::parse_dmy("29/2/2024"), Date::new(2024, 2, 29));
        for bad in [
            "29/2/2026",
            "2026-11-20",
            "1/13/2026",
            "0/1/2026",
            "1/1/26",
            "1/1/2026/1",
            "",
            "a/b/cccc",
        ] {
            assert_eq!(Date::parse_dmy(bad), None, "{bad}");
        }
        assert_eq!(Date::parse_key("2026-12-03"), Date::new(2026, 12, 3));
        assert_eq!(Date::parse_key("2026-12-3"), None);
    }

    #[test]
    fn timeline_suspend_moves_boot_not_mono() {
        let mut t = Timeline::starting(Date::new(2026, 11, 20).unwrap(), 23, 59, 30, 0);
        let a = t.now();
        let b = t.advance(1.0);
        assert_eq!(b.mono_ns - a.mono_ns, NS);
        assert_eq!(b.asleep_ns(), a.asleep_ns());
        let c = t.suspend(600.0);
        assert_eq!(c.mono_ns, b.mono_ns);
        assert_eq!(c.asleep_ns() - b.asleep_ns(), 600 * NS);
        assert_eq!(c.day_key(), "2026-11-21");
    }

    // Only the raw clocks: `Moment::now` also consults the process's zone (TZ), which tests leave
    // alone.
    #[test]
    fn real_clocks_read() {
        let mono = clock_ns(libc::CLOCK_MONOTONIC);
        let boot = clock_ns(libc::CLOCK_BOOTTIME);
        assert!(boot >= mono);
        assert!(clock_ns(libc::CLOCK_REALTIME) > 1_700_000_000 * NS);
    }
}
