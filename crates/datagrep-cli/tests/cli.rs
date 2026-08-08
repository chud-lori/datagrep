//! Black-box, end-to-end tests against the real compiled `datagrep` binary
//! (`env!("CARGO_BIN_EXE_datagrep")`) — no mocking, no reaching into the crate's
//! internals. Every test gets its own isolated `DATAGREP_CONFIG_DIR` (a tempdir)
//! so profiles/history from one test can never leak into another, and tests
//! stay safe to run in parallel (the default) against a shared real
//! `~/.config/datagrep` would not be.
//!
//! Covers, per the ticket's test list:
//! - every output format end to end against SQLite;
//! - NULL vs empty-string vs missing-column rendering;
//! - a multi-statement script;
//! - `@limit` actually capping;
//! - `@readonly` blocking a write;
//! - `profiles add` splitting an inline password into a `secret_ref`;
//! - `catalog` listing one level lazily;
//! - exit codes per failure class;
//! - `datagrep doctor` clean with zero profiles.

use std::path::Path;
use std::process::{Command, Output};

fn datagrep() -> Command {
    Command::new(env!("CARGO_BIN_EXE_datagrep"))
}

/// One isolated `$DATAGREP_CONFIG_DIR`, torn down with the `TempDir` at the end
/// of the test.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn config_dir(&self) -> &Path {
        self.dir.path()
    }

    fn cmd(&self) -> Command {
        let mut c = datagrep();
        c.env("DATAGREP_CONFIG_DIR", self.config_dir());
        c.env_remove("NO_COLOR");
        // Deterministic table width across whatever terminal happens to run
        // the test suite.
        c.env("COLUMNS", "120");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("spawn datagrep")
    }

    fn sqlite_path(&self) -> std::path::PathBuf {
        self.dir.path().join("data.db")
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

// ---------------------------------------------------------------------
// Cold-start / usage
// ---------------------------------------------------------------------

#[test]
fn help_exits_zero_and_never_touches_a_profile_store() {
    let sb = Sandbox::new();
    let out = sb.run(&["--help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("datagrep"));
    // Nothing was created — `--help` never opened the profile store.
    assert!(!sb.config_dir().join("profiles.db").exists());
}

#[test]
fn profiles_list_with_zero_profiles_is_near_instant_and_exits_zero() {
    let sb = Sandbox::new();
    let started = std::time::Instant::now();
    let out = sb.run(&["profiles", "list"]);
    let elapsed = started.elapsed();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("no profiles"));
    assert!(
        elapsed.as_millis() < 2000,
        "profiles list took {elapsed:?}, expected near-instant (design P1)"
    );
}

#[test]
fn missing_required_flag_is_a_usage_error_exit_2() {
    let sb = Sandbox::new();
    let out = sb.run(&["query", "-c", "select 1"]); // no --profile
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn unknown_profile_is_a_usage_error_exit_2() {
    let sb = Sandbox::new();
    let out = sb.run(&["query", "--profile", "nope", "-c", "select 1"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("no profile named"));
}

#[test]
fn doctor_is_clean_with_zero_profiles() {
    let sb = Sandbox::new();
    let out = sb.run(&["doctor"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("drivers:"));
    assert!(text.contains("sqlite"));
    assert!(text.contains("postgres"));
    assert!(text.contains("profiles: 0 configured"));
}

// ---------------------------------------------------------------------
// profiles add / show / export — secrets never touch disk in plaintext
// ---------------------------------------------------------------------

#[test]
fn profiles_add_sqlite_then_list_then_show() {
    let sb = Sandbox::new();
    let url = format!("sqlite://{}", sb.sqlite_path().display());
    let out = sb.run(&["profiles", "add", "local", &url]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = sb.run(&["profiles", "list"]);
    assert!(stdout(&out).contains("local"));
    assert!(stdout(&out).contains("sqlite"));

    let out = sb.run(&["profiles", "show", "local"]);
    let text = stdout(&out);
    assert!(text.contains("driver:     sqlite"));
    assert!(text.contains("secret:     (none)"));
}

#[test]
fn profiles_add_postgres_url_with_inline_password_never_stores_it() {
    let sb = Sandbox::new();
    let out = sb.run(&[
        "profiles",
        "add",
        "staging",
        "postgres://alice:hunter2@localhost:5432/app",
    ]);
    // This test drives the shipped binary, so it uses the real OS credential
    // store and cannot be pointed at the in-memory one the unit tests use. A
    // bare CI runner has no Secret Service on the session bus, so `add` fails
    // there for reasons that have nothing to do with what is being asserted.
    // Skip loudly rather than fail, and rather than pretend to have run.
    if !out.status.success() && stderr(&out).contains("secure storage") {
        eprintln!(
            "SKIPPED {}: no OS credential store on this machine",
            "profiles_add_postgres_url_with_inline_password_never_stores_it"
        );
        return;
    }
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("secret stored in the OS keychain"));

    let out = sb.run(&["profiles", "show", "staging"]);
    let text = stdout(&out);
    assert!(text.contains("secret:     ••••"));
    assert!(!text.contains("hunter2"));

    // The exported, git-committable TOML must not contain the password
    // either — only a `secret_ref` pointing at the keychain.
    let out = sb.run(&["profiles", "export"]);
    let toml = stdout(&out);
    assert!(!toml.contains("hunter2"));
    assert!(toml.contains("secret_ref"));
    assert!(toml.contains("keychain:datagrep:"));

    // Clean up the real keychain entry this test created.
    //
    // This test drives the shipped binary as a subprocess, so unlike the unit
    // tests it cannot be pointed at an in-memory secret store — it really does
    // write to the OS keychain, and really does have to clear up after itself.
    //
    // The account is read back out of the exported TOML rather than guessed.
    // It is the profile's generated id (`<id>:password`), NOT the profile name,
    // so the previous hard-coded `-a staging` never matched anything and every
    // single run leaked one entry — 35 had piled up on one machine before this
    // was noticed. Deleting by an account taken from this test's own output is
    // also what keeps the deletion precise.
    //
    // Scoping by BOTH service and account stays mandatory: `-s datagrep` alone
    // deletes the first entry for that service — i.e. one of the user's own
    // saved connections. That destroyed real credentials before it was caught.
    let account = toml
        .split("keychain:datagrep:")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .map(str::to_string)
        .expect("the exported TOML asserted a keychain: secret_ref above");
    // `security` is a macOS tool; on Linux the equivalent entry lives in
    // whatever Secret Service implementation is running and there is no
    // portable CLI to remove it, so the assertion is scoped to where it can
    // actually be enforced.
    if cfg!(target_os = "macos") {
        let cleaned = Command::new("security")
            .args(["delete-generic-password", "-s", "datagrep", "-a", &account])
            .output();
        assert!(
            cleaned.is_ok_and(|o| o.status.success()),
            "failed to remove the keychain entry `{account}` this test created; \
             left behind it would accumulate on every run"
        );
    }
}

// ---------------------------------------------------------------------
// query: every format, NULL/empty/multi-statement/@limit/@readonly
// ---------------------------------------------------------------------

fn seed_table(sb: &Sandbox) {
    let url = format!("sqlite://{}", sb.sqlite_path().display());
    let out = sb.run(&["profiles", "add", "db", &url]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "-c",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, note TEXT); \
         INSERT INTO t (id, name, note) VALUES (1, 'alice', NULL); \
         INSERT INTO t (id, name, note) VALUES (2, 'bob', '');",
    ]);
    assert!(out.status.success(), "seed failed: {}", stderr(&out));
}

#[test]
fn query_table_format_distinguishes_null_from_empty_string() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "-c",
        "SELECT id, name, note FROM t ORDER BY id",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    let alice_line = lines.iter().find(|l| l.contains("alice")).unwrap();
    let bob_line = lines.iter().find(|l| l.contains("bob")).unwrap();
    assert!(alice_line.contains("NULL"), "row: {alice_line}");
    assert!(!bob_line.contains("NULL"), "row: {bob_line}");
    assert!(text.contains("(2 rows)"));
}

#[test]
fn query_json_format_is_one_array_of_objects() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--format",
        "json",
        "-c",
        "SELECT id, name, note FROM t ORDER BY id",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid json");
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], serde_json::json!("alice"));
    assert_eq!(rows[0]["note"], serde_json::Value::Null);
    assert_eq!(rows[1]["note"], serde_json::json!(""));
}

