//! Date parsing for `YYYY-MM-DD` and `YYYY-MM-DDTHH:MM:SS[.fff]Z`, to UTC epoch
//! milliseconds inside the engine's four-digit-year domain.

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn number(text: &str, digits: usize) -> Option<i64> {
    if text.len() != digits || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Parse an ISO-8601 UTC date or date-time to epoch milliseconds.
pub(crate) fn parse_date(text: &str) -> Option<i64> {
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let mut parts = date.split('-');
    let (y, m, d) = (
        number(parts.next()?, 4)?,
        number(parts.next()?, 2)?,
        number(parts.next()?, 2)?,
    );
    if parts.next().is_some() || !(1..=12).contains(&m) {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days_in_month).contains(&d) {
        return None;
    }
    let mut millis = days_from_civil(y, m, d) * 86_400_000;
    if let Some(time) = time {
        let time = time.strip_suffix('Z')?;
        let (clock, fraction) = match time.split_once('.') {
            Some((clock, fraction)) => (clock, Some(fraction)),
            None => (time, None),
        };
        let mut parts = clock.split(':');
        let (h, mi, s) = (
            number(parts.next()?, 2)?,
            number(parts.next()?, 2)?,
            number(parts.next()?, 2)?,
        );
        if parts.next().is_some() || h > 23 || mi > 59 || s > 59 {
            return None;
        }
        millis += ((h * 60 + mi) * 60 + s) * 1000;
        if let Some(fraction) = fraction {
            if fraction.is_empty()
                || fraction.len() > 3
                || !fraction.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            millis += fraction.parse::<i64>().ok()? * 10_i64.pow(3 - fraction.len() as u32);
        }
    }
    Some(millis)
}

#[cfg(test)]
mod tests {
    use super::parse_date;

    #[test]
    fn dates_parse_to_known_epoch_milliseconds() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("1970-01-02"), Some(86_400_000));
        assert_eq!(parse_date("2026-01-01T00:00:00Z"), Some(1_767_225_600_000));
        assert_eq!(
            parse_date("2026-01-01T00:00:00.5Z"),
            Some(1_767_225_600_500)
        );
        assert_eq!(parse_date("2000-02-29"), Some(951_782_400_000));
        assert_eq!(parse_date("1969-12-31"), Some(-86_400_000));
    }

    #[test]
    fn malformed_and_impossible_dates_are_refused() {
        for bad in [
            "",
            "2026",
            "2026-1-01",
            "2026-13-01",
            "2026-00-10",
            "2026-02-30",
            "2100-02-29",
            "2026-01-01T",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T00:00:00",
            "2026-01-01T00:00Z",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:00.1234Z",
            "2026-01-01-01",
            "abcd-01-01",
            "2026-01-01T00:00:00:00Z",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
        assert!(parse_date("2024-02-29").is_some());
    }
}
