//! Black-box tests against `datagrep-profiles`'s public API only. White-box
//! tests that need to force `fts5_available` or otherwise reach into `Db`
//! live in `src/queries.rs`.

use std::collections::BTreeMap;
use std::sync::Arc;

use datagrep_api::{ConfigValue, ConnectionConfig};
use datagrep_profiles::{
    new_id, now_ms, Folder, HistoryStatus, ImportStrategy, NewHistoryEntry, ProfilesError,
    RetentionPolicy, SavedQuery, Store, Tunnel,
};

// ---------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------

fn sample_config() -> ConnectionConfig {
    let mut values = BTreeMap::new();
    values.insert("host".to_string(), ConfigValue::Str("localhost".into()));
    values.insert("port".to_string(), ConfigValue::Num(5432.0));
    values.insert("tls".to_string(), ConfigValue::Bool(true));
    ConnectionConfig {
        driver: Arc::from("postgres"),
        values,
    }
}

fn sample_folder(id: &str) -> Folder {
    let now = now_ms();
    Folder {
        id: id.to_string(),
        parent_id: None,
        name: format!("folder-{id}"),
        sort_order: 0,
        created_at: now,
        updated_at: now,
    }
}

fn sample_profile(id: &str, folder_id: Option<String>) -> datagrep_profiles::Profile {
    let now = now_ms();
    datagrep_profiles::Profile {
        id: id.to_string(),
        folder_id,
        name: format!("profile-{id}"),
        driver_id: "postgres".to_string(),
        config: sample_config(),
        secret_ref: Some(format!("keychain:datagrep:profile:{id}")),
        tunnel_id: None,
        color: Some("#00ff00".to_string()),
        read_only: false,
        confirm_writes: false,
        auto_limit: Some(1000),
        idle_timeout_s: Some(300),
        last_used_at: None,
        created_at: now,
        updated_at: now,
    }
}

fn sample_tunnel(id: &str) -> Tunnel {
    let now = now_ms();
    Tunnel {
        id: id.to_string(),
        name: format!("tunnel-{id}"),
        host: "jump.example.com".to_string(),
        port: 22,
        username: "deploy".to_string(),
        secret_ref: Some(format!("keychain:datagrep:tunnel:{id}")),
        known_hosts_pin: Some("SHA256:abc123".to_string()),
        created_at: now,
        updated_at: now,
    }
}

// ---------------------------------------------------------------------
// lazy open
// ---------------------------------------------------------------------

#[tokio::test]
async fn store_open_does_not_touch_disk_until_first_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("datagrep.db");

    let store = Store::open(&path);
    assert!(
        !path.exists(),
        "constructing Store must not open the database"
    );

    store
        .create_folder(sample_folder("f1"))
        .await
        .expect("first real call should open, migrate, and write");
    assert!(
        path.exists(),
        "the first real call should have created the database file"
    );
}

// ---------------------------------------------------------------------
// migrations
// ---------------------------------------------------------------------

#[tokio::test]
async fn migration_brings_empty_db_to_current_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("datagrep.db");

    let store = Store::open(&path);
    store
        .list_folders()
        .await
        .expect("open+migrate on first call");

    let conn = rusqlite::Connection::open(&path).expect("reopen for inspection");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        version, 2,
        "fresh db should land on the current schema version"
    );

    // The rest of the schema landed too, not just the version pragma.
    for table in [
        "folder",
        "profile",
        "tunnel",
        "query_history",
        "saved_query",
        "editor_tab",
        "kv",
    ] {
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "table `{table}` should exist after migration");
    }
}

#[tokio::test]
async fn bak_snapshot_is_created_on_migration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("datagrep.db");
    let bak = dir.path().join("datagrep.db.bak");

    let store = Store::open(&path);
    store.list_folders().await.expect("open+migrate");

    assert!(
        bak.exists(),
        "a .bak snapshot should be copied before migrating"
    );
}

