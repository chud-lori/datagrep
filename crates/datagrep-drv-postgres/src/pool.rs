//! [`PgPool`] — the small set of *physical* Postgres sessions backing one
//! *logical* [`crate::connection::PgConnection`].
//!
//! # Why this exists (the deadlock this replaces)
//!
//! The driver used to hold exactly one `Client` behind one
//! `Arc<Mutex<Option<Client>>>`. That is forced by two hard constraints:
//!
//! 1. `tokio_postgres::Transaction<'a>` borrows `&'a mut Client`, and a
//!    streaming portal only exists inside a transaction — so a live cursor
//!    *must* keep exclusive use of its `Client` (see `actor.rs`).
//! 2. Design §3.5: "a pool that silently moves a BEGIN to a different socket
//!    is a correctness bug" — a transaction or cursor **pins** its session.
//!
//! Both are true and neither is negotiable. The bug was the conclusion drawn
//! from them: that *everything else* should queue behind the pinned session.
//! `catalog()` and the next `execute()` awaited the same mutex with no
//! timeout, so "results grid open + click the schema tree" — the GUI's
//! bread-and-butter interleaving — froze the driver forever, with the server
//! showing `idle in transaction` and nothing ever returning an error.
//!
//! The fix has three layers, in the order they take effect:
//!
//! * **Release early.** A cursor pins its session only until the portal is
//!   *drained* (or the cursor is closed/dropped), not until the handle goes
//!   out of scope. [`crate::cursor::PgCursor`] rolls back its transparent
//!   read-only wrapper transaction the moment it sees a short/empty batch, so
//!   the overwhelmingly common case — read a result, then do something else —
//!   never contends at all. This also matters *server-side*: an open
//!   transaction that has read a table holds an `ACCESS SHARE` lock, so a
//!   later `DROP TABLE` would block on the lock even from a second socket.
//! * **Acquire a different session.** When a session genuinely is pinned (a
//!   half-scrolled grid, an open interactive transaction), anything else that
//!   needs the server takes a *different* physical connection from this pool,
//!   dialled lazily with the same config. Catalog browsing and the next query
//!   therefore never wait on a cursor. This is option 1 of the design note:
//!   pinned means pinned, so everyone else takes another socket.
//! * **Never hang.** The pool is capped at [`MAX_SESSIONS`]. At the cap,
//!   [`PgPool::acquire`] waits with a bounded [`ACQUIRE_TIMEOUT`] and then
//!   returns [`DbError::ResourceExhausted`] naming what is holding the
//!   sessions. A database client that freezes silently is worse than one that
//!   errors.
//!
//! Session-level state (`SET SESSION CHARACTERISTICS ... READ ONLY`) is
//! recorded per session and reconciled lazily on acquire, so a session that
//! was pinned when `set_read_only` was called still gets the setting before it
//! is handed out again — the pool never serves a socket whose session state
//! disagrees with what the caller asked for.

use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_postgres::{CancelToken, Client, Config};

use datagrep_api::error::DbError;

use crate::error::map_pg_error;

/// Hard ceiling on physical sessions per logical connection. Each one is a
/// real backend process on the server, so this is deliberately small: it is a
/// safety valve for interleaved use, not a throughput pool.
pub const MAX_SESSIONS: usize = 8;

/// How long [`PgPool::acquire`] waits for a session once the pool is at
/// [`MAX_SESSIONS`] before giving up with `ResourceExhausted`.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// One physical Postgres session plus the session-level state we have applied
/// to *this* socket. `client: None` means the session was closed by
/// [`PgPool::close`] and must never be handed out again.
pub struct PgSession {
    client: Option<Client>,
    /// The last `SET SESSION CHARACTERISTICS AS TRANSACTION READ …` applied
    /// to this socket; `None` = never set (server default).
    read_only: Option<bool>,
}

impl PgSession {
    /// The live client, or `DbError::Closed` if this session was closed.
    pub fn client(&self) -> Result<&Client, DbError> {
        self.client.as_ref().ok_or(DbError::Closed)
    }

    /// Mutable access, for `Client::build_transaction` (which needs `&mut`).
    pub fn client_mut(&mut self) -> Option<&mut Client> {
        self.client.as_mut()
    }
}

type Slot = Arc<Mutex<PgSession>>;

/// An exclusively-held session, borrowed from the pool for the life of this
/// value. Derefs to the underlying `Client`.
pub struct PooledClient(OwnedMutexGuard<PgSession>);

impl PooledClient {
    /// Hand the raw guard to the transaction actor, which needs to own the
    /// session for the whole life of its `Transaction` (see `actor.rs`).
    pub fn into_guard(self) -> OwnedMutexGuard<PgSession> {
        self.0
    }
}