#[test]
fn query_ndjson_format_is_one_object_per_line() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--format",
        "ndjson",
        "-c",
        "SELECT id FROM t ORDER BY id",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line).expect("each line is valid json");
    }
}

#[test]
fn query_csv_and_tsv_formats() {
    let sb = Sandbox::new();
    seed_table(&sb);

    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--format",
        "csv",
        "-c",
        "SELECT id, name FROM t ORDER BY id",
    ]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "id,name\r\n1,alice\r\n2,bob\r\n");

    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--format",
        "tsv",
        "-c",
        "SELECT id, name FROM t ORDER BY id",
    ]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "id\tname\r\n1\talice\r\n2\tbob\r\n");
}

#[test]
fn query_missing_column_via_a_query_alias_reads_as_null_not_a_crash() {
    // No driver in this build produces a genuinely `Absent` cell (only
    // sqlite/postgres, both `Shape::Table`), so this proves the adjacent,
    // always-reachable claim: a column that is NULL for every row still
    // gets a header and renders NULL, never panics or silently vanishes.
    let sb = Sandbox::new();
    seed_table(&sb);
    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--format",
        "json",
        "-c",
        "SELECT id, NULL AS missing_col FROM t ORDER BY id",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(json[0]["missing_col"], serde_json::Value::Null);
}

