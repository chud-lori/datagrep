/// Grouped for prose ("first 12,000 rows"); the row-number gutter stays ungrouped.
pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

pub fn bytes(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = value;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", value as i64)
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

pub fn rows(value: Option<u64>) -> Option<String> {
    let value = value?;
    Some(match value {
        1 => "1 row".to_owned(),
        _ => format!("{} rows", count(value)),
    })
}

pub fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.2} s", ms as f64 / 1000.0)
    } else {
        format!("{} m {:02} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

fn local(ms: i64) -> Option<glib::DateTime> {
    glib::DateTime::from_unix_local(ms.div_euclid(1000)).ok()
}

pub fn time_of_day(ms: i64) -> String {
    local(ms)
        .and_then(|dt| dt.format("%H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// "Today" means the day the user had, so the comparison is on local day keys.
pub fn day_title(day_key: &str, now_ms: i64) -> String {
    if day_key == super::history::day_key(now_ms) {
        return "Today".to_owned();
    }
    if day_key == super::history::day_key(now_ms - 86_400_000) {
        return "Yesterday".to_owned();
    }
    let Some(date) = parse_day_key(day_key) else {
        return day_key.to_owned();
    };
    let month = date.format("%B").map(|s| s.to_string()).unwrap_or_default();
    let this_year = local(now_ms).map(|now| now.year()) == Some(date.year());
    if this_year {
        let weekday = date.format("%A").map(|s| s.to_string()).unwrap_or_default();
        format!("{weekday} {} {month}", date.day_of_month())
    } else {
        format!("{} {month} {}", date.day_of_month(), date.year())
    }
}

fn parse_day_key(key: &str) -> Option<glib::DateTime> {
    let mut parts = key.split('-').map(str::parse::<i32>);
    let (year, month, day) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    glib::DateTime::new(&glib::TimeZone::local(), year, month, day, 12, 0, 0.0).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped_at_every_third_digit() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn bytes_stay_whole_until_they_leave_bytes() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(16384.0), "16.0 KB");
        assert_eq!(bytes(1024.0 * 1024.0 * 3.5), "3.5 MB");
    }

    #[test]
    fn durations_switch_unit_at_a_second_and_at_a_minute() {
        assert_eq!(duration(999), "999 ms");
        assert_eq!(duration(1500), "1.50 s");
        assert_eq!(duration(61_000), "1 m 01 s");
    }

    #[test]
    fn a_result_set_of_one_row_is_not_pluralised_and_none_is_not_a_zero() {
        assert_eq!(rows(Some(1)).as_deref(), Some("1 row"));
        assert_eq!(rows(Some(4210)).as_deref(), Some("4,210 rows"));
        assert_eq!(rows(None), None);
    }

    #[test]
    fn the_day_a_statement_ran_is_named_relative_to_today() {
        let now = crate::model::history::now_ms();
        assert_eq!(
            day_title(&crate::model::history::day_key(now), now),
            "Today"
        );
        assert_eq!(
            day_title(&crate::model::history::day_key(now - 86_400_000), now),
            "Yesterday"
        );
        assert_eq!(day_title("2019-03-04", now), "4 March 2019");
    }
}
