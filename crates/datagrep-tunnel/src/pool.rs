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

const IDLE_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TunnelKey {
    host: String,
    port: u16,
    user: String,
}

struct Entry<P: HostKeyPolicy + 'static> {
    tunnel: SshTunnel<P>,
    refs: Arc<AtomicUsize>,
    generation: Arc<AtomicUsize>,
}

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

        let tunnel =
            SshTunnel::connect(key.host.clone(), key.port, key.user.clone(), auth, policy).await?;

        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get(&key) {
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

    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

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
                    entries.remove(&key);
                }
            }
        });
    }
}
