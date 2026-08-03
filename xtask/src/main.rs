//! dbx xtask — CI helper binary (design doc §5 / §6 / §11.3).
//!
//! Invoked as `cd xtask && cargo run -- <cmd>`, or by `ci/gates.sh` as a
//! prebuilt binary. Commands:
//!
//! * `budget-check <binary-path> [--budget <path>]`
//!   File size vs budget.toml P11 (installed-on-disk); P10 (compressed
//!   installer) reported informationally. Exits nonzero on a FAIL-threshold
//!   breach (the CI-red line), zero on target-only breach (WARN).
//! * `count-crates [<workspace-root>] [--budget <path>] [--strict]`
//!   Unique crates in `cargo tree --workspace -e normal` vs P16d. Always
//!   exits 0 unless `--strict` and the fail threshold is breached (the gate
//!   is warn-only for now per §5 wiring plan).
//! * `grep-gates [<root>] [--allowlist <path>]`
//!   The §5.2 banned-pattern greps, structured, with an allowlist. Exits
//!   nonzero on any non-allowlisted FAIL finding.
//!
//! No dependencies by design: the TOML subset parser below handles exactly
//! the shape of budget.toml (tables + `key = "string"`), and is unit-tested.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter().map(String::as_str);
    match it.next() {
        Some("budget-check") => cmd_budget_check(&args[1..]),
        Some("count-crates") => cmd_count_crates(&args[1..]),
        Some("grep-gates") => cmd_grep_gates(&args[1..]),
        Some("help") | None => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("xtask: unknown command `{other}`");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage:\n  \
         xtask budget-check <binary-path> [--budget <budget.toml>]\n  \
         xtask count-crates [<workspace-root>] [--budget <budget.toml>] [--strict]\n  \
         xtask grep-gates [<root>] [--allowlist <path>]"
    );
}

// ---------------------------------------------------------------------------
// TOML subset parser (budget.toml only: [Table] + key = "quoted string")
// ---------------------------------------------------------------------------

type Budget = BTreeMap<String, BTreeMap<String, String>>;

fn parse_budget(text: &str) -> Result<Budget, String> {
    let mut out: Budget = BTreeMap::new();
    let mut current: Option<String> = None;
    for (idx, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let name = name.trim().to_string();
            out.entry(name.clone()).or_default();
            current = Some(name);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("budget.toml line {}: unrecognized syntax", idx + 1));
        };
        let key = key.trim().to_string();
        let value = value.trim();
        let Some(value) = value
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .map(str::to_string)
        else {
            return Err(format!(
                "budget.toml line {}: only `key = \"string\"` values are supported",
                idx + 1
            ));
        };
        let Some(table) = current.as_ref() else {
            return Err(format!("budget.toml line {}: key outside a table", idx + 1));
        };
        out.get_mut(table).unwrap().insert(key, value);
    }
    Ok(out)
}

/// Strip a `#` comment, respecting double-quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Parse "22MB" / "64KB" / "4GB" / "512B" / "1234" into bytes (decimal units,
/// matching how installer sizes are conventionally quoted).
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("GB") {
        (n, 1_000_000_000u64)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n, 1_000_000u64)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n, 1_000u64)
    } else if let Some(n) = s.strip_suffix('B') {
        (n, 1u64)
    } else {
        (s, 1u64)
    };
    let num = num.trim();
    num.parse::<f64>()
        .map(|v| (v * mult as f64) as u64)
        .map_err(|_| format!("cannot parse size `{s}`"))
}

/// P16d values look like "24 / 400" (pipelines / crates). Return the crate
/// count — the part after the '/'.
fn parse_crate_limit(s: &str) -> Result<u64, String> {
    let part = s.rsplit('/').next().unwrap_or(s).trim();
    // Tolerate suffixed prose like "> 600" if the toml ever carries it.
    let digits: String = part.chars().filter(|c| c.is_ascii_digit()).collect();
    digits
        .parse::<u64>()
        .map_err(|_| format!("cannot parse crate limit from `{s}`"))
}

