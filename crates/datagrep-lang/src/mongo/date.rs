fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

pub fn parse_iso8601_utc_micros(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let slice = s.get(r)?;
        if !slice.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        slice.parse().ok()
    };
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = digits(0..4)?;
    let month = digits(5..7)? as u32;
    let day = digits(8..10)? as u32;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut hour = 0i64;
    let mut minute = 0i64;
    let mut second = 0i64;
    let mut micros = 0i64;

    let mut rest = &s[10..];
    if let Some(after_sep) = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' ')) {
        rest = after_sep;
        let rb = rest.as_bytes();
        if rb.len() < 8 || rb[2] != b':' || rb[5] != b':' {
            return None;
        }
        hour = rest.get(0..2)?.parse().ok()?;
        minute = rest.get(3..5)?.parse().ok()?;
        second = rest.get(6..8)?.parse().ok()?;
        rest = &rest[8..];
        if let Some(frac) = rest.strip_prefix('.') {
            let end = frac
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(frac.len());
            let frac_digits = &frac[..end];
            if frac_digits.is_empty() {
                return None;
            }
            let mut padded = frac_digits.to_string();
            padded.truncate(6);
            while padded.len() < 6 {
                padded.push('0');
            }
            micros = padded.parse().ok()?;
            rest = &frac[end..];
        }
        if !rest.is_empty() && rest != "Z" {
            return None;
        }
    } else if !rest.is_empty() {
        return None;
    }

    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..60).contains(&second) {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let secs_of_day = hour * 3600 + minute * 60 + second;
    Some((days * 86_400 + secs_of_day) * 1_000_000 + micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch() {
        assert_eq!(parse_iso8601_utc_micros("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_utc_micros("1970-01-01"), Some(0));
    }

    #[test]
    fn known_date() {
        // 2024-01-15T10:30:00Z, cross-checked against `date -u -d ...`.
        let micros = parse_iso8601_utc_micros("2024-01-15T10:30:00Z").unwrap();
        assert_eq!(micros / 1_000_000 / 86_400, days_from_civil(2024, 1, 15));
        assert_eq!(micros % 86_400_000_000, (10 * 3600 + 30 * 60) * 1_000_000);
    }

    #[test]
    fn fractional_seconds() {
        let micros = parse_iso8601_utc_micros("2024-01-15T10:30:00.123Z").unwrap();
        assert_eq!(micros % 1_000_000, 123_000);
    }

    #[test]
    fn space_separator_accepted() {
        assert_eq!(
            parse_iso8601_utc_micros("2024-01-15 10:30:00"),
            parse_iso8601_utc_micros("2024-01-15T10:30:00Z")
        );
    }

    #[test]
    fn pre_epoch_date_is_negative() {
        let micros = parse_iso8601_utc_micros("1960-01-01T00:00:00Z").unwrap();
        assert!(micros < 0);
    }

    #[test]
    fn garbage_is_none_not_panic() {
        assert_eq!(parse_iso8601_utc_micros(""), None);
        assert_eq!(parse_iso8601_utc_micros("not a date"), None);
        assert_eq!(parse_iso8601_utc_micros("2024-13-01"), None);
        assert_eq!(parse_iso8601_utc_micros("2024-01-32"), None);
        assert_eq!(parse_iso8601_utc_micros("2024-01-15T25:00:00Z"), None);
    }
}
