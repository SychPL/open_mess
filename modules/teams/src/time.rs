//! UTC times as epoch milliseconds, parsed without a clock or a date crate: ISO 8601 for message
//! times, HTTP dates for `Retry-After`.

/// ISO 8601 UTC text as the chat service writes it, `2027-01-15T08:00:00.100Z` (the fraction is
/// optional; digits past milliseconds are dropped).
pub(crate) fn iso8601_ms(text: &str) -> Option<i64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let (hms, fraction) = time.split_once('.').unwrap_or((time, ""));
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis: String = fraction.chars().chain("000".chars()).take(3).collect();
    let mut d = date.split('-');
    let (year, month, day) = (digits(d.next()?, 4)?, digits(d.next()?, 2)?, digits(d.next()?, 2)?);
    let mut t = hms.split(':');
    let (hour, minute, second) = (digits(t.next()?, 2)?, digits(t.next()?, 2)?, digits(t.next()?, 2)?);
    if d.next().is_some() || t.next().is_some() {
        return None;
    }
    Some(epoch_ms(year, month, day, hour, minute, second)? + millis.parse::<i64>().ok()?)
}

/// An HTTP date in the IMF-fixdate form, `Sun, 06 Nov 1994 08:49:37 GMT` (RFC 9110 5.6.7), the
/// form `Retry-After` dates are sent in.
pub(crate) fn http_date_ms(text: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut parts = text.split_whitespace();
    let _weekday = parts.next()?.strip_suffix(',')?;
    let day = digits(parts.next()?, 2)?;
    let month_name = parts.next()?;
    let month = i64::try_from(MONTHS.iter().position(|m| *m == month_name)?).ok()? + 1;
    let year = digits(parts.next()?, 4)?;
    let mut t = parts.next()?.split(':');
    let (hour, minute, second) = (digits(t.next()?, 2)?, digits(t.next()?, 2)?, digits(t.next()?, 2)?);
    if parts.next()? != "GMT" || parts.next().is_some() || t.next().is_some() {
        return None;
    }
    epoch_ms(year, month, day, hour, minute, second)
}

/// Exactly `len` ASCII digits; the fixed width keeps every later product far from overflow.
fn digits(text: &str, len: usize) -> Option<i64> {
    if text.len() != len || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Days from civil, H. Hinnant's algorithm (proleptic Gregorian calendar).
fn epoch_ms(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_matches_known_epochs() {
        assert_eq!(iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso8601_ms("2027-01-15T08:00:00.100Z"), Some(1_800_000_000_100));
        assert_eq!(iso8601_ms("2027-01-15T08:00:00.1234567Z"), Some(1_800_000_000_123));
        assert_eq!(iso8601_ms("2000-02-29T12:00:00Z"), Some(951_825_600_000));
        for bad in ["", "soon", "2027-01-15T08:00:00", "2027-13-15T08:00:00Z", "99999-01-15T08:00:00Z", "2027-01-15T08:00:00.x1Z"] {
            assert_eq!(iso8601_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn http_date_matches_known_epochs() {
        assert_eq!(http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777_000));
        assert_eq!(http_date_ms("Fri, 15 Jan 2027 08:00:30 GMT"), Some(1_800_000_030_000));
        for bad in
            ["", "Sun, 06 Nov 1994 08:49:37", "Sun, 06 Foo 1994 08:49:37 GMT", "Sun, 6 Nov 1994 08:49:37 GMT", "Sunday, 06-Nov-94 08:49:37 GMT"]
        {
            assert_eq!(http_date_ms(bad), None, "{bad}");
        }
    }
}
