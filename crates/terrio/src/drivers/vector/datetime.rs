//! Dates and date-times as text, the form of terrio's vector data model, converted to and from
//! the day and millisecond counts of GeoParquet, Arrow and GeoPackage.

/// `YYYY-MM-DD` from days since 1970-01-01 (Howard Hinnant's civil_from_days).
pub(crate) fn format_date(days: i64) -> String {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

/// An ISO 8601 date-time from milliseconds since the Unix epoch, marked UTC when `utc`.
pub(crate) fn format_datetime(ms: i64, utc: bool) -> String {
    let days = ms.div_euclid(86_400_000);
    let t = ms.rem_euclid(86_400_000);
    let (h, mi, s, frac) = (t / 3_600_000, t / 60_000 % 60, t / 1000 % 60, t % 1000);
    let mut out = format!("{}T{h:02}:{mi:02}:{s:02}", format_date(days));
    if frac > 0 {
        out.push_str(&format!(".{frac:03}"));
    }
    if utc {
        out.push('Z');
    }
    out
}

/// Days since 1970-01-01 of a `YYYY-MM-DD` date.
pub(crate) fn parse_date(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s.get(..4)?.parse().ok()?;
    let m: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i32::try_from(era * 146097 + doe - 719468).ok()
}

/// Milliseconds since the Unix epoch of an ISO 8601 date-time, in UTC when it has an offset.
pub(crate) fn parse_datetime(s: &str) -> Option<i64> {
    let days = parse_date(s)? as i64;
    let rest = &s[10..];
    if rest.is_empty() {
        return Some(days * 86_400_000);
    }
    let rest = rest.strip_prefix(['T', ' '])?;
    let (h, mi, sec) = (rest.get(..2)?.parse::<i64>().ok()?, rest.get(3..5)?.parse::<i64>().ok()?, rest.get(6..8)?.parse::<i64>().ok()?);
    let mut tail = &rest[8..];
    let mut ms = 0i64;
    if let Some(frac) = tail.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        let digits = format!("{:0<3}", &frac[..n.min(3)]);
        ms = digits.parse().ok()?;
        tail = &frac[n..];
    }
    let offset_min = match tail {
        "" | "Z" => 0,
        t if t.len() >= 5 && (t.starts_with('+') || t.starts_with('-')) => {
            let sign = if t.starts_with('-') { -1 } else { 1 };
            let t = t[1..].replace(':', "");
            sign * (t.get(..2)?.parse::<i64>().ok()? * 60 + t.get(2..4)?.parse::<i64>().ok()?)
        }
        _ => return None,
    };
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60) * 1000) + ms)
}

/// Whether a date-time string ends with a UTC offset (`Z`, `+hh:mm`, `-hhmm`).
pub(crate) fn has_offset(s: &str) -> bool {
    let time = s.get(10..).unwrap_or_default();
    time.ends_with('Z') || time.get(1..).is_some_and(|t| t.contains('+') || t.rfind('-').is_some_and(|i| i >= 6))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2000-03-01"), Some(11017));
        assert_eq!(parse_datetime("1970-01-02T00:00:01.5Z"), Some(86_401_500));
        assert_eq!(parse_datetime("1970-01-01T01:00:00+01:00"), Some(0));
        // Multi-byte text is not a date, and must not panic (#159).
        assert_eq!(parse_date("2020-01-0千"), None);
        assert_eq!(parse_datetime("東京都千代田区一丁目"), None);
    }

    #[test]
    fn formats_dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(11017), "2000-03-01");
        assert_eq!(format_date(-1), "1969-12-31");
        assert_eq!(format_datetime(86_401_500, true), "1970-01-02T00:00:01.500Z");
    }
}
