//! All SQL for the store's tables. Every function here runs synchronously on
//! the worker thread (see `store.rs`) — nothing in this module is async.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::db::{hash_text, Db};
use crate::error::ProfilesError;
use crate::model::{
    EditorTab, Env, Folder, HistoryEntry, HistoryStatus, NewHistoryEntry, Profile, SavedQuery,
    Tunnel,
};

// ---------------------------------------------------------------------
// folder
// ---------------------------------------------------------------------

fn folder_from_row(row: &Row<'_>) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: row.get("id")?,
        parent_id: row.get("parent_id")?,
        name: row.get("name")?,
        sort_order: row.get("sort_order")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn create_folder(conn: &Connection, f: Folder) -> Result<Folder, ProfilesError> {
    conn.execute(
        "INSERT INTO folder (id, parent_id, name, sort_order, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            f.id,
            f.parent_id,
            f.name,
            f.sort_order,
            f.created_at,
            f.updated_at
        ],
    )?;
    Ok(f)
}

pub(crate) fn get_folder(conn: &Connection, id: &str) -> Result<Option<Folder>, ProfilesError> {
    conn.query_row(
        "SELECT * FROM folder WHERE id = ?1",
        params![id],
        folder_from_row,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn list_folders(conn: &Connection) -> Result<Vec<Folder>, ProfilesError> {
    let mut stmt = conn.prepare("SELECT * FROM folder ORDER BY sort_order, name")?;
    let rows = stmt.query_map([], folder_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub(crate) fn update_folder(conn: &Connection, f: Folder) -> Result<Folder, ProfilesError> {
    let changed = conn.execute(
        "UPDATE folder SET parent_id = ?2, name = ?3, sort_order = ?4, updated_at = ?5 WHERE id = ?1",
        params![f.id, f.parent_id, f.name, f.sort_order, f.updated_at],
    )?;
    if changed == 0 {
        return Err(ProfilesError::NotFound {
            what: "folder",
            id: f.id,
        });
    }
    Ok(f)
}

pub(crate) fn delete_folder(conn: &Connection, id: &str) -> Result<(), ProfilesError> {
    conn.execute("DELETE FROM folder WHERE id = ?1", params![id])?;
    Ok(())
}

// ---------------------------------------------------------------------
// profile
// ---------------------------------------------------------------------

/// Raw columns before `config_json`/`env` are decoded — lets us keep the
/// SQLite row-mapping closure infallible (`rusqlite::Result`) and do the
/// fallible JSON/enum parsing afterwards as a `ProfilesError`.
struct ProfileRow {
    id: String,
    folder_id: Option<String>,
    name: String,
    driver_id: String,
    config_json: String,
    secret_ref: Option<String>,
    tunnel_id: Option<String>,
    env: String,
    color: Option<String>,
    read_only: bool,
    confirm_writes: bool,
    auto_limit: Option<i64>,
    idle_timeout_s: Option<i64>,
    last_used_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

fn profile_row_from_row(row: &Row<'_>) -> rusqlite::Result<ProfileRow> {
    Ok(ProfileRow {
        id: row.get("id")?,
        folder_id: row.get("folder_id")?,
        name: row.get("name")?,
        driver_id: row.get("driver_id")?,
        config_json: row.get("config_json")?,
        secret_ref: row.get("secret_ref")?,
        tunnel_id: row.get("tunnel_id")?,
        env: row.get("env")?,
        color: row.get("color")?,
        read_only: row.get("read_only")?,
        confirm_writes: row.get("confirm_writes")?,
        auto_limit: row.get("auto_limit")?,
        idle_timeout_s: row.get("idle_timeout_s")?,
        last_used_at: row.get("last_used_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn profile_from_row(row: ProfileRow) -> Result<Profile, ProfilesError> {
    let config = serde_json::from_str(&row.config_json)?;
    let env = Env::parse(&row.env).ok_or(ProfilesError::InvalidEnv(row.env))?;
    Ok(Profile {
        id: row.id,
        folder_id: row.folder_id,
        name: row.name,
        driver_id: row.driver_id,
        config,
        secret_ref: row.secret_ref,
        tunnel_id: row.tunnel_id,
        env,
        color: row.color,
        read_only: row.read_only,
        confirm_writes: row.confirm_writes,
        auto_limit: row.auto_limit,
        idle_timeout_s: row.idle_timeout_s,
        last_used_at: row.last_used_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

pub(crate) fn create_profile(conn: &Connection, p: Profile) -> Result<Profile, ProfilesError> {
    let config_json = serde_json::to_string(&p.config)?;
    conn.execute(
        "INSERT INTO profile (
            id, folder_id, name, driver_id, config_json, secret_ref, tunnel_id, env, color,
            read_only, confirm_writes, auto_limit, idle_timeout_s, last_used_at, created_at, updated_at
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
        params![
            p.id, p.folder_id, p.name, p.driver_id, config_json, p.secret_ref, p.tunnel_id,
            p.env.as_str(), p.color, p.read_only, p.confirm_writes, p.auto_limit,
            p.idle_timeout_s, p.last_used_at, p.created_at, p.updated_at,
        ],
    )?;
    Ok(p)
}

pub(crate) fn get_profile(conn: &Connection, id: &str) -> Result<Option<Profile>, ProfilesError> {
    let raw = conn
        .query_row(
            "SELECT * FROM profile WHERE id = ?1",
            params![id],
            profile_row_from_row,
        )
        .optional()?;
    raw.map(profile_from_row).transpose()
}

pub(crate) fn list_profiles(
    conn: &Connection,
    folder_id: Option<&str>,
) -> Result<Vec<Profile>, ProfilesError> {
    let mut rows = Vec::new();
    match folder_id {
        Some(fid) => {
            let mut stmt =
                conn.prepare("SELECT * FROM profile WHERE folder_id = ?1 ORDER BY name")?;
            for r in stmt.query_map(params![fid], profile_row_from_row)? {
                rows.push(r?);
            }
        }
        None => {
            let mut stmt = conn.prepare("SELECT * FROM profile ORDER BY name")?;
            for r in stmt.query_map([], profile_row_from_row)? {
                rows.push(r?);
            }
        }
    }
    rows.into_iter().map(profile_from_row).collect()
}

pub(crate) fn update_profile(conn: &Connection, p: Profile) -> Result<Profile, ProfilesError> {
    let config_json = serde_json::to_string(&p.config)?;
    let changed = conn.execute(
        "UPDATE profile SET
            folder_id = ?2, name = ?3, driver_id = ?4, config_json = ?5, secret_ref = ?6,
            tunnel_id = ?7, env = ?8, color = ?9, read_only = ?10, confirm_writes = ?11,
            auto_limit = ?12, idle_timeout_s = ?13, updated_at = ?14
         WHERE id = ?1",
        params![
            p.id,
            p.folder_id,
            p.name,
            p.driver_id,
            config_json,
            p.secret_ref,
            p.tunnel_id,
            p.env.as_str(),
            p.color,
            p.read_only,
            p.confirm_writes,
            p.auto_limit,
            p.idle_timeout_s,
            p.updated_at,
        ],
    )?;
    if changed == 0 {
        return Err(ProfilesError::NotFound {
            what: "profile",
            id: p.id,
        });
    }
    Ok(p)
}

pub(crate) fn delete_profile(conn: &Connection, id: &str) -> Result<(), ProfilesError> {
    conn.execute("DELETE FROM profile WHERE id = ?1", params![id])?;
    Ok(())
}

pub(crate) fn touch_profile_last_used(
    conn: &Connection,
    id: &str,
    at: i64,
) -> Result<(), ProfilesError> {
    let changed = conn.execute(
        "UPDATE profile SET last_used_at = ?2 WHERE id = ?1",
        params![id, at],
    )?;
    if changed == 0 {
        return Err(ProfilesError::NotFound {
            what: "profile",
            id: id.to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------
// tunnel
// ---------------------------------------------------------------------

fn tunnel_from_row(row: &Row<'_>) -> rusqlite::Result<Tunnel> {
    Ok(Tunnel {
        id: row.get("id")?,
        name: row.get("name")?,
        host: row.get("host")?,
        port: row.get::<_, i64>("port")? as u16,
        username: row.get("username")?,
        secret_ref: row.get("secret_ref")?,
        known_hosts_pin: row.get("known_hosts_pin")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn create_tunnel(conn: &Connection, t: Tunnel) -> Result<Tunnel, ProfilesError> {
    conn.execute(
        "INSERT INTO tunnel (id, name, host, port, username, secret_ref, known_hosts_pin, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            t.id, t.name, t.host, t.port, t.username, t.secret_ref, t.known_hosts_pin,
            t.created_at, t.updated_at,
        ],
    )?;
    Ok(t)
}

pub(crate) fn get_tunnel(conn: &Connection, id: &str) -> Result<Option<Tunnel>, ProfilesError> {
    conn.query_row(
        "SELECT * FROM tunnel WHERE id = ?1",
        params![id],
        tunnel_from_row,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn list_tunnels(conn: &Connection) -> Result<Vec<Tunnel>, ProfilesError> {
    let mut stmt = conn.prepare("SELECT * FROM tunnel ORDER BY name")?;
    let rows = stmt.query_map([], tunnel_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub(crate) fn update_tunnel(conn: &Connection, t: Tunnel) -> Result<Tunnel, ProfilesError> {
    let changed = conn.execute(
        "UPDATE tunnel SET name = ?2, host = ?3, port = ?4, username = ?5, secret_ref = ?6,
            known_hosts_pin = ?7, updated_at = ?8
         WHERE id = ?1",
        params![
            t.id,
            t.name,
            t.host,
            t.port,
            t.username,
            t.secret_ref,
            t.known_hosts_pin,
            t.updated_at,
        ],
    )?;
    if changed == 0 {
        return Err(ProfilesError::NotFound {
            what: "tunnel",
            id: t.id,
        });
    }
    Ok(t)
}

pub(crate) fn delete_tunnel(conn: &Connection, id: &str) -> Result<(), ProfilesError> {
    conn.execute("DELETE FROM tunnel WHERE id = ?1", params![id])?;
    Ok(())
}

// ---------------------------------------------------------------------
// query_history
// ---------------------------------------------------------------------

fn history_from_row(row: &Row<'_>) -> rusqlite::Result<HistoryEntry> {
    let status: String = row.get("status")?;
    let status = HistoryStatus::parse(&status).unwrap_or(HistoryStatus::Error);
    Ok(HistoryEntry {
        id: row.get("id")?,
        profile_id: row.get("profile_id")?,
        text: row.get("text")?,
        text_hash: row.get("text_hash")?,
        started_at: row.get("started_at")?,
        duration_ms: row.get("duration_ms")?,
        row_count: row.get("row_count")?,
        status,
        error: row.get("error")?,
    })
}

/// Records one executed query. Dedupe rule: if the same
/// `(profile_id, text_hash)` was recorded within the last second, the
/// existing row is updated in place instead of a new one being inserted —
/// this absorbs rapid re-runs/retries without flooding history.
const DEDUPE_WINDOW_MS: i64 = 1_000;

pub(crate) fn record_history(
    conn: &Connection,
    entry: NewHistoryEntry,
) -> Result<HistoryEntry, ProfilesError> {
    let text_hash = hash_text(&entry.text);

    let existing: Option<(i64, i64)> = conn
        .query_row(
            "SELECT id, started_at FROM query_history
             WHERE profile_id = ?1 AND text_hash = ?2
             ORDER BY started_at DESC LIMIT 1",
            params![entry.profile_id, text_hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let id = if let Some((existing_id, existing_started_at)) = existing {
        if (entry.started_at - existing_started_at).abs() <= DEDUPE_WINDOW_MS {
            conn.execute(
                "UPDATE query_history SET
                    started_at = ?2, duration_ms = ?3, row_count = ?4, status = ?5, error = ?6
                 WHERE id = ?1",
                params![
                    existing_id,
                    entry.started_at,
                    entry.duration_ms,
                    entry.row_count,
                    entry.status.as_str(),
                    entry.error,
                ],
            )?;
            existing_id
        } else {
            insert_history(conn, &entry, &text_hash)?
        }
    } else {
        insert_history(conn, &entry, &text_hash)?
    };

    conn.query_row(
        "SELECT * FROM query_history WHERE id = ?1",
        params![id],
        history_from_row,
    )
    .map_err(Into::into)
}

fn insert_history(
    conn: &Connection,
    entry: &NewHistoryEntry,
    text_hash: &str,
) -> Result<i64, ProfilesError> {
    conn.execute(
        "INSERT INTO query_history (profile_id, text, text_hash, started_at, duration_ms, row_count, status, error)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            entry.profile_id, entry.text, text_hash, entry.started_at, entry.duration_ms,
            entry.row_count, entry.status.as_str(), entry.error,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn recent_history(
    conn: &Connection,
    profile_id: Option<&str>,
    limit: u32,
) -> Result<Vec<HistoryEntry>, ProfilesError> {
    let mut rows = Vec::new();
    match profile_id {
        Some(pid) => {
            let mut stmt = conn.prepare(
                "SELECT * FROM query_history WHERE profile_id = ?1 ORDER BY started_at DESC LIMIT ?2",
            )?;
            for r in stmt.query_map(params![pid, limit], history_from_row)? {
                rows.push(r?);
            }
        }
        None => {
            let mut stmt =
                conn.prepare("SELECT * FROM query_history ORDER BY started_at DESC LIMIT ?1")?;
            for r in stmt.query_map(params![limit], history_from_row)? {
                rows.push(r?);
            }
        }
    }
    Ok(rows)
}

/// Full-text search over recorded query text. Uses the FTS5 index when this
/// SQLite build has it; otherwise falls back to a `LIKE` scan (design: "no
/// plugin host" is one thing, silently losing search on a build quirk is
/// another — degrade, don't fail).
pub(crate) fn search_history(
    db: &Db,
    profile_id: Option<&str>,
    query: &str,
    limit: u32,
) -> Result<Vec<HistoryEntry>, ProfilesError> {
    if query.trim().is_empty() {
        return recent_history(&db.conn, profile_id, limit);
    }

    if db.fts5_available {
        // Quote as an FTS5 phrase so punctuation/operators in the searched
        // text (`(`, `:`, `-`, ...) can't be parsed as query syntax.
        let phrase = format!("\"{}\"", query.replace('"', "\"\""));
        let mut rows = Vec::new();
        match profile_id {
            Some(pid) => {
                let mut stmt = db.conn.prepare(
                    "SELECT qh.* FROM query_history qh
                     JOIN query_history_fts f ON f.rowid = qh.id
                     WHERE query_history_fts MATCH ?1 AND qh.profile_id = ?2
                     ORDER BY qh.started_at DESC LIMIT ?3",
                )?;
                for r in stmt.query_map(params![phrase, pid, limit], history_from_row)? {
                    rows.push(r?);
                }
            }
            None => {
                let mut stmt = db.conn.prepare(
                    "SELECT qh.* FROM query_history qh
                     JOIN query_history_fts f ON f.rowid = qh.id
                     WHERE query_history_fts MATCH ?1
                     ORDER BY qh.started_at DESC LIMIT ?2",
                )?;
                for r in stmt.query_map(params![phrase, limit], history_from_row)? {
                    rows.push(r?);
                }
            }
        }
        Ok(rows)
    } else {
        let like = format!("%{}%", escape_like(query));
        let mut rows = Vec::new();
        match profile_id {
            Some(pid) => {
                let mut stmt = db.conn.prepare(
                    "SELECT * FROM query_history WHERE text LIKE ?1 ESCAPE '\\' AND profile_id = ?2
                     ORDER BY started_at DESC LIMIT ?3",
                )?;
                for r in stmt.query_map(params![like, pid, limit], history_from_row)? {
                    rows.push(r?);
                }
            }
            None => {
                let mut stmt = db.conn.prepare(
                    "SELECT * FROM query_history WHERE text LIKE ?1 ESCAPE '\\' ORDER BY started_at DESC LIMIT ?2",
                )?;
                for r in stmt.query_map(params![like, limit], history_from_row)? {
                    rows.push(r?);
                }
            }
        }
        Ok(rows)
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

// ---------------------------------------------------------------------
// saved_query
// ---------------------------------------------------------------------

fn saved_query_from_row(row: &Row<'_>) -> rusqlite::Result<SavedQuery> {
    Ok(SavedQuery {
        id: row.get("id")?,
        folder_id: row.get("folder_id")?,
        profile_id: row.get("profile_id")?,
        name: row.get("name")?,
        text: row.get("text")?,
        params_json: row.get("params_json")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn create_saved_query(
    conn: &Connection,
    q: SavedQuery,
) -> Result<SavedQuery, ProfilesError> {
    conn.execute(
        "INSERT INTO saved_query (id, folder_id, profile_id, name, text, params_json, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![q.id, q.folder_id, q.profile_id, q.name, q.text, q.params_json, q.created_at, q.updated_at],
    )?;
    Ok(q)
}

pub(crate) fn get_saved_query(
    conn: &Connection,
    id: &str,
) -> Result<Option<SavedQuery>, ProfilesError> {
    conn.query_row(
        "SELECT * FROM saved_query WHERE id = ?1",
        params![id],
        saved_query_from_row,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn list_saved_queries(conn: &Connection) -> Result<Vec<SavedQuery>, ProfilesError> {
    let mut stmt = conn.prepare("SELECT * FROM saved_query ORDER BY name")?;
    let rows = stmt.query_map([], saved_query_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub(crate) fn update_saved_query(
    conn: &Connection,
    q: SavedQuery,
) -> Result<SavedQuery, ProfilesError> {
    let changed = conn.execute(
        "UPDATE saved_query SET folder_id = ?2, profile_id = ?3, name = ?4, text = ?5,
            params_json = ?6, updated_at = ?7
         WHERE id = ?1",
        params![
            q.id,
            q.folder_id,
            q.profile_id,
            q.name,
            q.text,
            q.params_json,
            q.updated_at
        ],
    )?;
    if changed == 0 {
        return Err(ProfilesError::NotFound {
            what: "saved_query",
            id: q.id,
        });
    }
    Ok(q)
}

pub(crate) fn delete_saved_query(conn: &Connection, id: &str) -> Result<(), ProfilesError> {
    conn.execute("DELETE FROM saved_query WHERE id = ?1", params![id])?;
    Ok(())
}

// ---------------------------------------------------------------------
// editor_tab — crash-safe session restore: the whole set is replaced
// atomically so a reader never observes a half-written session.
// ---------------------------------------------------------------------

fn editor_tab_from_row(row: &Row<'_>) -> rusqlite::Result<EditorTab> {
    Ok(EditorTab {
        id: row.get("id")?,
        profile_id: row.get("profile_id")?,
        title: row.get("title")?,
        text: row.get("text")?,
        cursor_pos: row.get("cursor_pos")?,
        sort_order: row.get("sort_order")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn save_all_tabs(
    conn: &mut Connection,
    tabs: Vec<EditorTab>,
) -> Result<(), ProfilesError> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM editor_tab", [])?;
    for t in &tabs {
        tx.execute(
            "INSERT INTO editor_tab (id, profile_id, title, text, cursor_pos, sort_order, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![t.id, t.profile_id, t.title, t.text, t.cursor_pos, t.sort_order, t.updated_at],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn restore_all_tabs(conn: &Connection) -> Result<Vec<EditorTab>, ProfilesError> {
    let mut stmt = conn.prepare("SELECT * FROM editor_tab ORDER BY sort_order")?;
    let rows = stmt.query_map([], editor_tab_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

// ---------------------------------------------------------------------
// kv
// ---------------------------------------------------------------------

pub(crate) fn kv_get(conn: &Connection, key: &str) -> Result<Option<String>, ProfilesError> {
    conn.query_row(
        "SELECT value FROM kv WHERE \"key\" = ?1",
        params![key],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn kv_set(conn: &Connection, key: &str, value: &str) -> Result<(), ProfilesError> {
    conn.execute(
        "INSERT INTO kv (\"key\", \"value\") VALUES (?1, ?2)
         ON CONFLICT(\"key\") DO UPDATE SET \"value\" = excluded.\"value\"",
        params![key, value],
    )?;
    Ok(())
}

pub(crate) fn kv_delete(conn: &Connection, key: &str) -> Result<(), ProfilesError> {
    conn.execute("DELETE FROM kv WHERE \"key\" = ?1", params![key])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! White-box tests that need to reach into `Db` directly — in
    //! particular, forcing `fts5_available = false` to deterministically
    //! exercise the `LIKE` fallback regardless of whether *this* build's
    //! bundled SQLite has FTS5 compiled in (it does — see Cargo.toml — but
    //! the fallback still needs its own proof independent of that fact).
    use super::*;
    use crate::db::{open_and_prepare, RetentionPolicy, Target};

    fn open_memory() -> Db {
        open_and_prepare(&Target::Memory, RetentionPolicy::default()).expect("open in-memory db")
    }

    /// `query_history.profile_id` has a `REFERENCES profile(id)` — history
    /// belongs to a profile, and deleting a profile cascades its history — so
    /// tests that record history need a real profile row to satisfy the
    /// foreign key.
    fn ensure_profile(conn: &Connection, id: &str) {
        create_profile(
            conn,
            Profile {
                id: id.to_string(),
                folder_id: None,
                name: id.to_string(),
                driver_id: "postgres".to_string(),
                config: datagrep_api::ConnectionConfig {
                    driver: std::sync::Arc::from("postgres"),
                    values: Default::default(),
                },
                secret_ref: None,
                tunnel_id: None,
                env: Env::Dev,
                color: None,
                read_only: false,
                confirm_writes: false,
                auto_limit: None,
                idle_timeout_s: None,
                last_used_at: None,
                created_at: 0,
                updated_at: 0,
            },
        )
        .expect("insert fixture profile");
    }

    fn entry(profile_id: &str, text: &str, started_at: i64) -> NewHistoryEntry {
        NewHistoryEntry {
            profile_id: profile_id.to_string(),
            text: text.to_string(),
            started_at,
            duration_ms: Some(5),
            row_count: Some(1),
            status: HistoryStatus::Ok,
            error: None,
        }
    }

    #[test]
    fn bundled_sqlite_has_fts5_compiled_in() {
        // Guards the assumption documented in Cargo.toml: if a future
        // rusqlite/libsqlite3-sys bump ever drops FTS5 from the bundled
        // build, this fails loudly instead of the fallback silently
        // papering over lost search quality.
        let db = open_memory();
        assert!(
            db.fts5_available,
            "expected the bundled build to compile in FTS5"
        );
    }

    #[test]
    fn search_history_like_fallback_finds_by_word() {
        let mut db = open_memory();
        db.fts5_available = false; // force the LIKE path regardless of build
        ensure_profile(&db.conn, "p1");

        record_history(
            &db.conn,
            entry("p1", "SELECT * FROM users WHERE active", 1_000),
        )
        .unwrap();
        record_history(&db.conn, entry("p1", "SELECT * FROM orders", 2_000)).unwrap();

        let hits = search_history(&db, Some("p1"), "users", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("users"));
    }

    #[test]
    fn search_history_fts5_finds_by_word() {
        let db = open_memory();
        assert!(db.fts5_available);
        ensure_profile(&db.conn, "p1");

        record_history(
            &db.conn,
            entry("p1", "SELECT * FROM users WHERE active", 1_000),
        )
        .unwrap();
        record_history(&db.conn, entry("p1", "SELECT * FROM orders", 2_000)).unwrap();

        let hits = search_history(&db, Some("p1"), "orders", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("orders"));
    }

    #[test]
    fn record_history_dedupes_within_one_second() {
        let db = open_memory();
        ensure_profile(&db.conn, "p1");
        let first = record_history(&db.conn, entry("p1", "SELECT 1", 1_000)).unwrap();
        let second = record_history(&db.conn, entry("p1", "SELECT 1", 1_500)).unwrap();
        assert_eq!(
            first.id, second.id,
            "within the 1s window this should update, not insert"
        );
        assert_eq!(second.started_at, 1_500);

        let third = record_history(&db.conn, entry("p1", "SELECT 1", 10_000)).unwrap();
        assert_ne!(
            third.id, second.id,
            "outside the window this should insert a new row"
        );
    }
}