#[tokio::test]
async fn reopen_is_idempotent_and_does_not_reapply_migrations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("datagrep.db");
    let bak = dir.path().join("datagrep.db.bak");

    {
        let store = Store::open(&path);
        store
            .create_folder(sample_folder("f1"))
            .await
            .expect("create");
    } // worker thread joins here (Store::drop)

    let bak_len_after_first_open = std::fs::metadata(&bak).unwrap().len();

    {
        let store = Store::open(&path);
        let folders = store.list_folders().await.expect("reopen should not error");
        assert_eq!(folders.len(), 1, "data must survive a close/reopen cycle");
    }

    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2, "version must not change on a no-op reopen");

    // Nothing left to migrate on the second open, so `backup_before_migrate`
    // must not have run again — the .bak from the first open is untouched.
    let bak_len_after_second_open = std::fs::metadata(&bak).unwrap().len();
    assert_eq!(bak_len_after_first_open, bak_len_after_second_open);
}

// ---------------------------------------------------------------------
// profile CRUD + secret hygiene
// ---------------------------------------------------------------------

#[tokio::test]
async fn profile_crud_round_trips_connection_config_fidelity() {
    let store = Store::open_in_memory();
    let folder = store.create_folder(sample_folder("f1")).await.unwrap();
    let profile = sample_profile("p1", Some(folder.id.clone()));

    let created = store
        .create_profile(profile.clone())
        .await
        .expect("create_profile");
    assert_eq!(created, profile);

    let fetched = store
        .get_profile("p1")
        .await
        .unwrap()
        .expect("profile should exist");
    assert_eq!(
        fetched, profile,
        "round-tripped profile must match exactly, including config"
    );
    assert_eq!(fetched.config.driver.as_ref(), "postgres");
    assert_eq!(
        fetched.config.values.get("port"),
        Some(&ConfigValue::Num(5432.0))
    );

    let mut updated = fetched.clone();
    updated.name = "renamed".to_string();
    let updated = store.update_profile(updated).await.expect("update_profile");
    assert_eq!(updated.name, "renamed");
    assert!(updated.updated_at >= created.updated_at);

    let listed = store.list_profiles(Some(folder.id.clone())).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "p1");

    store.touch_profile_last_used("p1").await.unwrap();
    let touched = store.get_profile("p1").await.unwrap().unwrap();
    assert!(touched.last_used_at.is_some());

    store.delete_profile("p1").await.unwrap();
    assert!(store.get_profile("p1").await.unwrap().is_none());
}

#[tokio::test]
async fn secret_shaped_config_key_is_rejected() {
    let store = Store::open_in_memory();
    let mut profile = sample_profile("p1", None);
    profile
        .config
        .values
        .insert("password".to_string(), ConfigValue::Str("hunter2".into()));

    let err = store.create_profile(profile).await.unwrap_err();
    let msg = err.to_string();
    match &err {
        ProfilesError::SecretShapedKey { key, .. } => assert_eq!(key, "password"),
        other => panic!("expected SecretShapedKey, got {other:?}"),
    }
    assert!(
        msg.contains("secret_ref"),
        "error should point the caller at secret_ref: {msg}"
    );

    // Same rule applies on update.
    let store = Store::open_in_memory();
    let clean = sample_profile("p2", None);
    store.create_profile(clean.clone()).await.unwrap();
    let mut dirty = clean;
    dirty
        .config
        .values
        .insert("api_token".to_string(), ConfigValue::Str("x".into()));
    let err = store.update_profile(dirty).await.unwrap_err();
    assert!(matches!(err, ProfilesError::SecretShapedKey { .. }));
}

// ---------------------------------------------------------------------
// tunnels / saved queries — lighter CRUD coverage
// ---------------------------------------------------------------------

#[tokio::test]
async fn tunnel_crud_round_trip() {
    let store = Store::open_in_memory();
    let tunnel = store.create_tunnel(sample_tunnel("t1")).await.unwrap();
    assert_eq!(store.get_tunnel("t1").await.unwrap(), Some(tunnel.clone()));

    let mut updated = tunnel;
    updated.port = 2222;
    let updated = store.update_tunnel(updated).await.unwrap();
    assert_eq!(updated.port, 2222);
    assert_eq!(store.list_tunnels().await.unwrap().len(), 1);

    store.delete_tunnel("t1").await.unwrap();
    assert!(store.get_tunnel("t1").await.unwrap().is_none());
}

