//! NSE session clock: IST session phases, weekends and exchange holidays.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{Datelike, FixedOffset, NaiveDate, NaiveTime, TimeZone, Weekday};
use serde::Deserialize;

use crate::error::{CoreError, CoreResult};
use crate::types::Ts;

/// India Standard Time (UTC+05:30, no DST).
pub const IST: FixedOffset = match FixedOffset::east_opt(5 * 3600 + 30 * 60) {
    Some(tz) => tz,
    None => panic!("valid offset"),
};

/// Where the trading day is, from the engine's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 09:00–09:15: pre-open auction; no continuous trading yet.
    PreOpen,
    /// Normal trading; new entries allowed.
    Open,
    /// Still in market hours but no new entries (default 14:45 onward).
    NoNewEntries,
    /// Square-off window: flatten everything (default 15:10 onward).
    SquareOff,
    /// Outside market hours, weekend or holiday.
    Closed,
}

/// Session boundaries in IST wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTimes {
    pub pre_open: NaiveTime,
    pub open: NaiveTime,
    pub no_new_entries: NaiveTime,
    pub square_off: NaiveTime,
    pub close: NaiveTime,
}

impl Default for SessionTimes {
    fn default() -> Self {
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).expect("valid time");
        SessionTimes {
            pre_open: t(9, 0),
            open: t(9, 15),
            no_new_entries: t(14, 45),
            square_off: t(15, 10),
            close: t(15, 30),
        }
    }
}

impl SessionTimes {
    /// Overrides the engine-controlled cut-offs (from `risk.toml`), keeping
    /// exchange times fixed. Both must fall inside the session and in order.
    pub fn with_cutoffs(self, no_new_entries: &str, square_off: &str) -> CoreResult<Self> {
        let no_new_entries = parse_hhmm(no_new_entries)?;
        let square_off = parse_hhmm(square_off)?;
        if !(self.open < no_new_entries && no_new_entries <= square_off && square_off < self.close) {
            return Err(CoreError::InvalidTime(format!(
                "cut-offs must satisfy open < no_new_entries <= square_off < close, got {no_new_entries} / {square_off}"
            )));
        }
        Ok(SessionTimes {
            no_new_entries,
            square_off,
            ..self
        })
    }
}

pub fn parse_hhmm(s: &str) -> CoreResult<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").map_err(|_| CoreError::InvalidTime(s.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Holiday {
    pub date: NaiveDate,
    pub name: String,
}

/// Exchange holidays, loaded yearly from `config/holidays.toml`.
///
/// The calendar records which years it covers. Asking about a year it does not
/// cover is an error rather than a guess, so a stale file makes the engine
/// refuse to start instead of trading on a holiday.
#[derive(Debug, Clone, Default)]
pub struct HolidayCalendar {
    years: Vec<i32>,
    holidays: BTreeMap<NaiveDate, String>,
}

#[derive(Deserialize)]
struct HolidayFile {
    years: Vec<i32>,
    #[serde(default)]
    holidays: Vec<Holiday>,
}

impl HolidayCalendar {
    pub fn new(years: Vec<i32>, holidays: impl IntoIterator<Item = Holiday>) -> CoreResult<Self> {
        let holidays: BTreeMap<_, _> = holidays.into_iter().map(|h| (h.date, h.name)).collect();
        if let Some(d) = holidays.keys().find(|d| !years.contains(&d.year())) {
            return Err(CoreError::Calendar(format!(
                "holiday {d} is outside the declared years {years:?}"
            )));
        }
        Ok(HolidayCalendar { years, holidays })
    }

    pub fn from_toml_str(s: &str) -> CoreResult<Self> {
        let f: HolidayFile = toml::from_str(s).map_err(|e| CoreError::Calendar(e.to_string()))?;
        Self::new(f.years, f.holidays)
    }

    pub fn load(path: &Path) -> CoreResult<Self> {
        let s = std::fs::read_to_string(path).map_err(|e| CoreError::Calendar(format!("{}: {e}", path.display())))?;
        Self::from_toml_str(&s)
    }

    pub fn covers(&self, year: i32) -> bool {
        self.years.contains(&year)
    }

    pub fn holiday(&self, date: NaiveDate) -> Option<&str> {
        self.holidays.get(&date).map(String::as_str)
    }
}

/// Answers "can we trade now, and in which phase?" for NSE cash.
#[derive(Debug, Clone)]
pub struct MarketClock {
    pub times: SessionTimes,
    pub calendar: HolidayCalendar,
}

impl MarketClock {
    pub fn new(times: SessionTimes, calendar: HolidayCalendar) -> Self {
        MarketClock { times, calendar }
    }

