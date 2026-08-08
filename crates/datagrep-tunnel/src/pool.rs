//! [`TunnelPool`] — one SSH connection per `(host, port, user)`, shared by
//! refcounted checkout, torn down after the last checkout drops plus an
//! idle grace period — the same connection-lifecycle stance datagrep takes
//! for database connections, applied at the tunnel layer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::auth::Auth;
use crate::bridge::LocalEnd;
use crate::error::TunnelError;
use crate::host_key::HostKeyPolicy;
use crate::tunnel::SshTunnel;

/// How long a tunnel with zero checked-out clones is kept alive before the
/// pool drops its own reference. Event-driven (a single timer armed only
/// when the count reaches zero — see [`Checkout::drop`]), not a recurring
/// poll: an idle app should be an idle CPU.
const IDLE_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TunnelKey {
    host: String,
    port: u16,
    user: String,
}

struct Entry<P: HostKeyPolicy + 'static> {
    tunnel: SshTunnel<P>,
    /// Number of live [`Checkout`]s referencing this entry. The pool's own
    /// map entry is not counted — reaching zero means "nobody outside the
    /// pool holds this tunnel right now".
    refs: Arc<AtomicUsize>,
    /// Bumped on every new checkout; a pending eviction task compares its
    /// captured generation against the current one so a reconnect during
    /// the grace window cancels the stale eviction instead of racing it.
    generation: Arc<AtomicUsize>,
}

/// Shared pool of [`SshTunnel`]s, keyed by `(host, port, user)`.
pub struct TunnelPool<P: HostKeyPolicy + 'static> {
    entries: Arc<Mutex<HashMap<TunnelKey, Entry<P>>>>,
}

impl<P: HostKeyPolicy + 'static> std::fmt::Debug for TunnelPool<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelPool").finish_non_exhaustive()
    }
}

impl<P: HostKeyPolicy + 'static> Default for TunnelPool<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: HostKeyPolicy + 'static> TunnelPool<P> {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get a connected, checked-out tunnel for `(host, port, user)`,
    /// connecting fresh if none is pooled (or the pooled one died — see
    /// [`SshTunnel::connect`]'s keepalive-none note: death is detected
    /// lazily, so a stale entry is replaced on next use, not proactively).
    pub async fn checkout(
        &self,
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        auth: Auth,
        policy: Arc<P>,
    ) -> Result<Checkout<P>, TunnelError> {
        let key = TunnelKey {
            host: host.into(),
            port,
            user: user.into(),
        };

        if let Some(checkout) = self.try_checkout_existing(&key).await {
            return Ok(checkout);
        }

        // Connect *without* holding the pool lock — a handshake is a
        // network round trip (real SSH tests take tens to hundreds of ms;
        // a hung server can take far longer) and a `tokio::sync::Mutex`
        // held across that would stall every other host's checkout, not
        // just this one. Two callers racing to connect the same
        // `(host, port, user)` for the first time both pay the connect
        // cost. That is the same tradeoff datagrep makes for DB connections:
        // there is no idle floor to keep, because a reconnect costs tens of
        // milliseconds and nobody notices. So the loser's redundant
        // connection is cheap, not incorrect — it's simply dropped, per the
        // double-checked insert below.
        let tunnel =
            SshTunnel::connect(key.host.clone(), key.port, key.user.clone(), auth, policy).await?;

        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get(&key) {
            // Someone else won the race and inserted first; use theirs and
            // let `tunnel` (ours) drop, closing the redundant connection.
            entry.refs.fetch_add(1, Ordering::SeqCst);
            entry.generation.fetch_add(1, Ordering::SeqCst);
            return Ok(Checkout {
                tunnel: entry.tunnel.clone(),
                refs: entry.refs.clone(),
                generation: entry.generation.clone(),
                pool: self.entries.clone(),
                key,
            });
        }
        let refs = Arc::new(AtomicUsize::new(1));
        let generation = Arc::new(AtomicUsize::new(0));
        entries.insert(
            key.clone(),
            Entry {
                tunnel: tunnel.clone(),
                refs: refs.clone(),
                generation: generation.clone(),
            },
        );
        Ok(Checkout {
            tunnel,
            refs,
            generation,
            pool: self.entries.clone(),
            key,
        })
    }

