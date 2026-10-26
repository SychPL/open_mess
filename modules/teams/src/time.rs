//! UTC times as epoch milliseconds, parsed without a clock or a date crate.

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
}