    /// Whether `date` is a regular trading session. Errors if the holiday
    /// calendar does not cover the year.
    pub fn is_trading_day(&self, date: NaiveDate) -> CoreResult<bool> {
        if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            return Ok(false);
        }
        if !self.calendar.covers(date.year()) {
            return Err(CoreError::Calendar(format!(
                "holiday calendar does not cover {}; update config/holidays.toml",
                date.year()
            )));
        }
        Ok(self.calendar.holiday(date).is_none())
    }

    pub fn ist_date(now: Ts) -> NaiveDate {
        now.with_timezone(&IST).date_naive()
    }

    pub fn phase(&self, now: Ts) -> CoreResult<Phase> {
        let local = now.with_timezone(&IST);
        if !self.is_trading_day(local.date_naive())? {
            return Ok(Phase::Closed);
        }
        let t = local.time();
        let s = &self.times;
        Ok(if t < s.pre_open || t >= s.close {
            Phase::Closed
        } else if t < s.open {
            Phase::PreOpen
        } else if t < s.no_new_entries {
            Phase::Open
        } else if t < s.square_off {
            Phase::NoNewEntries
        } else {
            Phase::SquareOff
        })
    }

    /// The IST instant for `time` on `date`.
    pub fn at(date: NaiveDate, time: NaiveTime) -> Ts {
        IST.from_local_datetime(&date.and_time(time))
            .single()
            .expect("IST has no DST gaps")
            .to_utc()
    }

    /// Next trading day strictly after `date`.
    pub fn next_trading_day(&self, date: NaiveDate) -> CoreResult<NaiveDate> {
        let mut d = date;
        for _ in 0..30 {
            d = d
                .succ_opt()
                .ok_or_else(|| CoreError::Calendar("date overflow".into()))?;
            if self.is_trading_day(d)? {
                return Ok(d);
            }
        }
        Err(CoreError::Calendar(format!(
            "no trading day within 30 days after {date}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLIDAYS: &str = r#"
        years = [2026]
        [[holidays]]
        date = "2026-01-26"
        name = "Republic Day"
        [[holidays]]
        date = "2026-10-02"
        name = "Gandhi Jayanti"
    "#;

    fn clock() -> MarketClock {
        MarketClock::new(
            SessionTimes::default(),
            HolidayCalendar::from_toml_str(HOLIDAYS).unwrap(),
        )
    }

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn at(date: &str, hhmm: &str) -> Ts {
        MarketClock::at(d(date), parse_hhmm(hhmm).unwrap())
    }

    #[test]
    fn phases_through_a_trading_day() {
        let c = clock();
        let day = "2026-10-27"; // Tuesday
        for (t, p) in [
            ("08:59", Phase::Closed),
            ("09:00", Phase::PreOpen),
            ("09:14", Phase::PreOpen),
            ("09:15", Phase::Open),
            ("14:44", Phase::Open),
            ("14:45", Phase::NoNewEntries),
            ("15:09", Phase::NoNewEntries),
            ("15:10", Phase::SquareOff),
            ("15:29", Phase::SquareOff),
            ("15:30", Phase::Closed),
        ] {
            assert_eq!(c.phase(at(day, t)).unwrap(), p, "{t}");
        }
    }

    #[test]
    fn weekends_and_holidays_are_closed() {
        let c = clock();
        assert_eq!(c.phase(at("2026-10-31", "10:00")).unwrap(), Phase::Closed); // Saturday
        assert_eq!(c.phase(at("2026-10-02", "10:00")).unwrap(), Phase::Closed); // holiday
        assert!(!c.is_trading_day(d("2026-01-26")).unwrap());
        assert!(c.is_trading_day(d("2026-01-27")).unwrap());
    }

    #[test]
    fn uncovered_year_is_an_error_not_a_guess() {
        let c = clock();
        assert!(c.is_trading_day(d("2027-01-05")).is_err());
        // Weekends never need the calendar.
        assert!(!c.is_trading_day(d("2027-01-02")).unwrap());
    }

    #[test]
    fn next_trading_day_skips_weekend_and_holiday() {
        let c = clock();
        assert_eq!(c.next_trading_day(d("2026-10-01")).unwrap(), d("2026-10-05"));
        assert_eq!(c.next_trading_day(d("2026-01-23")).unwrap(), d("2026-01-27"));
    }

    #[test]
    fn ist_conversion() {
        let ts = at("2026-10-27", "09:15");
        assert_eq!(ts.to_rfc3339(), "2026-10-27T03:45:00+00:00");
        assert_eq!(MarketClock::ist_date(ts), d("2026-10-27"));
    }

    #[test]
    fn custom_cutoffs_are_validated() {
        let s = SessionTimes::default();
        let s2 = s.with_cutoffs("14:30", "15:05").unwrap();
        assert_eq!(s2.no_new_entries, parse_hhmm("14:30").unwrap());
        assert!(s.with_cutoffs("15:20", "15:10").is_err());
        assert!(s.with_cutoffs("09:00", "15:10").is_err());
        assert!(s.with_cutoffs("14:45", "15:45").is_err());
    }

    #[test]
    fn calendar_rejects_holiday_outside_years() {
        let bad = r#"
            years = [2026]
            [[holidays]]
            date = "2027-01-26"
            name = "x"
        "#;
        assert!(HolidayCalendar::from_toml_str(bad).is_err());
    }

    #[test]
    fn shipped_holiday_file_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/holidays.toml");
        let cal = HolidayCalendar::load(Path::new(path)).unwrap();
        assert!(cal.covers(2026));
        assert_eq!(cal.holiday(d("2026-12-25")), Some("Christmas"));
    }
}