#[test]
fn multi_statement_script_runs_every_statement_in_order() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let script = "SELECT id FROM t WHERE id = 1;\nSELECT id FROM t WHERE id = 2;\n";
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("script.sql");
    std::fs::write(&script_path, script).unwrap();

    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--format",
        "ndjson",
        "-f",
        script_path.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    // Scalars keep their JSON types (`{"id":1}`, never `{"id":"1"}`) — the
    // README's `--format json | jq` pitch depends on it. See `value_text.rs`.
    assert!(text.contains("\"id\":1"), "got: {text}");
    assert!(text.contains("\"id\":2"), "got: {text}");
}

#[test]
fn limit_directive_caps_the_rows_actually_printed() {
    let sb = Sandbox::new();
    seed_table(&sb);
    // `--command=<value>` (one joined arg), not `-c <value>` (two args):
    // clap treats a *following* argument that starts with `-` as an
    // ambiguous new flag, and `-- @limit ...` starts with `--`. Real users
    // hit the identical clap behavior; `-f a-file.sql` sidesteps it, which
    // is what the other multi-statement test above uses.
    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--format",
        "ndjson",
        "--command=-- @limit 1\nSELECT id FROM t ORDER BY id",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "expected exactly 1 row, got: {lines:?}");
}

#[test]
fn cli_limit_flag_caps_rows_the_same_way_as_the_directive() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--format",
        "ndjson",
        "--limit",
        "1",
        "-c",
        "SELECT id FROM t ORDER BY id",
    ]);
    assert!(out.status.success());
    assert_eq!(stdout(&out).lines().count(), 1);
}

#[test]
fn readonly_directive_blocks_a_write_and_exits_1() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--command=-- @readonly\nDELETE FROM t",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("read-only guard"));

    // And the table is untouched.
    let out = sb.run(&[
        "query",
        "--profile",
        "db",
        "--format",
        "ndjson",
        "-c",
        "SELECT id FROM t",
    ]);
    assert_eq!(stdout(&out).lines().count(), 2);
}

#[test]
fn empty_input_is_a_usage_error() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let out = sb.run(&["query", "--profile", "db", "-c", "   "]);
    assert_eq!(out.status.code(), Some(2));
}

// ---------------------------------------------------------------------
// export
// ---------------------------------------------------------------------

#[test]
fn export_streams_to_disk_and_reports_progress_on_stderr_not_stdout() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let out_path = sb.dir.path().join("out.csv");
    let out = sb.run(&[
        "export",
        "--profile",
        "db",
        "-c",
        "SELECT id, name FROM t ORDER BY id",
        "--format",
        "csv",
        "-o",
        out_path.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "",
        "stdout must stay pipeable/empty for export"
    );
    assert!(stderr(&out).contains("rows"), "{}", stderr(&out));

    let contents = std::fs::read_to_string(&out_path).unwrap();
    assert_eq!(contents, "id,name\r\n1,alice\r\n2,bob\r\n");
}

// ---------------------------------------------------------------------
// catalog: one level, lazily
// ---------------------------------------------------------------------

#[test]
fn catalog_lists_tables_at_the_root_without_columns() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let out = sb.run(&["catalog", "--profile", "db"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    // The root level is schemas/databases for sqlite ("main"), not the
    // table itself — proves this is one level, not a crawl straight to
    // columns.
    assert!(
        !text.contains("id"),
        "root listing leaked column names: {text}"
    );
}

#[test]
fn catalog_describe_shows_declared_columns() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let out = sb.run(&["catalog", "--profile", "db", "--describe", "main.t"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("id"));
    assert!(text.contains("name"));
}

// ---------------------------------------------------------------------
// history: query actually records something to look at
// ---------------------------------------------------------------------

#[test]
fn history_list_shows_a_statement_that_was_run() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let _ = sb.run(&[
        "query",
        "--profile",
        "db",
        "-c",
        "SELECT id FROM t WHERE id = 1",
    ]);
    let out = sb.run(&["history", "list", "--profile", "db"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("SELECT id FROM t WHERE id = 1"));
}

#[test]
fn history_search_finds_by_word() {
    let sb = Sandbox::new();
    seed_table(&sb);
    let _ = sb.run(&[
        "query",
        "--profile",
        "db",
        "-c",
        "SELECT id FROM t WHERE id = 1",
    ]);
    let out = sb.run(&["history", "search", "WHERE"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("WHERE id = 1"));
}