fn budget_value<'a>(budget: &'a Budget, table: &str, key: &str) -> Result<&'a str, String> {
    budget
        .get(table)
        .and_then(|t| t.get(key))
        .map(String::as_str)
        .ok_or_else(|| format!("budget.toml: missing [{table}] {key}"))
}

fn load_budget(explicit: Option<&str>) -> Result<Budget, String> {
    let candidates: Vec<PathBuf> = match explicit {
        Some(p) => vec![PathBuf::from(p)],
        None => vec![
            PathBuf::from("budget.toml"),
            PathBuf::from("../budget.toml"),
        ],
    };
    for path in &candidates {
        if path.is_file() {
            let text =
                fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
            return parse_budget(&text);
        }
    }
    Err(format!(
        "budget.toml not found (tried: {})",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

// ---------------------------------------------------------------------------
// budget-check
// ---------------------------------------------------------------------------

fn cmd_budget_check(args: &[String]) -> ExitCode {
    let mut binary: Option<&str> = None;
    let mut budget_path: Option<&str> = None;
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        match a {
            "--budget" => budget_path = it.next(),
            _ if binary.is_none() => binary = Some(a),
            _ => {
                eprintln!("budget-check: unexpected argument `{a}`");
                return ExitCode::from(2);
            }
        }
    }
    let Some(binary) = binary else {
        eprintln!("budget-check: missing <binary-path>");
        return ExitCode::from(2);
    };
    match budget_check(binary, budget_path) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("budget-check: {e}");
            ExitCode::from(2)
        }
    }
}

