//! The async facade over one dedicated worker thread: every `rusqlite` call
//! in this crate is synchronous and runs on that thread, never on an async
//! runtime's own worker. `Store` methods hand a closure to the thread and
//! `.await` a oneshot reply.
//!
//! Construction is lazy so nothing touches disk on the startup path —
//! `Store::open` just remembers where the database *would* live; the worker
//! thread, the SQLite connection, and the migration/retention pass only
//! happen on the first real call.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::{oneshot, OnceCell};

use crate::db::{self, Db, RetentionPolicy, Target};
use crate::error::ProfilesError;
use crate::export::{ExportBundle, ImportStrategy, ImportSummary};
use crate::model::{
    now_ms, EditorTab, Folder, HistoryEntry, NewHistoryEntry, Profile, SavedQuery, Tunnel,
};
use crate::queries;
use crate::secrets::validate_no_secrets;

/// One unit of work for the worker thread: runs with exclusive `&mut`
/// access to the single live `Db`. Reports its result back to the async
/// caller through a oneshot sender it captures itself, so the run loop
/// doesn't need to know each job's return type.
type Job = Box<dyn FnOnce(&mut Db) + Send>;

/// The live worker thread plus the channel used to reach it. Held behind an
/// `Arc` inside `Store`'s `OnceCell` so every clone of the handle shares one
/// thread; dropping the last reference shuts the thread down.
struct WorkerHandle {
    cmd_tx: Mutex<std::sync::mpsc::Sender<Job>>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl WorkerHandle {
    fn submit(&self, job: Job) -> Result<(), ProfilesError> {
        self.cmd_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .send(job)
            .map_err(|_| ProfilesError::WorkerGone)
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // Close the command channel first so the worker thread's blocking
        // `recv()` returns `Err` and its loop exits — only *then* is it safe
        // to join, otherwise we'd deadlock waiting on a thread parked
        // waiting for work that will never arrive.
        if let Ok(mut guard) = self.cmd_tx.lock() {
            let (dummy_tx, _dummy_rx) = std::sync::mpsc::channel::<Job>();
            drop(std::mem::replace(&mut *guard, dummy_tx));
        }
        if let Ok(mut guard) = self.join.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Spawns the worker thread, opens (or creates) the database, migrates it,
/// and runs the on-open retention trim — the entire "first real call" path.
async fn spawn_worker(
    target: Target,
    retention: RetentionPolicy,
) -> Result<Arc<WorkerHandle>, ProfilesError> {
    let (init_tx, init_rx) = oneshot::channel::<Result<(), ProfilesError>>();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Job>();

    let join = std::thread::Builder::new()
        .name("datagrep-profiles-worker".to_string())
        .spawn(move || {
            let mut conn = match db::open_and_prepare(&target, retention) {
                Ok(db) => db,
                Err(err) => {
                    let _ = init_tx.send(Err(err));
                    return;
                }
            };
            if init_tx.send(Ok(())).is_err() {
                // The caller stopped waiting (its future was dropped) —
                // nothing left to serve.
                return;
            }
            while let Ok(job) = cmd_rx.recv() {
                job(&mut conn);
            }
        })
        .map_err(|err| ProfilesError::WorkerStart(err.to_string()))?;

    match init_rx.await {
        Ok(Ok(())) => Ok(Arc::new(WorkerHandle {
            cmd_tx: Mutex::new(cmd_tx),
            join: Mutex::new(Some(join)),
        })),
        Ok(Err(err)) => {
            let _ = join.join();
            Err(err)
        }
        Err(_) => {
            let _ = join.join();
            Err(ProfilesError::WorkerStart(
                "worker thread exited before signaling ready".to_string(),
            ))
        }
    }
}

/// Local persistence for `datagrep`: profiles, folders, tunnels, query
/// history (+FTS5), saved queries, editor tabs, and small key/value state,
/// in one SQLite file.
pub struct Store {
    target: Target,
    retention: RetentionPolicy,
    worker: OnceCell<Arc<WorkerHandle>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("target", &self.target)
            .field("opened", &self.worker.initialized())
            .finish()
    }
}

impl Store {
    /// A store backed by a SQLite file at `path`. Nothing touches disk yet —
    /// see module docs.
    pub fn open(path: impl AsRef<Path>) -> Store {
        Store::open_with_retention(path, RetentionPolicy::default())
    }

    /// Like [`Store::open`], with a non-default retention policy. Mainly for
    /// callers (and tests) that don't want to wait out the real 20k
    /// rows/180 day default.
    pub fn open_with_retention(path: impl AsRef<Path>, retention: RetentionPolicy) -> Store {
        Store {
            target: Target::File(path.as_ref().to_path_buf()),
            retention,
            worker: OnceCell::new(),
        }
    }

    /// An in-memory store: same schema and migrations, gone when it drops.
    /// Useful for tests and for short-lived embeddings.
    pub fn open_in_memory() -> Store {
        Store::open_in_memory_with_retention(RetentionPolicy::default())
    }

    /// Like [`Store::open_in_memory`], with a non-default retention policy.
    pub fn open_in_memory_with_retention(retention: RetentionPolicy) -> Store {
        Store {
            target: Target::Memory,
            retention,
            worker: OnceCell::new(),
        }
    }

    async fn ensure_worker(&self) -> Result<Arc<WorkerHandle>, ProfilesError> {
        let handle = self
            .worker
            .get_or_try_init(|| spawn_worker(self.target.clone(), self.retention))
            .await?;
        Ok(Arc::clone(handle))
    }

    /// Runs `f` with exclusive access to the database on the worker thread
    /// and returns its result. This is the only place SQL crosses from
    /// async code onto the worker thread.
    async fn run<F, T>(&self, f: F) -> Result<T, ProfilesError>
    where
        F: FnOnce(&mut Db) -> Result<T, ProfilesError> + Send + 'static,
        T: Send + 'static,
    {
        let handle = self.ensure_worker().await?;
        let (reply_tx, reply_rx) = oneshot::channel::<Result<T, ProfilesError>>();
        let job: Job = Box::new(move |db| {
            let _ = reply_tx.send(f(db));
        });
        handle.submit(job)?;
        reply_rx.await.map_err(|_| ProfilesError::WorkerGone)?
    }

    // -- folder ----------------------------------------------------------

    pub async fn create_folder(&self, folder: Folder) -> Result<Folder, ProfilesError> {
        self.run(move |db| queries::create_folder(&db.conn, folder))
            .await
    }

    pub async fn get_folder(&self, id: impl Into<String>) -> Result<Option<Folder>, ProfilesError> {
        let id = id.into();
        self.run(move |db| queries::get_folder(&db.conn, &id)).await
    }

    pub async fn list_folders(&self) -> Result<Vec<Folder>, ProfilesError> {
        self.run(|db| queries::list_folders(&db.conn)).await
    }

    pub async fn update_folder(&self, mut folder: Folder) -> Result<Folder, ProfilesError> {
        folder.updated_at = now_ms();
        self.run(move |db| queries::update_folder(&db.conn, folder))
            .await
    }

    pub async fn delete_folder(&self, id: impl Into<String>) -> Result<(), ProfilesError> {
        let id = id.into();
        self.run(move |db| queries::delete_folder(&db.conn, &id))
            .await
    }

    // -- profile -----------------------------------------------------------
    // Every write is validated to reject secret-shaped config keys *before*
    // the worker thread (and therefore SQLite) ever sees it.

    pub async fn create_profile(&self, profile: Profile) -> Result<Profile, ProfilesError> {
        validate_no_secrets(&profile.config)?;
        self.run(move |db| queries::create_profile(&db.conn, profile))
            .await
    }

    pub async fn get_profile(
        &self,
        id: impl Into<String>,
    ) -> Result<Option<Profile>, ProfilesError> {
        let id = id.into();
        self.run(move |db| queries::get_profile(&db.conn, &id))
            .await
    }

    /// Lists profiles, optionally scoped to one folder (`None` = all).
    pub async fn list_profiles(
        &self,
        folder_id: Option<String>,
    ) -> Result<Vec<Profile>, ProfilesError> {
        self.run(move |db| queries::list_profiles(&db.conn, folder_id.as_deref()))
            .await
    }

    pub async fn update_profile(&self, mut profile: Profile) -> Result<Profile, ProfilesError> {
        validate_no_secrets(&profile.config)?;
        profile.updated_at = now_ms();
        self.run(move |db| queries::update_profile(&db.conn, profile))
            .await
    }

    pub async fn delete_profile(&self, id: impl Into<String>) -> Result<(), ProfilesError> {
        let id = id.into();
        self.run(move |db| queries::delete_profile(&db.conn, &id))
            .await
    }

    pub async fn touch_profile_last_used(
        &self,
        id: impl Into<String>,
    ) -> Result<(), ProfilesError> {
        let id = id.into();
        let at = now_ms();
        self.run(move |db| queries::touch_profile_last_used(&db.conn, &id, at))
            .await
    }

    // -- tunnel ------------------------------------------------------------

    pub async fn create_tunnel(&self, tunnel: Tunnel) -> Result<Tunnel, ProfilesError> {
        self.run(move |db| queries::create_tunnel(&db.conn, tunnel))
            .await
    }

    pub async fn get_tunnel(&self, id: impl Into<String>) -> Result<Option<Tunnel>, ProfilesError> {
        let id = id.into();
        self.run(move |db| queries::get_tunnel(&db.conn, &id)).await
    }

    pub async fn list_tunnels(&self) -> Result<Vec<Tunnel>, ProfilesError> {
        self.run(|db| queries::list_tunnels(&db.conn)).await
    }

    pub async fn update_tunnel(&self, mut tunnel: Tunnel) -> Result<Tunnel, ProfilesError> {
        tunnel.updated_at = now_ms();
        self.run(move |db| queries::update_tunnel(&db.conn, tunnel))
            .await
    }

    pub async fn delete_tunnel(&self, id: impl Into<String>) -> Result<(), ProfilesError> {
        let id = id.into();
        self.run(move |db| queries::delete_tunnel(&db.conn, &id))
            .await
    }

    // -- query_history -------------------------------------------------

    /// Records one executed query, deduping against a same-hash entry for
    /// the same profile within the last second.
    pub async fn record_history(
        &self,
        entry: NewHistoryEntry,
    ) -> Result<HistoryEntry, ProfilesError> {
        self.run(move |db| queries::record_history(&db.conn, entry))
            .await
    }

    pub async fn recent_history(
        &self,
        profile_id: Option<String>,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, ProfilesError> {
        self.run(move |db| queries::recent_history(&db.conn, profile_id.as_deref(), limit))
            .await
    }

    /// Full-text search over recorded query text (FTS5 when available, a
    /// `LIKE` scan otherwise — see `queries::search_history`).
    pub async fn search_history(
        &self,
        profile_id: Option<String>,
        query: String,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>, ProfilesError> {
        self.run(move |db| queries::search_history(db, profile_id.as_deref(), &query, limit))
            .await
    }

    // -- saved_query -----------------------------------------------------

    pub async fn create_saved_query(&self, q: SavedQuery) -> Result<SavedQuery, ProfilesError> {
        self.run(move |db| queries::create_saved_query(&db.conn, q))
            .await
    }

    pub async fn get_saved_query(
        &self,
        id: impl Into<String>,
    ) -> Result<Option<SavedQuery>, ProfilesError> {
        let id = id.into();
        self.run(move |db| queries::get_saved_query(&db.conn, &id))
            .await
    }

    pub async fn list_saved_queries(&self) -> Result<Vec<SavedQuery>, ProfilesError> {
        self.run(|db| queries::list_saved_queries(&db.conn)).await
    }

    pub async fn update_saved_query(&self, mut q: SavedQuery) -> Result<SavedQuery, ProfilesError> {
        q.updated_at = now_ms();
        self.run(move |db| queries::update_saved_query(&db.conn, q))
            .await
    }

    pub async fn delete_saved_query(&self, id: impl Into<String>) -> Result<(), ProfilesError> {
        let id = id.into();
        self.run(move |db| queries::delete_saved_query(&db.conn, &id))
            .await
    }

    // -- editor_tab: crash-safe session restore --------------------------

    /// Atomically replaces the entire open-tabs set. Called on every editor
    /// change of note — not on a timer, this crate runs none — so a crash
    /// never loses more than the in-flight keystroke.
    pub async fn save_all_tabs(&self, tabs: Vec<EditorTab>) -> Result<(), ProfilesError> {
        self.run(move |db| queries::save_all_tabs(&mut db.conn, tabs))
            .await
    }

    pub async fn restore_all_tabs(&self) -> Result<Vec<EditorTab>, ProfilesError> {
        self.run(|db| queries::restore_all_tabs(&db.conn)).await
    }

    // -- kv ----------------------------------------------------------------

    pub async fn kv_get(&self, key: impl Into<String>) -> Result<Option<String>, ProfilesError> {
        let key = key.into();
        self.run(move |db| queries::kv_get(&db.conn, &key)).await
    }

    pub async fn kv_set(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), ProfilesError> {
        let key = key.into();
        let value = value.into();
        self.run(move |db| queries::kv_set(&db.conn, &key, &value))
            .await
    }

    pub async fn kv_delete(&self, key: impl Into<String>) -> Result<(), ProfilesError> {
        let key = key.into();
        self.run(move |db| queries::kv_delete(&db.conn, &key)).await
    }

    // -- TOML export/import ---------------------------------------------

    /// Git-committable TOML of folders, profiles, and tunnels. Secrets are
    /// excluded by construction — `Profile`/`Tunnel` only ever carry
    /// `secret_ref`, never a secret value.
    pub async fn export_profiles(&self) -> Result<String, ProfilesError> {
        self.run(|db| {
            let bundle = ExportBundle {
                version: 1,
                folder: queries::list_folders(&db.conn)?,
                profile: queries::list_profiles(&db.conn, None)?,
                tunnel: queries::list_tunnels(&db.conn)?,
            };
            bundle.to_toml()
        })
        .await
    }

    /// Imports a TOML export produced by [`Store::export_profiles`], matched
    /// on `id`. `strategy` controls whether existing rows not present in
    /// `toml` are left alone (`Merge`) or removed (`Replace`).
    pub async fn import_profiles(
        &self,
        toml: String,
        strategy: ImportStrategy,
    ) -> Result<ImportSummary, ProfilesError> {
        self.run(move |db| {
            let bundle = ExportBundle::from_toml(&toml)?;
            crate::export::apply_import(&mut db.conn, bundle, strategy)
        })
        .await
    }
}