impl Deref for PooledClient {
    type Target = Client;

    fn deref(&self) -> &Client {
        // Invariant: `PgPool::acquire` only ever returns a guard whose
        // `client` is `Some`, and nothing can take it while we hold the
        // guard — `PgPool::close` uses `try_lock` and skips busy sessions.
        self.0
            .client
            .as_ref()
            .expect("a pooled session always holds a live client while borrowed")
    }
}

pub struct PgPool {
    /// How to dial another session identical to the first one.
    config: Config,
    connect_timeout: Duration,
    slots: StdMutex<Vec<Slot>>,
    /// One cancel token per physical session, so `Connection::canceller` can
    /// cancel every socket this logical connection owns.
    cancel_tokens: StdMutex<Vec<CancelToken>>,
    /// Desired session read-only state, applied lazily on acquire.
    read_only: StdMutex<Option<bool>>,
    closed: AtomicBool,
}

impl PgPool {
    /// Build a pool around the already-established primary session.
    pub fn with_primary(config: Config, connect_timeout: Duration, client: Client) -> Arc<Self> {
        let pool = Arc::new(Self {
            config,
            connect_timeout,
            slots: StdMutex::new(Vec::new()),
            cancel_tokens: StdMutex::new(vec![client.cancel_token()]),
            read_only: StdMutex::new(None),
            closed: AtomicBool::new(false),
        });
        pool.slots_mut().push(Arc::new(Mutex::new(PgSession {
            client: Some(client),
            read_only: None,
        })));
        pool
    }

    fn slots_mut(&self) -> std::sync::MutexGuard<'_, Vec<Slot>> {
        // A poisoned lock here would mean a panic inside a few lines of
        // `Vec` manipulation; recovering the data is strictly better than
        // turning it into a second panic.
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn slot_snapshot(&self) -> Vec<Slot> {
        self.slots_mut().clone()
    }

