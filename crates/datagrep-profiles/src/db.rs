//! Schema, migrations, and open-time maintenance.
//!
//! Everything in this module runs synchronously on the store's dedicated
//! worker thread (see `store.rs`) — never on an async task.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, Transaction};

use crate::error::ProfilesError;
use crate::model::now_ms;

/// Where the database lives. `Memory` is used by tests and by embedders that
/// want a throwaway store; it skips backup snapshots (nothing to protect).
#[derive(Debug, Clone)]
pub(crate) enum Target {
    File(PathBuf),
    Memory,
}

/// History retention policy, enforced on every open. datagrep runs no
/// background flush and no timers anywhere, so trimming happens at the one
/// moment the database is already being touched rather than on a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Keep at most this many `query_history` rows (newest first).
    pub max_rows: i64,
    /// Drop rows older than this many days, regardless of `max_rows`.
    pub max_age_days: i64,
}

impl Default for RetentionPolicy {
    /// 20k rows or 180 days, whichever bites first.
    fn default() -> Self {
        RetentionPolicy {
            max_rows: 20_000,
            max_age_days: 180,
        }
    }
}

/// An opened, migrated, retention-trimmed connection plus the one piece of
/// runtime-detected capability the rest of the crate needs to know: whether
/// this SQLite build has the FTS5 extension compiled in.
pub(crate) struct Db {
    pub(crate) conn: Connection,
    pub(crate) fts5_available: bool,
}

/// Opens (creating parent directories as needed), migrates, detects FTS5,
/// and retention-trims — the full "first real call" path (design: "opened
/// lazily off the startup path").
pub(crate) fn open_and_prepare(
    target: &Target,
    retention: RetentionPolicy,
) -> Result<Db, ProfilesError> {
    let (mut conn, backup_path) = match target {
        Target::File(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let conn = Connection::open(path)?;
            // WAL only makes sense for a real file; must be set before we
            // start issuing other statements.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            (conn, Some(path.clone()))
        }
        Target::Memory => {
            let conn = Connection::open_in_memory()?;
            (conn, None)
        }
    };

    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Defense in depth: this crate serializes all access onto one worker
    // thread and one connection, so we should never generate SQLITE_BUSY
    // against ourselves — but an external tool (e.g. a "open the db file in
    // another app" debugging session) sharing the same file could, and a
    // busy_timeout means that surfaces as a short stall, not an error.
    conn.busy_timeout(Duration::from_millis(5_000))?;

    migrate(&mut conn, backup_path.as_deref())?;
    let fts5_available = fts5_table_exists(&conn)?;
    trim_retention(&conn, retention)?;

    Ok(Db {
        conn,
        fts5_available,
    })
}

/// One forward-only migration step. `up` runs inside its own transaction;
/// returning `Err` rolls that transaction back and aborts the whole open.
type MigrationFn = fn(&Transaction<'_>) -> rusqlite::Result<()>;

/// Migrations are append-only. Version N is `MIGRATIONS[N - 1]`; the current
/// schema version lives in SQLite's own `PRAGMA user_version`, so the
/// database carries its own version and no side-car file can drift from it.
const MIGRATIONS: &[MigrationFn] = &[migrate_v1];

/// Brings `conn` from its current `user_version` up to `MIGRATIONS.len()`.
/// Refuses to run against a database from a *newer* schema version. Snapshots
/// `<path>.bak` before applying anything.
pub(crate) fn migrate(
    conn: &mut Connection,
    backup_path: Option<&Path>,
) -> Result<(), ProfilesError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let target = MIGRATIONS.len() as i64;

    if current > target {
        return Err(ProfilesError::FutureSchema {
            found: current,
            supported: target,
        });
    }
    if current == target {
        return Ok(());
    }

    if let Some(path) = backup_path {
        backup_before_migrate(conn, path)?;
    }

    for (idx, migration) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = (idx + 1) as i64;
        let tx = conn.transaction()?;
        migration(&tx)?;
        // `user_version` inside the same transaction: either the whole
        // migration lands, schema and version bump together, or neither does.
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(version, "datagrep-profiles: applied migration");
    }

    Ok(())
}

/// Copies the live database to `<path>.bak` before a migration touches it.
/// WAL content is checkpointed into the main file first so the copy is a
/// complete, self-consistent snapshot rather than a stale base file.
fn backup_before_migrate(conn: &Connection, path: &Path) -> Result<(), ProfilesError> {
    let _: i64 = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
        .unwrap_or(0);

    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, PathBuf::from(bak))?;
    }
    Ok(())
}

