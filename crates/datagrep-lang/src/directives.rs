//! Block directives: the entire meta-language is four items,
//! written as the last comment lines immediately above a statement:
//!
//! ```text
//! -- @limit 200          -- @timeout 30s
//! -- @connection staging -- @readonly
//! ```
//!
//! `--`-comment languages (SQL) write directives with `--`; everything else
//! (Redis, MongoShell) uses `#`. No DSL beyond these four keys — an unknown
//! key or a malformed value is a [`DirectiveError`], never a silent no-op and
//! never a panic.

use std::time::Duration;

/// The four directives, parsed from the comment block immediately preceding
/// a statement. `Default` is "no directives were present."
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Directives {
    pub limit: Option<u64>,
    pub timeout: Option<Duration>,
    pub connection: Option<String>,
    pub readonly: bool,
}

/// A directive comment was present but malformed, or named a key outside the
/// fixed set of four.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DirectiveError {
    #[error("unknown directive @{0} — only limit, timeout, connection, readonly are recognized")]
    UnknownDirective(String),
    #[error("@{directive} {value:?} is not a valid value: {reason}")]
    InvalidValue {
        directive: &'static str,
        value: String,
        reason: &'static str,
    },
}

/// Parse the directive block immediately above a statement. `lines` must
/// already be exactly the contiguous run of directive-comment lines with
/// their comment marker (and any leading indentation) stripped — see
/// [`extract_directive_lines`] for how callers assemble that run from raw
/// source text.
///
/// Every line must match `@key value` (value omitted for `@readonly`); the
/// first malformed or unrecognized line aborts the whole block with an
/// error, rather than applying a partial set of directives silently.
pub fn parse_directives(lines: &[&str]) -> Result<Directives, DirectiveError> {
    let mut out = Directives::default();
    for line in lines {
        let rest = line.trim();
        let rest = rest
            .strip_prefix('@')
            .ok_or_else(|| DirectiveError::InvalidValue {
                directive: "?",
                value: (*line).to_string(),
                reason: "directive lines must start with @",
            })?;
        let (key, value) = match rest.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v.trim()),
            None => (rest, ""),
        };
        match key {
            "limit" => {
                let n: u64 = value.parse().map_err(|_| DirectiveError::InvalidValue {
                    directive: "limit",
                    value: value.to_string(),
                    reason: "expected a non-negative integer",
                })?;
                out.limit = Some(n);
            }
            "timeout" => {
                out.timeout =
                    Some(
                        parse_duration(value).ok_or_else(|| DirectiveError::InvalidValue {
                            directive: "timeout",
                            value: value.to_string(),
                            reason: "expected a duration like 30s, 500ms, 5m, or 2h",
                        })?,
                    );
            }
            "connection" => {
                if value.is_empty() {
                    return Err(DirectiveError::InvalidValue {
                        directive: "connection",
                        value: value.to_string(),
                        reason: "expected a connection name",
                    });
                }
                out.connection = Some(value.to_string());
            }
            "readonly" => {
                if !value.is_empty() {
                    return Err(DirectiveError::InvalidValue {
                        directive: "readonly",
                        value: value.to_string(),
                        reason: "@readonly takes no value",
                    });
                }
                out.readonly = true;
            }
            other => return Err(DirectiveError::UnknownDirective(other.to_string())),
        }
    }
    Ok(out)
}

/// Parse a duration like `30s`, `500ms`, `5m`, `2h`. No fractional units, no
/// bare numbers (ambiguity is a bug users hit once and then hate forever).
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let split_at = s.find(|c: char| !c.is_ascii_digit())?;
    if split_at == 0 {
        return None;
    }
    let (num, unit) = s.split_at(split_at);
    let n: u64 = num.parse().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n.checked_mul(60)?)),
        "h" => Some(Duration::from_secs(n.checked_mul(3600)?)),
        _ => None,
    }
}