    /// Borrow a session for one unit of work. Prefers an idle session; dials
    /// a new one when every existing session is pinned by a live cursor or
    /// transaction; and — only once at [`MAX_SESSIONS`] — waits with a
    /// deadline rather than forever.
    pub async fn acquire(&self) -> Result<PooledClient, DbError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(DbError::Closed);
        }

        // Two passes: a cursor that has just finished releases its session
        // when its actor task drops the guard, which can be one scheduler
        // tick behind the reply we already observed. Yielding once keeps the
        // common "read a result, then run the next query" case on a single
        // socket instead of needlessly dialling a second one.
        for attempt in 0..2 {
            if let Some(guard) = self.take_idle() {
                return self.reconcile(guard).await;
            }
            if attempt == 0 {
                tokio::task::yield_now().await;
            }
        }

        match self.dial().await {
            Ok(Some(guard)) => return self.reconcile(guard).await,
            Ok(None) => {} // at the cap — fall through to the bounded wait
            Err(e) => return Err(e),
        }

        let slots = self.slot_snapshot();
        if slots.is_empty() {
            return Err(DbError::Closed);
        }
        let waits: Vec<_> = slots
            .into_iter()
            .map(|slot| Box::pin(slot.lock_owned()))
            .collect();
        let (guard, _, _) =
            tokio::time::timeout(ACQUIRE_TIMEOUT, futures_util::future::select_all(waits))
                .await
                .map_err(|_| {
                    DbError::ResourceExhausted(format!(
                        "all {MAX_SESSIONS} Postgres sessions on this connection are pinned by \
                         open result cursors or transactions, and none was released within {}s — \
                         close a result set (or commit/roll back an open transaction) and retry",
                        ACQUIRE_TIMEOUT.as_secs()
                    ))
                })?;
        if guard.client.is_none() {
            return Err(DbError::Closed);
        }
        self.reconcile(guard).await
    }

    /// First session that is neither pinned nor dead. Dead sessions (their
    /// background connection task ended) are pruned as they're found.
    fn take_idle(&self) -> Option<OwnedMutexGuard<PgSession>> {
        let mut dead: Vec<Slot> = Vec::new();
        let mut found = None;
        for slot in self.slot_snapshot() {
            let Ok(guard) = slot.clone().try_lock_owned() else {
                continue; // pinned by a cursor/transaction — leave it alone
            };
            match guard.client.as_ref() {
                Some(c) if !c.is_closed() => {
                    found = Some(guard);
                    break;
                }
                Some(_) => dead.push(slot),
                None => dead.push(slot),
            }
        }
        if !dead.is_empty() {
            let mut slots = self.slots_mut();
            slots.retain(|s| !dead.iter().any(|d| Arc::ptr_eq(d, s)));
        }
        found
    }

    /// Dial one more physical session. `Ok(None)` means the pool is already
    /// at [`MAX_SESSIONS`] and the caller should wait instead.
    async fn dial(&self) -> Result<Option<OwnedMutexGuard<PgSession>>, DbError> {
        // Reserve the slot *before* the await so two concurrent acquirers
        // can't both decide there is room and overshoot the cap.
        let slot: Slot = Arc::new(Mutex::new(PgSession {
            client: None,
            read_only: None,
        }));
        let mut guard = slot.clone().lock_owned().await; // uncontended by construction
        {
            let mut slots = self.slots_mut();
            if slots.len() >= MAX_SESSIONS {
                return Ok(None);
            }
            slots.push(slot.clone());
        }

        tracing::debug!("dialling an additional postgres session (existing ones are pinned)");
        match self.connect_one().await {
            Ok(client) => {
                guard.client = Some(client);
                Ok(Some(guard))
            }
            Err(e) => {
                let mut slots = self.slots_mut();
                slots.retain(|s| !Arc::ptr_eq(s, &slot));
                Err(e)
            }
        }
    }

    async fn connect_one(&self) -> Result<Client, DbError> {
        let connect_fut = self.config.connect(tokio_postgres::NoTls);
        let (client, connection) = tokio::time::timeout(self.connect_timeout, connect_fut)
            .await
            .map_err(|_| DbError::Timeout)?
            .map_err(|e| DbError::Connect(e.to_string()))?;
        self.cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(client.cancel_token());
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!(error = %e, "postgres connection task ended with an error");
            }
        });
        Ok(client)
    }

    /// Bring a session's session-level state in line with what the caller
    /// last asked for before handing it out.
    async fn reconcile(
        &self,
        mut guard: OwnedMutexGuard<PgSession>,
    ) -> Result<PooledClient, DbError> {
        let desired = *self.read_only.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(want) = desired {
            if guard.read_only != Some(want) {
                let sql = read_only_sql(want);
                {
                    let client = guard.client()?;
                    client.batch_execute(sql).await.map_err(map_pg_error)?;
                }
                guard.read_only = Some(want);
            }
        }
        if guard.client.is_none() {
            return Err(DbError::Closed);
        }
        Ok(PooledClient(guard))
    }

    /// Record the desired session read-only state and prove it applies by
    /// acquiring one session (which reconciles it). Sessions currently pinned
    /// by a cursor or transaction pick the setting up when they are next
    /// acquired — they cannot change mid-transaction anyway, and the
    /// transaction they are inside already carries its own read-only mode.
    pub async fn set_read_only(&self, on: bool) -> Result<(), DbError> {
        *self.read_only.lock().unwrap_or_else(|e| e.into_inner()) = Some(on);
        let _session = self.acquire().await?;
        Ok(())
    }

    /// Every live session's cancel token, snapshotted now.
    pub fn cancel_tokens(&self) -> Vec<CancelToken> {
        self.cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Idempotent close. Every later `acquire` returns `DbError::Closed`.
    /// Idle sessions have their `Client` dropped right here so the socket
    /// shuts down promptly; a session still pinned by a live cursor is left
    /// to its actor, which drops the last `Arc` — and with it the `Client` —
    /// when it finishes. Deliberately never blocks on a pinned session:
    /// `close()` hanging behind an open cursor was part of the original bug.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let slots = std::mem::take(&mut *self.slots_mut());
        for slot in slots {
            if let Ok(mut guard) = slot.try_lock() {
                guard.client.take();
            }
        }
        self.cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Number of physical sessions currently open — test/telemetry only.
    pub fn session_count(&self) -> usize {
        self.slots_mut().len()
    }
}

fn read_only_sql(on: bool) -> &'static str {
    if on {
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"
    } else {
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_sql_matches_the_two_session_modes() {
        assert!(read_only_sql(true).ends_with("READ ONLY"));
        assert!(read_only_sql(false).ends_with("READ WRITE"));
    }

    /// The cap is what turns "wait forever" into "wait a bounded time, then
    /// say who is holding the sessions" — pin both numbers so a later edit
    /// can't quietly reintroduce an unbounded wait.
    #[test]
    fn acquire_is_bounded_and_capped() {
        let max = std::hint::black_box(MAX_SESSIONS);
        let timeout = std::hint::black_box(ACQUIRE_TIMEOUT);
        assert!(
            max >= 2,
            "interleaving a cursor with anything else needs a second session"
        );
        assert!(
            timeout > Duration::ZERO && timeout <= Duration::from_secs(60),
            "the acquire wait must be bounded — hanging forever is the bug this replaced"
        );
    }
}
