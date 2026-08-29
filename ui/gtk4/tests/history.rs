use std::fs;
use std::path::PathBuf;

use datagrep_gtk::model::history::{day_key, hash_text, normalise, now_ms, HistoryStore};
use datagrep_gtk::{HistoryEntry, HistoryFilter, Outcome, QueryStatus, Retention};

/// One line exactly as the Qt (`QJsonDocument::Compact`, sorted keys) and macOS
/// (`JSONEncoder.sortedKeys`) stores write it.
const QT_LINE: &str = r#"{"connection":"prod","durationMs":42,"engine":"postgres","id":"7b6f6c1e-0000-4000-8000-000000000001","outcome":"ok","rowCount":12,"runCount":1,"sql":"SELECT 1","startedAtMs":START,"textHash":"HASH"}"#;

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "datagrep-gtk-history-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        Self { dir }
    }

    fn store(&self) -> HistoryStore {
        HistoryStore::new(self.dir.clone())
    }

    fn day_file(&self, ms: i64) -> String {
        fs::read_to_string(self.dir.join(format!("{}.jsonl", day_key(ms)))).expect("a day file")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn entry(sql: &str, connection: &str, started_at_ms: i64) -> HistoryEntry {
    HistoryEntry {
        connection: connection.to_owned(),
        duration_ms: 42,
        engine: "postgres".to_owned(),
        row_count: Some(12),
        sql: sql.to_owned(),
        started_at_ms,
        ..HistoryEntry::default()
    }
}

#[test]
fn a_day_file_is_the_line_the_other_front_ends_write() {
    let fixture = Fixture::new("format");
    let store = fixture.store();
    let started = now_ms();
    let mut written = entry("SELECT 1", "prod", started);
    written.id = "7b6f6c1e-0000-4000-8000-000000000001".to_owned();
    store.record(written);
    store.flush();

    let expected = QT_LINE
        .replace("START", &started.to_string())
        .replace("HASH", &hash_text("SELECT 1"));
    assert_eq!(fixture.day_file(started), format!("{expected}\n"));
}

#[test]
fn a_line_written_by_the_qt_store_loads_and_round_trips_unchanged() {
    let fixture = Fixture::new("qt-line");
    let started = now_ms();
    let line = QT_LINE
        .replace("START", &started.to_string())
        .replace("HASH", &hash_text("SELECT 1"));
    fs::create_dir_all(&fixture.dir).expect("the history directory");
    fs::write(
        fixture.dir.join(format!("{}.jsonl", day_key(started))),
        format!("{line}\n"),
    )
    .expect("the day file is seeded");

    let store = fixture.store();
    store.load();
    let loaded = store.with_entries(|entries| entries.to_vec());
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].connection, "prod");
    assert_eq!(loaded[0].row_count, Some(12));
    assert_eq!(
        loaded[0].engine, "postgres",
        "a deleted connection still reads"
    );
    assert_eq!(
        serde_json::to_string(&loaded[0]).expect("re-encodes"),
        line,
        "a byte the Qt store wrote must survive a GTK read and write"
    );
}

#[test]
fn retention_is_the_two_key_object_the_other_stores_read() {
    let fixture = Fixture::new("retention-file");
    let store = fixture.store();
    assert_eq!(store.retention(), Retention::default());
    store.set_retention(Retention::clamped(500, 30));

    let text = fs::read_to_string(fixture.dir.join("retention.json")).expect("retention.json");
    assert_eq!(
        text,
        "{\n    \"maxDays\": 30,\n    \"maxEntries\": 500\n}\n"
    );
    assert_eq!(fixture.store().retention(), Retention::clamped(500, 30));
}

#[test]
fn a_retention_below_the_floor_is_raised_rather_than_obeyed() {
    assert_eq!(Retention::clamped(1, 0).max_entries, 100);
    assert_eq!(Retention::clamped(1, 0).max_days, 1);
}

#[test]
fn a_failed_statement_is_kept_with_the_error_that_broke_it() {
    let fixture = Fixture::new("failure");
    let store = fixture.store();
    store.execution_started("SELECT * FROM nope", "prod", "postgres");
    store.execution_progressed(&QueryStatus::parse(
        r#"{"state":"failed","elapsed_ms":7,"error":"relation \"nope\" does not exist"}"#,
    ));

    let kept = store.with_entries(|entries| entries.to_vec());
    assert_eq!(
        kept.len(),
        1,
        "the query you want back is the one that broke"
    );
    assert_eq!(kept[0].outcome, Outcome::Error);
    assert_eq!(kept[0].error, "relation \"nope\" does not exist");
    assert_eq!(kept[0].engine, "postgres");
}

