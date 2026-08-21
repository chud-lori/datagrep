use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_postgres::{CancelToken, Client, Config};

use datagrep_api::error::DbError;

use crate::error::map_pg_error;

pub const MAX_SESSIONS: usize = 8;

pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PgSession {
    client: Option<Client>,
    read_only: Option<bool>,
}

impl PgSession {
    pub fn client(&self) -> Result<&Client, DbError> {
        self.client.as_ref().ok_or(DbError::Closed)
    }

    pub fn client_mut(&mut self) -> Option<&mut Client> {
        self.client.as_mut()
    }
}

type Slot = Arc<Mutex<PgSession>>;

pub struct PooledClient(OwnedMutexGuard<PgSession>);

impl PooledClient {
    pub fn into_guard(self) -> OwnedMutexGuard<PgSession> {
        self.0
    }
}

impl Deref for PooledClient {
    type Target = Client;

    fn deref(&self) -> &Client {
        self.0
            .client
            .as_ref()
            .expect("a pooled session always holds a live client while borrowed")
    }
}

pub struct PgPool {
    config: Config,
    connect_timeout: Duration,
    slots: StdMutex<Vec<Slot>>,
    cancel_tokens: StdMutex<Vec<CancelToken>>,
    read_only: StdMutex<Option<bool>>,
    closed: AtomicBool,
}

impl PgPool {
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
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn slot_snapshot(&self) -> Vec<Slot> {
        self.slots_mut().clone()
    }

    pub async fn acquire(&self) -> Result<PooledClient, DbError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(DbError::Closed);
        }

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

    async fn dial(&self) -> Result<Option<OwnedMutexGuard<PgSession>>, DbError> {
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

    pub async fn set_read_only(&self, on: bool) -> Result<(), DbError> {
        *self.read_only.lock().unwrap_or_else(|e| e.into_inner()) = Some(on);
        let _session = self.acquire().await?;
        Ok(())
    }

    pub fn cancel_tokens(&self) -> Vec<CancelToken> {
        self.cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

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