/// Walk backward from `stmt_start` (a byte offset into `src`, the start of a
/// statement's non-whitespace content) collecting the contiguous run of
/// comment lines immediately above it that use `marker` (`--` or `#`) *and*
/// start with `@` after the marker. A blank line, a non-directive comment, or
/// any other content stops the walk — directives must be the comment(s)
/// truly immediately adjacent to the statement, not just "somewhere above."
///
/// Returns the directive lines in source order (top to bottom), each with
/// its marker and any leading indentation already stripped, ready for
/// [`parse_directives`].
pub fn extract_directive_lines<'s>(src: &'s str, stmt_start: usize, marker: &str) -> Vec<&'s str> {
    // Collect candidate physical lines strictly before `stmt_start`, walking
    // upward. `line_start` tracks the start of the line currently being
    // inspected.
    let mut lines_rev: Vec<&str> = Vec::new();
    let mut pos = stmt_start;
    loop {
        if pos == 0 {
            break;
        }
        // Find the start of the line ending at `pos` (pos is either
        // `stmt_start` or the start of the previously-accepted line, both of
        // which sit right after a '\n' or at buffer start).
        let line_end = pos.saturating_sub(1); // the '\n' just before `pos`, if any
        if src.as_bytes().get(line_end) != Some(&b'\n') {
            // stmt_start wasn't at a fresh line (e.g. directives on the same
            // line as the statement, which we don't support) — nothing to
            // extract.
            break;
        }
        let line_start = src[..line_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line = &src[line_start..line_end];
        let trimmed_line = line.trim_start();
        let after_marker = match trimmed_line.strip_prefix(marker) {
            Some(rest) => rest,
            None => break,
        };
        let after_marker = after_marker.trim_start();
        if !after_marker.starts_with('@') {
            break;
        }
        lines_rev.push(after_marker);
        pos = line_start;
    }
    lines_rev.reverse();
    lines_rev
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_four_directives_stacked() {
        let lines = [
            "@limit 200",
            "@timeout 30s",
            "@connection staging",
            "@readonly",
        ];
        let d = parse_directives(&lines).unwrap();
        assert_eq!(d.limit, Some(200));
        assert_eq!(d.timeout, Some(Duration::from_secs(30)));
        assert_eq!(d.connection.as_deref(), Some("staging"));
        assert!(d.readonly);
    }

    #[test]
    fn timeout_units() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("2x"), None);
        assert_eq!(parse_duration("s"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn unknown_directive_errors() {
        let lines = ["@bogus 1"];
        let err = parse_directives(&lines).unwrap_err();
        assert_eq!(err, DirectiveError::UnknownDirective("bogus".to_string()));
    }

    #[test]
    fn bad_values_error_not_panic() {
        assert!(parse_directives(&["@limit notanumber"]).is_err());
        assert!(parse_directives(&["@limit -5"]).is_err());
        assert!(parse_directives(&["@timeout"]).is_err());
        assert!(parse_directives(&["@timeout 30"]).is_err());
        assert!(parse_directives(&["@connection"]).is_err());
        assert!(parse_directives(&["@readonly yes"]).is_err());
    }

    #[test]
    fn no_directives_is_default() {
        assert_eq!(parse_directives(&[]).unwrap(), Directives::default());
    }

    #[test]
    fn extract_stops_at_blank_line_or_plain_comment() {
        let src = "-- a plain comment\n-- @limit 5\nSELECT 1;";
        let stmt_start = src.rfind("SELECT").unwrap();
        let lines = extract_directive_lines(src, stmt_start, "--");
        // The plain comment above the directive breaks contiguity, so only
        // the directive line itself is collected.
        assert_eq!(lines, vec!["@limit 5"]);

        let src2 = "-- @limit 5\n\nSELECT 1;";
        let stmt_start2 = src2.rfind("SELECT").unwrap();
        assert!(extract_directive_lines(src2, stmt_start2, "--").is_empty());
    }

    #[test]
    fn extract_collects_contiguous_block_in_order() {
        let src = "-- @limit 5\n-- @readonly\nSELECT 1;";
        let stmt_start = src.rfind("SELECT").unwrap();
        let lines = extract_directive_lines(src, stmt_start, "--");
        assert_eq!(lines, vec!["@limit 5", "@readonly"]);
    }
}