#[tokio::test]
async fn saved_query_crud_round_trip() {
    let store = Store::open_in_memory();
    let now = now_ms();
    let q = SavedQuery {
        id: new_id(),
        folder_id: None,
        profile_id: None,
        name: "top customers".to_string(),
        text: "SELECT * FROM customers ORDER BY revenue DESC LIMIT 10".to_string(),
        params_json: None,
        created_at: now,
        updated_at: now,
    };
    let created = store.create_saved_query(q.clone()).await.unwrap();
    assert_eq!(
        store.get_saved_query(created.id.clone()).await.unwrap(),
        Some(created.clone())
    );
    assert_eq!(store.list_saved_queries().await.unwrap().len(), 1);

    store.delete_saved_query(created.id.clone()).await.unwrap();
    assert!(store.list_saved_queries().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------
// query_history
// ---------------------------------------------------------------------

#[tokio::test]
async fn history_dedupe_window_updates_instead_of_inserting() {
    let store = Store::open_in_memory();
    store
        .create_profile(sample_profile("p1", None))
        .await
        .unwrap();
    let base = now_ms();

    let first = store
        .record_history(NewHistoryEntry {
            profile_id: "p1".into(),
            text: "SELECT 1".into(),
            started_at: base,
            duration_ms: Some(1),
            row_count: Some(1),
            status: HistoryStatus::Ok,
            error: None,
        })
        .await
        .unwrap();

    let second = store
        .record_history(NewHistoryEntry {
            profile_id: "p1".into(),
            text: "SELECT 1".into(),
            started_at: base + 400, // inside the 1s dedupe window
            duration_ms: Some(2),
            row_count: Some(1),
            status: HistoryStatus::Ok,
            error: None,
        })
        .await
        .unwrap();
    assert_eq!(first.id, second.id);

    let recent = store.recent_history(Some("p1".into()), 10).await.unwrap();
    assert_eq!(
        recent.len(),
        1,
        "the dedupe window should have updated, not inserted"
    );
}

#[tokio::test]
async fn history_search_finds_recorded_query_by_word() {
    let store = Store::open_in_memory();
    store
        .create_profile(sample_profile("p1", None))
        .await
        .unwrap();
    let base = now_ms();
    store
        .record_history(NewHistoryEntry {
            profile_id: "p1".into(),
            text: "SELECT * FROM invoices WHERE overdue".into(),
            started_at: base,
            duration_ms: Some(3),
            row_count: Some(4),
            status: HistoryStatus::Ok,
            error: None,
        })
        .await
        .unwrap();
    store
        .record_history(NewHistoryEntry {
            profile_id: "p1".into(),
            text: "SELECT * FROM customers".into(),
            started_at: base + 5_000,
            duration_ms: Some(1),
            row_count: Some(9),
            status: HistoryStatus::Ok,
            error: None,
        })
        .await
        .unwrap();

    let hits = store
        .search_history(Some("p1".into()), "invoices".into(), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].text.contains("invoices"));
}

#[tokio::test]
async fn retention_trims_rows_older_than_max_age_on_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("datagrep.db");
    let now = now_ms();
    let very_old = now - 200 * 24 * 3_600 * 1_000; // older than the 180d default

    {
        let store = Store::open(&path);
        store
            .create_profile(sample_profile("p1", None))
            .await
            .unwrap();
        store
            .record_history(NewHistoryEntry {
                profile_id: "p1".into(),
                text: "old query".into(),
                started_at: very_old,
                duration_ms: None,
                row_count: None,
                status: HistoryStatus::Ok,
                error: None,
            })
            .await
            .unwrap();
        store
            .record_history(NewHistoryEntry {
                profile_id: "p1".into(),
                text: "recent query".into(),
                started_at: now,
                duration_ms: None,
                row_count: None,
                status: HistoryStatus::Ok,
                error: None,
            })
            .await
            .unwrap();
    }

    // Retention only runs on open — there is no timer, so reopening is what
    // trims.
    let store = Store::open(&path);
    let remaining = store.recent_history(Some("p1".into()), 100).await.unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "the 200-day-old row should have been trimmed on reopen"
    );
    assert_eq!(remaining[0].text, "recent query");
}

