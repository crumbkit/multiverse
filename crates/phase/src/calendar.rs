//! Converts between days since 1970-01-01 and dates of the proleptic
//! Gregorian calendar, in UTC, with Howard Hinnant's algorithms. phase reads
//! docker's timestamps with them, and [`civil_from_days`] is public so that
//! a caller can name a day.

/// Returns the days from 1970-01-01 to the proleptic Gregorian date
/// `year`-`month`-`day`, with Howard Hinnant's `days_from_civil`. A date
/// before 1970 gives a negative number.
#[must_use]
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_from_march = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Returns the year, month and day of the proleptic Gregorian date `days`
/// after 1970-01-01, with Howard Hinnant's `civil_from_days`. A negative
/// `days` gives a date before 1970.
#[must_use]
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, days_from_civil};

    #[test]
    fn days_since_the_epoch_give_the_calendar_date() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_394), (2025, 11, 2));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn a_date_and_its_days_convert_both_ways() {
        for days in [-719_468, -1, 0, 59, 11_016, 20_394, 2_932_896] {
            let (year, month, day) = civil_from_days(days);
            let month = u32::try_from(month).unwrap();
            let day = u32::try_from(day).unwrap();
            assert_eq!(
                days_from_civil(year, month, day),
                days,
                "{year}-{month}-{day}"
            );
        }
    }
}
