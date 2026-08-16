//! Quota periods and consumption outcomes. The period is the calendar
//! month: usage resets at the first of each month; a worker job rolls old
//! periods over (keeps the last `N` months for reporting).

use chrono::{DateTime, Datelike, NaiveDate, Utc};

/// The usage period a timestamp belongs to (first day of the month).
pub fn period_start(now: DateTime<Utc>) -> NaiveDate {
    NaiveDate::from_ymd_opt(now.year(), now.month(), 1).expect("calendar invariants hold")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_starts_on_the_first_of_the_month() {
        let now: DateTime<Utc> = "2026-08-16T12:00:00Z".parse().unwrap();
        assert_eq!(period_start(now).to_string(), "2026-08-01");

        let new_year: DateTime<Utc> = "2027-01-01T00:00:01Z".parse().unwrap();
        assert_eq!(period_start(new_year).to_string(), "2027-01-01");
    }

    #[test]
    fn distinct_months_get_distinct_periods() {
        let august: DateTime<Utc> = "2026-08-31T23:59:59Z".parse().unwrap();
        let september: DateTime<Utc> = "2026-09-01T00:00:00Z".parse().unwrap();
        assert_ne!(period_start(august), period_start(september));
    }
}
