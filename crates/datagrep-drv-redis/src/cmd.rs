//! Small, dependency-free helpers shared by `connection.rs` and
//! `catalog.rs`: building a `redis::Cmd` from tokenized arguments,
//! compiling a [`Predicate`] to a `SCAN`-style glob for `Op::Scan`, and
//! recognizing the handful of commands that block the connection (the
//! `Canceller` needs to know before one is dispatched).

use std::sync::Arc;

use datagrep_api::request::Predicate;
use datagrep_api::value::Value;
use datagrep_api::DbError;

/// Build a `redis::Cmd` from already-tokenized redis-cli-style arguments
/// (`datagrep_lang::redis::tokenize_args`'s output). Arguments are sent as raw
/// bytes — Redis commands are binary-safe and we must not reinterpret them.
pub fn cmd_from_args<I, S>(args: I) -> redis::Cmd
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut cmd = redis::Cmd::new();
    for a in args {
        cmd.arg(a.as_ref().as_bytes());
    }
    cmd
}

/// Commands that block the connection waiting on the server; cancelling one
/// needs a real `CLIENT KILL ID` from a second connection, since simply
/// abandoning it client-side would leave the socket hung until the server's
/// own timeout.
/// Matched case-insensitively against the first token only — `WAIT`/`WAITAOF`
/// block unconditionally; the `B`-prefixed list commands and `XREAD`/
/// `XREADGROUP` only block when a `BLOCK` option is present, which
/// `is_blocking_invocation` checks separately.
const UNCONDITIONALLY_BLOCKING: &[&str] = &["WAIT", "WAITAOF"];

const ALWAYS_BLOCKING_COMMANDS: &[&str] = &[
    "BLPOP",
    "BRPOP",
    "BLMOVE",
    "BRPOPLPUSH",
    "BLMPOP",
    "BZPOPMIN",
    "BZPOPMAX",
    "BZMPOP",
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
];

/// Whether dispatching `args` (already tokenized, `args[0]` the command
/// name) will block the connection until the server has something to say —
/// the signal `connection.rs` uses to arm the canceller's `CLIENT KILL`
/// path instead of the default `ClientAbandon` one.
pub fn is_blocking_invocation(args: &[String]) -> bool {
    let Some(cmd) = args.first() else {
        return false;
    };
    let hit = |set: &[&str]| set.iter().any(|c| cmd.eq_ignore_ascii_case(c));
    if hit(UNCONDITIONALLY_BLOCKING) || hit(ALWAYS_BLOCKING_COMMANDS) {
        return true;
    }
    // XREAD / XREADGROUP only block when a `BLOCK <ms>` option is present.
    if cmd.eq_ignore_ascii_case("XREAD") || cmd.eq_ignore_ascii_case("XREADGROUP") {
        return args.iter().any(|a| a.eq_ignore_ascii_case("BLOCK"));
    }
    false
}

/// Compile a portable [`Predicate`] from `Op::Scan` into a Redis `MATCH`
/// glob. `Shape::Pairs` has no real field names —
/// the only addressable "column" of a Redis key listing is the key name
/// itself, so this only understands predicates against a field literally
/// named `key`, matching `datagrep_api::value::FieldPath::field("key")`.
///
/// Returns `Err(DbError::Unsupported)` — never a silently dropped filter —
/// for anything this narrow glob model cannot express: comparisons other
/// than equality/prefix-like patterns, multi-field/boolean combinations,
/// `Exists`/`IsNull` (Redis pairs have no notion of a present-but-null
/// field), etc.
pub fn compile_glob(predicate: &Predicate) -> Result<String, DbError> {
    match predicate {
        Predicate::Eq {
            field,
            value: Value::Str(s),
        } if is_key_field(field) => Ok(escape_glob_literal(s)),
        Predicate::Like { field, pattern } if is_key_field(field) => Ok(pattern.to_string()),
        Predicate::And(parts) if parts.len() == 1 => compile_glob(&parts[0]),
        other => Err(DbError::Unsupported {
            feature: format!(
                "predicate {other:?} has no Redis MATCH-glob equivalent (only Eq/Like on the \
                 `key` field compile; refusing to silently drop the filter)"
            ),
        }),
    }
}

