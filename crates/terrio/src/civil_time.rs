//! Civil UTC dates shared by HTTP retry dates and directory listings.

/// The seconds since the Unix epoch of a UTC date and time.
pub(crate) fn unix_seconds(y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    // Days from the civil date (Howard Hinnant's days_from_civil).
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    days * 86_400 + h * 3600 + mi * 60 + s
}

/// Validates an untrusted HTTP calendar date before integer conversion.
pub(crate) fn checked_unix_seconds(y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64) -> Option<i64> {
    if !(1..=9999).contains(&y) || !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=60).contains(&s) {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if !(1..=days).contains(&d) {
        return None;
    }
    Some(unix_seconds(y, m, d, h, mi, s))
}