#[tokio::test]
async fn retention_trims_to_max_row_count_on_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("datagrep.db");
    let now = now_ms();

    {
        let store = Store::open(&path);
        store
            .create_profile(sample_profile("p1", None))
            .await
            .unwrap();
        for i in 0..5i64 {
            store
                .record_history(NewHistoryEntry {
                    profile_id: "p1".into(),
                    text: format!("query {i}"),
                    started_at: now + i * 10_000,
                    duration_ms: None,
                    row_count: None,
                    status: HistoryStatus::Ok,
                    error: None,
                })
                .await
                .unwrap();
        }
    }

    let store = Store::open_with_retention(
        &path,
        RetentionPolicy {
            max_rows: 2,
            max_age_days: 180,
        },
    );
    let remaining = store.recent_history(Some("p1".into()), 100).await.unwrap();
    assert_eq!(
        remaining.len(),
        2,
        "only the newest max_rows entries should survive reopen"
    );
    assert_eq!(remaining[0].text, "query 4");
    assert_eq!(remaining[1].text, "query 3");
}

// ---------------------------------------------------------------------
// kv
// ---------------------------------------------------------------------

#[tokio::test]
async fn kv_get_set_delete() {
    let store = Store::open_in_memory();
    assert_eq!(store.kv_get("theme").await.unwrap(), None);
    store.kv_set("theme", "dark").await.unwrap();
    assert_eq!(
        store.kv_get("theme").await.unwrap(),
        Some("dark".to_string())
    );
    store.kv_set("theme", "light").await.unwrap();
    assert_eq!(
        store.kv_get("theme").await.unwrap(),
        Some("light".to_string())
    );
    store.kv_delete("theme").await.unwrap();
    assert_eq!(store.kv_get("theme").await.unwrap(), None);
}

// ---------------------------------------------------------------------
// TOML export / import
// ---------------------------------------------------------------------

#[tokio::test]
async fn toml_export_then_import_round_trips_profiles() {
    let source = Store::open_in_memory();
    let folder = source.create_folder(sample_folder("f1")).await.unwrap();
    let tunnel = source.create_tunnel(sample_tunnel("t1")).await.unwrap();
    let mut profile = sample_profile("p1", Some(folder.id.clone()));
    profile.tunnel_id = Some(tunnel.id.clone());
    let profile = source.create_profile(profile).await.unwrap();

    let exported = source.export_profiles().await.unwrap();
    assert!(!exported.contains("hunter2"));
    assert!(
        exported.contains("secret_ref"),
        "secret_ref should survive export"
    );
    assert!(exported.contains("keychain:datagrep:profile:p1"));

    let dest = Store::open_in_memory();
    let summary = dest
        .import_profiles(exported.clone(), ImportStrategy::Replace)
        .await
        .expect("import should succeed");
    assert_eq!(summary.folders_upserted, 1);
    assert_eq!(summary.profiles_upserted, 1);
    assert_eq!(summary.tunnels_upserted, 1);

    let imported_profile = dest
        .get_profile("p1")
        .await
        .unwrap()
        .expect("profile imported");
    assert_eq!(imported_profile.id, profile.id);
    assert_eq!(imported_profile.name, profile.name);
    assert_eq!(imported_profile.config, profile.config);
    assert_eq!(imported_profile.secret_ref, profile.secret_ref);
    assert_eq!(imported_profile.tunnel_id, profile.tunnel_id);
    assert_eq!(imported_profile.folder_id, profile.folder_id);

    assert_eq!(
        dest.list_folders().await.unwrap(),
        source.list_folders().await.unwrap()
    );
    assert_eq!(
        dest.list_tunnels().await.unwrap(),
        source.list_tunnels().await.unwrap()
    );

    // Merge should not disturb an existing row that isn't in the import.
    dest.create_folder(sample_folder("f2")).await.unwrap();
    dest.import_profiles(exported, ImportStrategy::Merge)
        .await
        .unwrap();
    assert_eq!(dest.list_folders().await.unwrap().len(), 2);
}