fn is_key_field(field: &datagrep_api::value::FieldPath) -> bool {
    matches!(
        field.segments(),
        [datagrep_api::value::PathSeg::Field(name)] if &**name == "key"
    )
}

/// Escape Redis glob metacharacters (`* ? [ ]` and `\` itself) so an exact
/// string can be used as a literal `MATCH` pattern.
pub fn escape_glob_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Split a stored keyspace prefix (`"user:"`) into a `MATCH` glob covering
/// everything under it.
pub fn prefix_glob(prefix: &str) -> String {
    format!("{}*", escape_glob_literal(prefix))
}

/// Derive top-level colon-delimited prefixes from a *sample* of key names —
/// the catalog builds its tree by splitting sampled keys on `:`, never by
/// walking the whole keyspace. Prefixes are returned with their trailing
/// `:` kept, insertion-ordered by first appearance, deduplicated.
pub fn derive_prefixes(sampled_keys: &[String]) -> Vec<Arc<str>> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for key in sampled_keys {
        if let Some(idx) = key.find(':') {
            let prefix = &key[..=idx];
            if seen.insert(prefix.to_string()) {
                out.push(Arc::from(prefix));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::value::FieldPath;

    #[test]
    fn blocking_detection_covers_the_documented_commands() {
        assert!(is_blocking_invocation(&[
            "BLPOP".into(),
            "q".into(),
            "0".into()
        ]));
        assert!(is_blocking_invocation(&[
            "wait".into(),
            "1".into(),
            "0".into()
        ]));
        assert!(!is_blocking_invocation(&["GET".into(), "k".into()]));
        assert!(is_blocking_invocation(&[
            "XREAD".into(),
            "BLOCK".into(),
            "0".into(),
            "STREAMS".into(),
            "s".into(),
            "$".into()
        ]));
        assert!(!is_blocking_invocation(&[
            "XREAD".into(),
            "COUNT".into(),
            "5".into(),
            "STREAMS".into(),
            "s".into(),
            "0".into()
        ]));
        assert!(!is_blocking_invocation(&[]));
    }

    #[test]
    fn glob_compiles_eq_and_like_on_key_field() {
        let eq = Predicate::Eq {
            field: FieldPath::field("key"),
            value: Value::Str("user:42".into()),
        };
        assert_eq!(compile_glob(&eq).unwrap(), "user:42");

        let like = Predicate::Like {
            field: FieldPath::field("key"),
            pattern: "user:*".into(),
        };
        assert_eq!(compile_glob(&like).unwrap(), "user:*");
    }

    #[test]
    fn glob_escapes_metacharacters_in_exact_match() {
        let eq = Predicate::Eq {
            field: FieldPath::field("key"),
            value: Value::Str("weird[key]*name".into()),
        };
        assert_eq!(compile_glob(&eq).unwrap(), r"weird\[key\]\*name");
    }

    #[test]
    fn glob_rejects_unexpressable_predicates_instead_of_dropping_them() {
        let ne = Predicate::Ne {
            field: FieldPath::field("key"),
            value: Value::Str("x".into()),
        };
        assert!(matches!(
            compile_glob(&ne),
            Err(DbError::Unsupported { .. })
        ));

        let wrong_field = Predicate::Eq {
            field: FieldPath::field("value"),
            value: Value::Str("x".into()),
        };
        assert!(matches!(
            compile_glob(&wrong_field),
            Err(DbError::Unsupported { .. })
        ));

        let multi = Predicate::And(vec![
            Predicate::Eq {
                field: FieldPath::field("key"),
                value: Value::Str("a".into()),
            },
            Predicate::Eq {
                field: FieldPath::field("key"),
                value: Value::Str("b".into()),
            },
        ]);
        assert!(matches!(
            compile_glob(&multi),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn prefix_derivation_splits_on_first_colon_and_dedupes() {
        let keys = vec![
            "user:1".to_string(),
            "user:2".to_string(),
            "session:abc".to_string(),
            "noColonHere".to_string(),
            "user:3:profile".to_string(),
        ];
        let prefixes = derive_prefixes(&keys);
        let strs: Vec<&str> = prefixes.iter().map(|p| &**p).collect();
        assert_eq!(
            strs,
            vec!["user:", "session:"],
            "insertion-ordered, deduped, no-colon keys skipped"
        );
    }
}