    async fn try_checkout_existing(&self, key: &TunnelKey) -> Option<Checkout<P>> {
        let entries = self.entries.lock().await;
        let entry = entries.get(key)?;
        entry.refs.fetch_add(1, Ordering::SeqCst);
        entry.generation.fetch_add(1, Ordering::SeqCst);
        Some(Checkout {
            tunnel: entry.tunnel.clone(),
            refs: entry.refs.clone(),
            generation: entry.generation.clone(),
            pool: self.entries.clone(),
            key: key.clone(),
        })
    }

    /// Number of distinct `(host, port, user)` tunnels currently pooled
    /// (checked out or idle-within-grace). For tests/diagnostics.
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// A refcounted checkout from a [`TunnelPool`]. Derefs to the underlying
/// [`SshTunnel`] to open channels; dropping it releases the pool's
/// reference, arming the idle-grace eviction once the count reaches zero.
pub struct Checkout<P: HostKeyPolicy + 'static> {
    tunnel: SshTunnel<P>,
    refs: Arc<AtomicUsize>,
    generation: Arc<AtomicUsize>,
    pool: Arc<Mutex<HashMap<TunnelKey, Entry<P>>>>,
    key: TunnelKey,
}

impl<P: HostKeyPolicy + 'static> Checkout<P> {
    pub async fn open_channel(
        &self,
        target_host: impl Into<String>,
        target_port: u16,
    ) -> Result<LocalEnd, TunnelError> {
        self.tunnel.open_channel(target_host, target_port).await
    }

    pub fn host(&self) -> &str {
        self.tunnel.host()
    }

    pub fn port(&self) -> u16 {
        self.tunnel.port()
    }
}

impl<P: HostKeyPolicy + 'static> std::fmt::Debug for Checkout<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkout")
            .field("host", &self.key.host)
            .field("port", &self.key.port)
            .field("user", &self.key.user)
            .finish_non_exhaustive()
    }
}

impl<P: HostKeyPolicy + 'static> Clone for Checkout<P> {
    fn clone(&self) -> Self {
        self.refs.fetch_add(1, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        Self {
            tunnel: self.tunnel.clone(),
            refs: self.refs.clone(),
            generation: self.generation.clone(),
            pool: self.pool.clone(),
            key: self.key.clone(),
        }
    }
}

impl<P: HostKeyPolicy + 'static> Drop for Checkout<P> {
    fn drop(&mut self) {
        let remaining = self.refs.fetch_sub(1, Ordering::SeqCst) - 1;
        if remaining != 0 {
            return;
        }
        let expected_generation = self.generation.load(Ordering::SeqCst);
        let pool = self.pool.clone();
        let key = self.key.clone();
        let refs = self.refs.clone();
        let generation = self.generation.clone();
        // Single one-shot timer, armed only on the transition to zero —
        // not a recurring poll that would wake an otherwise idle app.
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_GRACE).await;
            if refs.load(Ordering::SeqCst) != 0 {
                return; // reconnected during the grace window
            }
            if generation.load(Ordering::SeqCst) != expected_generation {
                return; // a checkout/drop raced us; a fresher timer owns this
            }
            let mut entries = pool.lock().await;
            if let Some(entry) = entries.get(&key) {
                if entry.refs.load(Ordering::SeqCst) == 0
                    && entry.generation.load(Ordering::SeqCst) == expected_generation
                {
                    // Removing the last strong reference to `SshTunnel`'s
                    // `Arc<client::Handle<_>>` drops the `Handle`, which
                    // drops its sender to the session's background task,
                    // which ends the task and closes the socket — no
                    // explicit "close" call needed.
                    entries.remove(&key);
                }
            }
        });
    }
}