#[test]
fn a_run_that_never_got_a_query_handle_still_lands_in_history() {
    let fixture = Fixture::new("no-handle");
    let store = fixture.store();
    store.execution_started("SELECT 1", "gone", "mysql");
    store.execution_failed_to_start("could not connect");
    store.with_entries(|entries| {
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].error, "could not connect");
        assert_eq!(entries[0].row_count, None, "no result set is not zero rows");
    });
}

#[test]
fn history_spans_every_connection_and_filtering_is_the_users_choice() {
    let fixture = Fixture::new("connections");
    let store = fixture.store();
    let now = now_ms();
    store.record(entry("SELECT 1", "prod", now));
    store.record(entry("SELECT 2", "staging", now));

    assert_eq!(store.connections(), ["prod", "staging"]);
    store.with_entries(|entries| {
        assert_eq!(entries.len(), 2, "the log is never scoped for the user");
        let scoped = HistoryFilter {
            connection: "prod".to_owned(),
            ..HistoryFilter::default()
        }
        .prepare(now);
        assert_eq!(entries.iter().filter(|e| scoped.matches(e)).count(), 1);
    });
}

#[test]
fn a_repeat_inside_the_dedupe_window_collapses_into_one_entry() {
    let fixture = Fixture::new("dedupe");
    let store = fixture.store();
    let now = now_ms();
    store.record(entry("SELECT 1", "prod", now - 60_000));
    store.record(entry("select   1 ;", "prod", now));

    store.with_entries(|entries| {
        assert_eq!(entries.len(), 2, "normalising is whitespace, not case");
    });
    store.record(entry("SELECT 1", "prod", now));
    store.with_entries(|entries| {
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].run_count, 2);
        assert_eq!(entries[0].started_at_ms, now);
    });
}

#[test]
fn the_search_reads_the_error_as_well_as_the_sql() {
    let now = now_ms();
    let mut broken = entry("SELECT * FROM nope", "prod", now);
    broken.outcome = Outcome::Error;
    broken.error = "relation does not exist".to_owned();
    let filter = HistoryFilter {
        text: "RELATION".to_owned(),
        ..HistoryFilter::default()
    }
    .prepare(now);
    assert!(filter.matches(&broken));
}

#[test]
fn the_entry_ceiling_drops_the_oldest_and_takes_its_day_file_with_it() {
    let fixture = Fixture::new("prune");
    let store = fixture.store();
    store.set_retention(Retention::clamped(100, 180));
    let now = now_ms();
    for i in 0..120 {
        store.record(entry(&format!("SELECT {i}"), "prod", now - i64::from(i)));
    }
    store.flush();

    store.with_entries(|entries| {
        assert_eq!(entries.len(), 100);
        assert_eq!(entries[0].sql, "SELECT 0", "newest first");
        assert_eq!(entries[99].sql, "SELECT 99");
    });
    assert_eq!(fixture.day_file(now).lines().count(), 100);
}

#[test]
fn a_day_outside_the_window_loses_its_file_on_the_next_write() {
    let fixture = Fixture::new("old-day");
    let store = fixture.store();
    let now = now_ms();
    let ancient = now - 400 * 86_400_000;
    fs::create_dir_all(&fixture.dir).expect("the history directory");
    let ancient_file = fixture.dir.join(format!("{}.jsonl", day_key(ancient)));
    fs::write(&ancient_file, "{\"sql\":\"SELECT 1\"}\n").expect("the old day file");

    store.record(entry("SELECT 1", "prod", now));
    store.flush();
    assert!(
        !ancient_file.exists(),
        "past the retention window, and gone"
    );
}

#[test]
fn the_text_hash_is_the_fnv_1a_the_other_stores_compute() {
    assert_eq!(normalise("  SELECT\n\t1 ;  "), "SELECT 1");
    assert_eq!(hash_text("SELECT 1"), hash_text("  SELECT\n  1;  "));
    // FNV-1a over "SELECT 1", the value both other stores print in base 16.
    assert_eq!(hash_text("SELECT 1"), "199e7bca63ea84f2");
}

#[test]
fn a_multiline_statement_reads_as_one_line_in_the_list() {
    let entry = entry("SELECT *\n  FROM users\n\n WHERE id = 1", "prod", now_ms());
    assert_eq!(entry.one_line(), "SELECT * FROM users WHERE id = 1");
}
