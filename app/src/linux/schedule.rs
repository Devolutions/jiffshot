//! Local-time scheduling for a daily capture while the application is open.

use gstreamer::glib;
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ScheduleTime {
    hour: u8,
    minute: u8,
}

impl FromStr for ScheduleTime {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        if bytes.len() != 5
            || bytes[2] != b':'
            || ![bytes[0], bytes[1], bytes[3], bytes[4]]
                .iter()
                .all(u8::is_ascii_digit)
        {
            return Err("the daily capture time must use HH:MM".to_owned());
        }
        let hour = (bytes[0] - b'0') * 10 + (bytes[1] - b'0');
        let minute = (bytes[3] - b'0') * 10 + (bytes[4] - b'0');
        if hour > 23 || minute > 59 {
            return Err("the daily capture time must be between 00:00 and 23:59".to_owned());
        }
        Ok(Self { hour, minute })
    }
}

impl fmt::Display for ScheduleTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:02}:{:02}", self.hour, self.minute)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ScheduleDay {
    year: u16,
    day_of_year: u16,
}

impl ScheduleDay {
    fn new(year: u16, day_of_year: u16) -> Result<Self, String> {
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        if year == 0
            || year > 9999
            || day_of_year == 0
            || day_of_year > if leap { 366 } else { 365 }
        {
            return Err("the last daily capture date is invalid".to_owned());
        }
        Ok(Self { year, day_of_year })
    }
}

impl FromStr for ScheduleDay {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        if bytes.len() != 8
            || bytes[4] != b'-'
            || !bytes[..4].iter().chain(&bytes[5..]).all(u8::is_ascii_digit)
        {
            return Err("the last daily capture date is invalid".to_owned());
        }
        let year = value[..4]
            .parse()
            .map_err(|_| "the last daily capture date is invalid".to_owned())?;
        let day_of_year = value[5..]
            .parse()
            .map_err(|_| "the last daily capture date is invalid".to_owned())?;
        Self::new(year, day_of_year)
    }
}

impl fmt::Display for ScheduleDay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:04}-{:03}", self.year, self.day_of_year)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LocalMinute {
    day: ScheduleDay,
    time: ScheduleTime,
}

impl LocalMinute {
    pub(super) fn now() -> Result<Self, String> {
        let now = glib::DateTime::now_local()
            .map_err(|error| format!("could not read local time: {error}"))?;
        let year =
            u16::try_from(now.year()).map_err(|_| "the local year is out of range".to_owned())?;
        let day = u16::try_from(now.day_of_year())
            .map_err(|_| "the local day is out of range".to_owned())?;
        let hour =
            u8::try_from(now.hour()).map_err(|_| "the local hour is out of range".to_owned())?;
        let minute = u8::try_from(now.minute())
            .map_err(|_| "the local minute is out of range".to_owned())?;
        Ok(Self {
            day: ScheduleDay::new(year, day)?,
            time: ScheduleTime { hour, minute },
        })
    }

    pub(super) fn day(self) -> ScheduleDay {
        self.day
    }
}

pub(super) struct DailySchedule {
    time: ScheduleTime,
    last_fired: Option<ScheduleDay>,
}

impl DailySchedule {
    pub(super) fn new(time: ScheduleTime, last_fired: Option<ScheduleDay>) -> Self {
        Self { time, last_fired }
    }

    pub(super) fn should_fire(&mut self, now: LocalMinute) -> bool {
        if now.time != self.time || self.last_fired.is_some_and(|last| now.day <= last) {
            return false;
        }
        self.last_fired = Some(now.day);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moment(day: &str, time: &str) -> LocalMinute {
        LocalMinute {
            day: day.parse().expect("test day should be valid"),
            time: time.parse().expect("test time should be valid"),
        }
    }

    #[test]
    fn schedule_time_accepts_exact_day_boundaries_and_rejects_invalid_values() {
        for value in ["00:00", "09:07", "23:59"] {
            assert_eq!(value.parse::<ScheduleTime>().unwrap().to_string(), value);
        }
        for value in [
            "", "9:07", "09:7", "24:00", "23:60", "99:99", "ab:cd", "09:00 ",
        ] {
            assert!(
                value.parse::<ScheduleTime>().is_err(),
                "{value:?} must fail"
            );
        }
    }

    #[test]
    fn schedule_day_validates_leap_years_and_round_trips() {
        for value in ["2024-366", "2000-366", "2025-365", "9999-001"] {
            assert_eq!(value.parse::<ScheduleDay>().unwrap().to_string(), value);
        }
        for value in [
            "2025-366", "1900-366", "0000-001", "2024-000", "2024-367", "2024-1",
        ] {
            assert!(value.parse::<ScheduleDay>().is_err(), "{value:?} must fail");
        }
    }

    #[test]
    fn daily_schedule_fires_once_at_the_target_minute_and_on_later_days() {
        let mut schedule = DailySchedule::new("09:00".parse().unwrap(), None);
        assert!(!schedule.should_fire(moment("2025-365", "08:59")));
        assert!(schedule.should_fire(moment("2025-365", "09:00")));
        assert!(!schedule.should_fire(moment("2025-365", "09:00")));
        assert!(!schedule.should_fire(moment("2025-365", "09:01")));
        assert!(!schedule.should_fire(moment("2025-364", "09:00")));
        assert!(schedule.should_fire(moment("2026-001", "09:00")));
        assert!(!schedule.should_fire(moment("2026-001", "09:00")));
    }

    #[test]
    fn saved_last_fire_prevents_a_duplicate_after_restart() {
        let mut schedule =
            DailySchedule::new("01:30".parse().unwrap(), Some("2025-300".parse().unwrap()));
        assert!(!schedule.should_fire(moment("2025-300", "01:30")));
        assert!(schedule.should_fire(moment("2025-301", "01:30")));
    }

    #[test]
    fn system_local_time_produces_a_valid_schedule_date_and_time() {
        let now = LocalMinute::now().expect("the system local time should be available");
        assert_eq!(now.day.to_string().parse::<ScheduleDay>().unwrap(), now.day);
        assert_eq!(
            now.time.to_string().parse::<ScheduleTime>().unwrap(),
            now.time
        );
    }
}