fn budget_check(binary: &str, budget_path: Option<&str>) -> Result<bool, String> {
    let budget = load_budget(budget_path)?;
    let size = fs::metadata(binary)
        .map_err(|e| format!("stat {binary}: {e}"))?
        .len();

    let p11_target = parse_size(budget_value(&budget, "P11", "target")?)?;
    let p11_fail = parse_size(budget_value(&budget, "P11", "fail")?)?;
    let p10_target = parse_size(budget_value(&budget, "P10", "target")?)?;
    let p10_fail = parse_size(budget_value(&budget, "P10", "fail")?)?;

    println!(
        "budget-check: {} is {} bytes ({:.1} MB)",
        binary,
        size,
        size as f64 / 1e6
    );
    println!(
        "  P11 installed-on-disk: target {:.0} MB, fail {:.0} MB",
        p11_target as f64 / 1e6,
        p11_fail as f64 / 1e6
    );
    println!(
        "  P10 (informational — applies to the *compressed installer*, not this file): \
         target {:.0} MB, fail {:.0} MB",
        p10_target as f64 / 1e6,
        p10_fail as f64 / 1e6
    );

    if size > p11_fail {
        println!(
            "  FAIL: binary exceeds the P11 fail threshold by {:.1} MB",
            (size - p11_fail) as f64 / 1e6
        );
        Ok(false)
    } else if size > p11_target {
        println!(
            "  WARN: binary exceeds the P11 target (within fail threshold) by {:.1} MB",
            (size - p11_target) as f64 / 1e6
        );
        Ok(true)
    } else {
        println!("  OK: within P11 target");
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// count-crates
// ---------------------------------------------------------------------------

fn cmd_count_crates(args: &[String]) -> ExitCode {
    let mut root: Option<&str> = None;
    let mut budget_path: Option<&str> = None;
    let mut strict = false;
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        match a {
            "--budget" => budget_path = it.next(),
            "--strict" => strict = true,
            _ if root.is_none() => root = Some(a),
            _ => {
                eprintln!("count-crates: unexpected argument `{a}`");
                return ExitCode::from(2);
            }
        }
    }
    let root = root.unwrap_or(if Path::new("Cargo.toml").is_file() {
        "."
    } else {
        ".."
    });
    match count_crates(root, budget_path, strict) {
        Ok(ok) => {
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("count-crates: {e}");
            ExitCode::from(2)
        }
    }
}

fn count_crates(root: &str, budget_path: Option<&str>, strict: bool) -> Result<bool, String> {
    let out = Command::new("cargo")
        .args(["tree", "--workspace", "-e", "normal", "--prefix", "none"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("spawn cargo tree: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo tree failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let count = count_unique_crates(&stdout);

    let budget = match budget_path {
        Some(p) => load_budget(Some(p))?,
        None => {
            // Prefer <root>/budget.toml, then the usual ./ and ../ fallbacks.
            let p = format!("{root}/budget.toml");
            if Path::new(&p).is_file() {
                load_budget(Some(&p))?
            } else {
                load_budget(None)?
            }
        }
    };
    let target = parse_crate_limit(budget_value(&budget, "P16d", "target")?)?;
    let fail = parse_crate_limit(budget_value(&budget, "P16d", "fail")?)?;

    println!(
        "count-crates: {count} unique crates in the normal-dep graph (P16d target <= {target}, fail > {fail})"
    );
    if count > fail {
        println!("  FAIL: crate count breaches the P16d fail threshold");
        Ok(!strict)
    } else if count > target {
        println!("  WARN: crate count exceeds the P16d target (within fail threshold)");
        Ok(true)
    } else {
        println!("  OK: within P16d target");
        Ok(true)
    }
}

/// Count unique `name version` pairs in `cargo tree --prefix none` output.
/// Deduplicated repeats are printed with a trailing `(*)`; feature/target
/// annotations vary — key on the first two whitespace-separated tokens.
fn count_unique_crates(tree_output: &str) -> u64 {
    let mut set: BTreeSet<(String, String)> = BTreeSet::new();
    for line in tree_output.lines() {
        let mut tok = line.split_whitespace();
        let (Some(name), Some(version)) = (tok.next(), tok.next()) else {
            continue;
        };
        if !version.starts_with('v') {
            continue;
        }
        set.insert((name.to_string(), version.to_string()));
    }
    set.len() as u64
}

// ---------------------------------------------------------------------------
// grep-gates — §5.2 banned anti-patterns, structured
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Fail,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    rule: &'static str,
    severity: Severity,
    path: String,
    line: usize,
    text: String,
}

/// Path is inside test/bench/example/spike territory, where the banned
/// patterns are legitimate (benchmarks may use ControlFlow::Poll etc.).
fn in_test_or_bench(path: &str) -> bool {
    let file_is_test = path.ends_with("_test.rs") || path.ends_with("_tests.rs");
    file_is_test
        || path.split('/').any(|seg| {
            matches!(seg, "tests" | "benches" | "bench" | "examples") || seg.starts_with("spike")
        })
}

fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") // covers //, ///, //!
}

/// Scan one file's content. Pure function — unit-tested below.
fn scan_content(path: &str, content: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let in_src = path.contains("/src/");
    let skip = in_test_or_bench(path);
    let render_path = {
        let lower = path.to_ascii_lowercase();
        lower.contains("render") || lower.contains("paint")
    };
    for (i, line) in content.lines().enumerate() {
        if is_comment_line(line) {
            continue;
        }
        let lineno = i + 1;
        let mut push = |rule: &'static str, severity: Severity| {
            findings.push(Finding {
                rule,
                severity,
                path: path.to_string(),
                line: lineno,
                text: line.trim().to_string(),
            });
        };
        if !skip {
            // §5.2: "ControlFlow::Poll anywhere outside a benchmark" — repaints
            // at display refresh forever; 200-1000x over the P12 idle budget.
            if line.contains("ControlFlow::Poll") {
                push("controlflow-poll", Severity::Fail);
            }
            // §3.4/§9.4: free-running tokio::time::interval banned — the only
            // timer is the armed-on-demand DelayQueue. Allowlist justified uses.
            if line.contains("tokio::time::interval") {
                push("tokio-interval", Severity::Fail);
            }
            // §3.2: "If any channel in the data path is unbounded, we have
            // re-implemented DBeaver." Hard fail inside dbx-core src; warn
            // elsewhere (allowlist justified non-data-path uses).
            if line.contains("unbounded_channel") {
                let sev = if path.contains("dbx-core/src") {
                    Severity::Fail
                } else {
                    Severity::Warn
                };
                push("unbounded-channel", sev);
            }
            // Warning-only: .unwrap() in non-test src code (count reported).
            if in_src && line.contains(".unwrap()") {
                push("unwrap", Severity::Warn);
            }
            // §5.2: "format! in the cell-render path" — allocates per cell per
            // frame; use itoa/ryu into a per-frame arena. Warn on format! in
            // any file whose path mentions render/paint.
            if render_path && line.contains("format!") {
                push("format-in-render", Severity::Warn);
            }
        }
    }
    findings
}

/// Allowlist file format (ci/grep-allowlist.txt): one entry per line,
/// `<rule-id> <path-fragment>`, `#` comments. A finding is allowlisted when an
/// entry's rule matches and its path-fragment is a substring of the path.
fn parse_allowlist(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut tok = line.split_whitespace();
        if let (Some(rule), Some(frag)) = (tok.next(), tok.next()) {
            out.push((rule.to_string(), frag.to_string()));
        }
    }
    out
}

