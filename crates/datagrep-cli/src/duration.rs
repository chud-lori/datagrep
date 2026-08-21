use std::time::Duration;

pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("expected a duration like 30s, 500ms, 5m, or 2h".to_string());
    }
    let split_at = s
        .find(|c: char| !c.is_ascii_digit())
        .filter(|&i| i > 0)
        .ok_or_else(|| format!("`{s}` is missing a unit (expected 30s, 500ms, 5m, or 2h)"))?;
    let (num, unit) = s.split_at(split_at);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("`{num}` is not a valid number"))?;
    match unit {
        "ms" => Ok(Duration::from_millis(n)),
        "s" => Ok(Duration::from_secs(n)),
        "m" => n
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(|| format!("`{s}` overflows")),
        "h" => n
            .checked_mul(3600)
            .map(Duration::from_secs)
            .ok_or_else(|| format!("`{s}` overflows")),
        other => Err(format!(
            "unknown duration unit `{other}` (expected ms, s, m, or h)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_unit() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn rejects_bare_numbers_and_garbage() {
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("s").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("30x").is_err());
    }
}
