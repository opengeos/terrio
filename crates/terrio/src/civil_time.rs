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