/// The base schema, minus a `plugin` table: no plugin host exists yet, so
/// there is nothing for it to reference and adding it now would just be dead
/// DDL to migrate again once the plugin host's actual shape (sha256, granted
/// hosts) is known.
const BASE_SCHEMA_SQL: &str = r#"
CREATE TABLE folder (
    id          TEXT PRIMARY KEY,
    parent_id   TEXT REFERENCES folder(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    sort_order  INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE tunnel (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    host             TEXT NOT NULL,
    port             INTEGER NOT NULL,
    username         TEXT NOT NULL,
    secret_ref       TEXT,
    known_hosts_pin  TEXT,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE TABLE profile (
    id              TEXT PRIMARY KEY,
    folder_id       TEXT REFERENCES folder(id) ON DELETE SET NULL,
    name            TEXT NOT NULL,
    driver_id       TEXT NOT NULL,
    config_json     TEXT NOT NULL,
    secret_ref      TEXT,
    tunnel_id       TEXT REFERENCES tunnel(id) ON DELETE SET NULL,
    env             TEXT NOT NULL DEFAULT 'dev' CHECK (env IN ('dev','staging','prod')),
    color           TEXT,
    read_only       INTEGER NOT NULL DEFAULT 0,
    confirm_writes  INTEGER NOT NULL DEFAULT 0,
    auto_limit      INTEGER,
    idle_timeout_s  INTEGER,
    last_used_at    INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX ix_profile_folder ON profile(folder_id);

CREATE TABLE query_history (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id   TEXT NOT NULL REFERENCES profile(id) ON DELETE CASCADE,
    text         TEXT NOT NULL,
    text_hash    TEXT NOT NULL,
    started_at   INTEGER NOT NULL,
    duration_ms  INTEGER,
    row_count    INTEGER,
    status       TEXT NOT NULL CHECK (status IN ('ok','error','cancelled')),
    error        TEXT
);
CREATE INDEX ix_hist_profile_time ON query_history(profile_id, started_at DESC);
CREATE INDEX ix_hist_hash ON query_history(text_hash);

CREATE TABLE saved_query (
    id           TEXT PRIMARY KEY,
    folder_id    TEXT REFERENCES folder(id) ON DELETE SET NULL,
    profile_id   TEXT REFERENCES profile(id) ON DELETE SET NULL,
    name         TEXT NOT NULL,
    text         TEXT NOT NULL,
    params_json  TEXT,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

-- Unused. Editor tabs live in ~/Library/Application Support/datagrep/tabs as
-- one .sql file plus a JSON sidecar per tab, which is deliberate: a saved query
-- is a file you can open in any editor and commit to git. No ABI entry point
-- ever reached this table and no row was ever written to it, so its Rust side
-- has been removed rather than left as a second, dead store. The DDL stays
-- because dropping it would mean a forward migration of a database holding real
-- connections, to reclaim nothing.
CREATE TABLE editor_tab (
    id           TEXT PRIMARY KEY,
    profile_id   TEXT REFERENCES profile(id) ON DELETE SET NULL,
    title        TEXT,
    text         TEXT NOT NULL,
    cursor_pos   INTEGER,
    sort_order   INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL
);

CREATE TABLE kv (
    "key"    TEXT PRIMARY KEY,
    "value"  TEXT NOT NULL
);
"#;

/// FTS5 external-content index over `query_history.text`, kept in sync by
/// triggers. Applied as a separate, independently-failable block so a
/// SQLite build without the FTS5 extension still gets the rest of the
/// schema — `Store` falls back to a `LIKE` scan (design says degrade
/// gracefully, not refuse to open).
const FTS5_SQL: &str = r#"
CREATE VIRTUAL TABLE query_history_fts USING fts5(
    text,
    content='query_history',
    content_rowid='id'
);

CREATE TRIGGER query_history_ai AFTER INSERT ON query_history BEGIN
    INSERT INTO query_history_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE TRIGGER query_history_ad AFTER DELETE ON query_history BEGIN
    INSERT INTO query_history_fts(query_history_fts, rowid, text) VALUES('delete', old.id, old.text);
END;

CREATE TRIGGER query_history_au AFTER UPDATE ON query_history BEGIN
    INSERT INTO query_history_fts(query_history_fts, rowid, text) VALUES('delete', old.id, old.text);
    INSERT INTO query_history_fts(rowid, text) VALUES (new.id, new.text);
END;
"#;

fn migrate_v1(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(BASE_SCHEMA_SQL)?;
    match tx.execute_batch(FTS5_SQL) {
        Ok(()) => tracing::debug!("datagrep-profiles: FTS5 available, query_history_fts created"),
        Err(err) => tracing::warn!(
            %err,
            "datagrep-profiles: FTS5 unavailable in this SQLite build; query_history search will use a LIKE fallback"
        ),
    }
    Ok(())
}

fn fts5_table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='query_history_fts'",
        [],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Deletes rows past the retention window. Runs once at open time, not on a
/// timer — this crate starts no background threads of its own.
pub(crate) fn trim_retention(
    conn: &Connection,
    retention: RetentionPolicy,
) -> Result<(), ProfilesError> {
    let cutoff = now_ms().saturating_sub(retention.max_age_days.saturating_mul(24 * 3_600 * 1_000));
    conn.execute(
        "DELETE FROM query_history WHERE started_at < ?1",
        params![cutoff],
    )?;
    conn.execute(
        "DELETE FROM query_history WHERE id IN (\
            SELECT id FROM query_history ORDER BY started_at DESC LIMIT -1 OFFSET ?1\
         )",
        params![retention.max_rows],
    )?;
    Ok(())
}

/// Cheap, non-cryptographic hash used only as a dedupe/index key for
/// `query_history.text_hash`. `DefaultHasher` (SipHash) avoids
/// pulling in a sha2 dependency this crate has no other use for; collisions
/// only cost a missed dedupe, never a correctness or security property.
pub(crate) fn hash_text(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
