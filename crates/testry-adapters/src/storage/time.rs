//! UTC timestamps for display. Durations are measured with a monotonic clock
//! elsewhere; these wall-clock values are never used to compute a duration.

use std::time::{SystemTime, UNIX_EPOCH};

/// Formats a point in time as `YYYY-MM-DDTHH:MM:SSZ`. Times before the Unix
/// epoch are clamped to the epoch.
pub fn rfc3339_utc(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60
    )
}

/// Converts days since 1970-01-01 to a proleptic Gregorian date
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(seconds: u64) -> String {
        rfc3339_utc(UNIX_EPOCH + Duration::from_secs(seconds))
    }

    #[test]
    fn formats_known_instants() {
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(at(1_709_251_200), "2024-03-01T00:00:00Z");
        assert_eq!(at(1_794_906_000), "2026-11-17T09:00:00Z");
        assert_eq!(at(4_102_444_800), "2100-01-01T00:00:00Z");
        assert_eq!(
            at(4_107_542_400),
            "2100-03-01T00:00:00Z",
            "2100 is not a leap year"
        );
    }

    #[test]
    fn times_before_the_epoch_are_clamped() {
        assert_eq!(
            rfc3339_utc(UNIX_EPOCH - Duration::from_secs(10)),
            "1970-01-01T00:00:00Z"
        );
    }
}