fn is_allowlisted(allow: &[(String, String)], f: &Finding) -> bool {
    allow
        .iter()
        .any(|(rule, frag)| rule == f.rule && f.path.contains(frag.as_str()))
}

fn cmd_grep_gates(args: &[String]) -> ExitCode {
    let mut root: Option<&str> = None;
    let mut allowlist_path: Option<&str> = None;
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        match a {
            "--allowlist" => allowlist_path = it.next(),
            _ if root.is_none() => root = Some(a),
            _ => {
                eprintln!("grep-gates: unexpected argument `{a}`");
                return ExitCode::from(2);
            }
        }
    }
    let root = root.unwrap_or(if Path::new("crates").is_dir() {
        "."
    } else {
        ".."
    });
    match grep_gates(root, allowlist_path) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("grep-gates: {e}");
            ExitCode::from(2)
        }
    }
}

fn grep_gates(root: &str, allowlist_path: Option<&str>) -> Result<bool, String> {
    let root_path = Path::new(root);
    let allow = match allowlist_path {
        Some(p) => {
            parse_allowlist(&fs::read_to_string(p).map_err(|e| format!("read allowlist {p}: {e}"))?)
        }
        None => {
            let default = root_path.join("ci/grep-allowlist.txt");
            if default.is_file() {
                parse_allowlist(&fs::read_to_string(&default).map_err(|e| e.to_string())?)
            } else {
                Vec::new()
            }
        }
    };

    let mut files = Vec::new();
    collect_rs_files(root_path, root_path, &mut files)?;

    let mut fail_count = 0usize;
    let mut warn_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut allowed_count = 0usize;

    for file in &files {
        let rel = file
            .strip_prefix(root_path)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        // Prefix with "/" so rules like `contains("/src/")` work at repo root.
        let rel = format!("/{rel}");
        let Ok(content) = fs::read_to_string(file) else {
            continue; // non-UTF8 or unreadable — not source we gate on
        };
        for f in scan_content(&rel, &content) {
            if is_allowlisted(&allow, &f) {
                allowed_count += 1;
                println!("ALLOW {} {}:{}: {}", f.rule, f.path, f.line, f.text);
                continue;
            }
            match f.severity {
                Severity::Fail => {
                    fail_count += 1;
                    println!("FAIL  {} {}:{}: {}", f.rule, f.path, f.line, f.text);
                }
                Severity::Warn => {
                    *warn_counts.entry(f.rule).or_insert(0) += 1;
                    // Keep unwrap warnings to a count (they can be numerous);
                    // print other warn rules per-finding.
                    if f.rule != "unwrap" {
                        println!("WARN  {} {}:{}: {}", f.rule, f.path, f.line, f.text);
                    }
                }
            }
        }
    }

    for (rule, n) in &warn_counts {
        println!("WARN  {rule}: {n} occurrence(s) (non-blocking)");
    }
    if allowed_count > 0 {
        println!("note: {allowed_count} finding(s) waived via allowlist");
    }
    if fail_count > 0 {
        println!("grep-gates: {fail_count} FAIL finding(s) — see §5.2 of the design doc");
        Ok(false)
    } else {
        println!("grep-gates: OK ({} file(s) scanned)", files.len());
        Ok(true)
    }
}

