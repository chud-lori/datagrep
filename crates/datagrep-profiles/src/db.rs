use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, Transaction};

use crate::error::ProfilesError;
use crate::model::now_ms;

#[derive(Debug, Clone)]
pub(crate) enum Target {
    File(PathBuf),
    Memory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub max_rows: i64,
    pub max_age_days: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        RetentionPolicy {
            max_rows: 20_000,
            max_age_days: 180,
        }
    }
}

pub(crate) struct Db {
    pub(crate) conn: Connection,
    pub(crate) fts5_available: bool,
}

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
            conn.pragma_update(None, "journal_mode", "WAL")?;
            (conn, Some(path.clone()))
        }
        Target::Memory => {
            let conn = Connection::open_in_memory()?;
            (conn, None)
        }
    };

    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_millis(5_000))?;

    migrate(&mut conn, backup_path.as_deref())?;
    let fts5_available = fts5_table_exists(&conn)?;
    trim_retention(&conn, retention)?;

    Ok(Db {
        conn,
        fts5_available,
    })
}

type MigrationFn = fn(&Transaction<'_>) -> rusqlite::Result<()>;

const MIGRATIONS: &[MigrationFn] = &[migrate_v1, migrate_v2];

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
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(version, "datagrep-profiles: applied migration");
    }

    Ok(())
}

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

fn migrate_v2(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    // Nothing to do for a database created after this landed.
    let has_env: bool = tx
        .prepare("SELECT 1 FROM pragma_table_info('profile') WHERE name = 'env'")?
        .exists([])?;
    if !has_env {
        return Ok(());
    }

    tx.execute_batch(
        "CREATE TABLE profile_new (
            id              TEXT PRIMARY KEY,
            folder_id       TEXT REFERENCES folder(id) ON DELETE SET NULL,
            name            TEXT NOT NULL,
            driver_id       TEXT NOT NULL,
            config_json     TEXT NOT NULL,
            secret_ref      TEXT,
            tunnel_id       TEXT REFERENCES tunnel(id) ON DELETE SET NULL,
            color           TEXT,
            read_only       INTEGER NOT NULL DEFAULT 0,
            confirm_writes  INTEGER NOT NULL DEFAULT 0,
            auto_limit      INTEGER,
            idle_timeout_s  INTEGER,
            last_used_at    INTEGER,
            created_at      INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL
        );

        INSERT INTO profile_new
            SELECT id, folder_id, name, driver_id, config_json, secret_ref,
                   tunnel_id, color, read_only, confirm_writes, auto_limit,
                   idle_timeout_s, last_used_at, created_at, updated_at
            FROM profile;

        DROP TABLE profile;
        ALTER TABLE profile_new RENAME TO profile;
        CREATE INDEX ix_profile_folder ON profile(folder_id);",
    )
}

fn fts5_table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='query_history_fts'",
        [],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

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

pub(crate) fn hash_text(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