#[tokio::test]
async fn toml_import_rejects_secret_shaped_config() {
    let dest = Store::open_in_memory();
    let toml = r#"
version = 1

[[profile]]
id = "p1"
name = "leaky"
driver_id = "postgres"
env = "dev"
read_only = false
confirm_writes = false
created_at = 0
updated_at = 0

[profile.config]
driver = "postgres"

[profile.config.values.password]
Str = "hunter2"
"#;
    let err = dest
        .import_profiles(toml.to_string(), ImportStrategy::Merge)
        .await
        .unwrap_err();
    assert!(matches!(err, ProfilesError::SecretShapedKey { .. }));
}

// ---------------------------------------------------------------------
// concurrency + worker lifecycle
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_history_writes_do_not_leak_sqlite_busy() {
    let store = Arc::new(Store::open_in_memory());
    store
        .create_profile(sample_profile("p1", None))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..64i64 {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            store
                .record_history(NewHistoryEntry {
                    profile_id: "p1".into(),
                    text: format!("query {i}"),
                    started_at: now_ms() + i, // distinct hashes, no dedupe collisions
                    duration_ms: Some(1),
                    row_count: Some(0),
                    status: HistoryStatus::Ok,
                    error: None,
                })
                .await
        }));
    }

    for task in tasks {
        task.await
            .expect("task panicked")
            .expect("record_history must not surface SQLITE_BUSY");
    }

    let recorded = store.recent_history(Some("p1".into()), 200).await.unwrap();
    assert_eq!(recorded.len(), 64);
}

#[tokio::test]
async fn worker_thread_joins_cleanly_on_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("datagrep.db"));
    store.create_folder(sample_folder("f1")).await.unwrap();

    // `WorkerHandle::drop` closes the command channel and then *joins* the
    // worker thread synchronously. If shutdown were broken (e.g. the
    // channel weren't closed before joining) this would deadlock forever
    // instead of returning — run it on a blocking task under a timeout so a
    // regression fails the test instead of hanging the suite.
    let dropped = tokio::task::spawn_blocking(move || drop(store));
    tokio::time::timeout(std::time::Duration::from_secs(5), dropped)
        .await
        .expect("dropping Store hung — worker thread did not join cleanly")
        .expect("drop task panicked");
}

/// A v1 database carrying a real profile must come through `migrate_v2` with
/// every other column intact — not just without `env`.
///
/// The column had a CHECK constraint, so dropping it means rebuilding the
/// table, and a rebuild that forgets a column is silent: `secret_ref` in
/// particular would orphan a keychain entry and break a working connection
/// while every other test still passed.
///
/// The fixture is built by making a current database and then regressing it —
/// re-adding `env` and winding `user_version` back to 1 — rather than
/// hand-writing the old schema, so it keeps every other table the open-time
/// retention pass expects.
#[tokio::test]
async fn migrating_off_env_keeps_every_other_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("profiles.db");

    let mut profile = sample_profile("p1", None);
    profile.secret_ref = Some("keychain:datagrep:p1:password".to_string());
    profile.color = Some("red".to_string());
    profile.read_only = true;
    profile.confirm_writes = true;
    profile.auto_limit = Some(500);
    profile.idle_timeout_s = Some(30);
    {
        let store = Store::open(&path);
        store.create_profile(profile.clone()).await.unwrap();
    }

    // Regress to the v1 shape.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE profile ADD COLUMN env TEXT NOT NULL DEFAULT 'dev';
             UPDATE profile SET env = 'prod';
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }

    let store = Store::open(&path);
    let profiles = store.list_profiles(None).await.unwrap();
    assert_eq!(profiles.len(), 1, "the profile must survive the rebuild");
    let p = &profiles[0];
    assert_eq!(p.id, profile.id);
    assert_eq!(p.name, profile.name);
    assert_eq!(
        p.secret_ref.as_deref(),
        Some("keychain:datagrep:p1:password"),
        "a lost secret_ref orphans the keychain entry"
    );
    assert_eq!(p.color.as_deref(), Some("red"));
    assert!(p.read_only);
    assert!(p.confirm_writes);
    assert_eq!(p.auto_limit, Some(500));
    assert_eq!(p.idle_timeout_s, Some(30));
    assert_eq!(p.config, profile.config);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let has_env: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('profile') WHERE name = 'env'")
        .unwrap()
        .exists([])
        .unwrap();
    assert!(!has_env, "env should be gone");
}