/// Recursively collect .rs files, skipping directories that are not gated
/// source: VCS/build dirs, this xtask itself, ci/fixtures/.github.
fn collect_rs_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            let skip_anywhere =
                name == ".git" || name == "node_modules" || name.starts_with("target");
            let at_root = dir == root;
            let skip_at_root =
                at_root && matches!(name.as_str(), "xtask" | "ci" | "fixtures" | ".github");
            if skip_anywhere || skip_at_root {
                continue;
            }
            collect_rs_files(root, &path, out)?;
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_BUDGET: &str = r#"
# header comment
[P10]
metric = "installer, compressed"
target = "22MB"
fail = "35MB"

[P11]
metric = "installed on disk"
target = "55MB"
fail = "90MB"

[P16d]
metric = "render pipelines / crates in release graph"
target = "24 / 400"
fail = "60 / 600"

[P15]
metric = "window-shown to first real frame"
target = "16.7ms"                                    # 1 frame
fail = "100ms, or any blank/white frame presented"
"#;

    #[test]
    fn parses_budget_tables_and_inline_comments() {
        let b = parse_budget(SAMPLE_BUDGET).unwrap();
        assert_eq!(b["P10"]["target"], "22MB");
        assert_eq!(b["P11"]["fail"], "90MB");
        assert_eq!(b["P15"]["target"], "16.7ms"); // inline comment stripped
        assert_eq!(b["P16d"]["fail"], "60 / 600");
    }

    #[test]
    fn rejects_non_string_values() {
        assert!(parse_budget("[T]\nkey = 42\n").is_err());
        assert!(parse_budget("key = \"orphan\"\n").is_err());
    }

    #[test]
    fn strip_comment_respects_quotes() {
        assert_eq!(
            strip_comment(r#"fail = "a # b"  # real comment"#).trim(),
            r#"fail = "a # b""#
        );
        assert_eq!(strip_comment("plain"), "plain");
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("22MB").unwrap(), 22_000_000);
        assert_eq!(parse_size("64KB").unwrap(), 64_000);
        assert_eq!(parse_size("4GB").unwrap(), 4_000_000_000);
        assert_eq!(parse_size("512B").unwrap(), 512);
        assert_eq!(parse_size("1234").unwrap(), 1234);
        assert_eq!(parse_size("1.5MB").unwrap(), 1_500_000);
        assert!(parse_size("lots").is_err());
    }

    #[test]
    fn parses_crate_limits_from_pairs() {
        assert_eq!(parse_crate_limit("24 / 400").unwrap(), 400);
        assert_eq!(parse_crate_limit("60 / 600").unwrap(), 600);
        assert_eq!(parse_crate_limit("400").unwrap(), 400);
    }

    #[test]
    fn counts_unique_crates_in_tree_output() {
        let out = "\
dbx-api v0.1.0 (/repo/crates/dbx-api)
serde v1.0.210
serde_derive v1.0.210 (proc-macro)
serde v1.0.210 (*)
thiserror v2.0.3

serde v1.0.100
";
        // serde appears at two versions -> both count; the (*) dedupe repeat
        // and the blank line are ignored.
        assert_eq!(count_unique_crates(out), 5);
    }

    // -- grep rules ---------------------------------------------------------

    #[test]
    fn flags_controlflow_poll_outside_benches() {
        let f = scan_content(
            "/crates/dbx-ui/src/event_loop.rs",
            "let cf = ControlFlow::Poll;\n",
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "controlflow-poll");
        assert_eq!(f[0].severity, Severity::Fail);
        assert_eq!(f[0].line, 1);
    }

    #[test]
    fn ignores_banned_patterns_in_bench_spike_and_test_paths() {
        for path in [
            "/crates/dbx-ui/benches/fling.rs",
            "/crates/spike-ui/src/main.rs",
            "/crates/dbx-core/tests/stream.rs",
            "/crates/dbx-core/src/pipeline_test.rs",
        ] {
            let f = scan_content(path, "ControlFlow::Poll; tokio::time::interval(d);\n");
            assert!(f.is_empty(), "expected no findings for {path}");
        }
    }

    #[test]
    fn ignores_patterns_in_line_comments() {
        let f = scan_content(
            "/crates/dbx-core/src/lib.rs",
            "// ControlFlow::Poll is banned per design §5.2\n/// never use tokio::time::interval\n",
        );
        assert!(f.is_empty());
    }

    #[test]
    fn unbounded_channel_severity_depends_on_crate() {
        let core = scan_content(
            "/crates/dbx-core/src/feeder.rs",
            "let (tx, rx) = mpsc::unbounded_channel();\n",
        );
        assert_eq!(core[0].severity, Severity::Fail);
        let other = scan_content(
            "/crates/dbx-tui/src/input.rs",
            "let (tx, rx) = mpsc::unbounded_channel();\n",
        );
        assert_eq!(other[0].severity, Severity::Warn);
    }

    #[test]
    fn unwrap_warns_only_in_src() {
        let src = scan_content("/crates/dbx-api/src/value.rs", "x.unwrap();\n");
        assert_eq!(src.len(), 1);
        assert_eq!(src[0].rule, "unwrap");
        assert_eq!(src[0].severity, Severity::Warn);
        let build = scan_content("/crates/dbx-api/build.rs", "x.unwrap();\n");
        assert!(build.is_empty());
    }

    #[test]
    fn format_flagged_only_in_render_paths() {
        let render = scan_content(
            "/crates/dbx-ui/src/grid_render.rs",
            "let s = format!(\"{v}\");\n",
        );
        assert_eq!(render.len(), 1);
        assert_eq!(render[0].rule, "format-in-render");
        let plain = scan_content(
            "/crates/dbx-ui/src/layout.rs",
            "let s = format!(\"{v}\");\n",
        );
        assert!(plain.is_empty());
    }

    #[test]
    fn allowlist_waives_by_rule_and_path_fragment() {
        let allow = parse_allowlist(
            "# comment\n\
             tokio-interval  crates/dbx-tui/src/tick.rs  # justified: ratatui redraw\n",
        );
        let hit = Finding {
            rule: "tokio-interval",
            severity: Severity::Fail,
            path: "/crates/dbx-tui/src/tick.rs".into(),
            line: 3,
            text: String::new(),
        };
        assert!(is_allowlisted(&allow, &hit));
        let miss = Finding {
            path: "/crates/dbx-core/src/timer.rs".into(),
            ..hit.clone()
        };
        assert!(!is_allowlisted(&allow, &miss));
        let wrong_rule = Finding {
            rule: "controlflow-poll",
            ..hit
        };
        assert!(!is_allowlisted(&allow, &wrong_rule));
    }

    #[test]
    fn spike_dirs_are_recognized() {
        assert!(in_test_or_bench("/crates/spike-ui/src/main.rs"));
        assert!(in_test_or_bench("/spikes/s1_idle/src/main.rs"));
        assert!(!in_test_or_bench("/crates/dbx-core/src/store.rs"));
    }
}
