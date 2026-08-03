//! Host-key verification (design §3.8, security item 6: "SSH host-key
//! pinning with an explicit change prompt").
//!
//! [`HostKeyPolicy`] is what [`crate::SshTunnel::connect`] consults during
//! the handshake. [`TofuStore`] is the shipped implementation: trust On
//! First Use, persisted to a flat file (`host:port base64-key`, one entry
//! per pinned host — design brief's literal format). Three outcomes:
//!
//! - **Known, matches** → the handshake proceeds silently.
//! - **Unknown** → a [`HostKeyDecision`] is sent to whoever is listening on
//!   [`TofuStore::decisions`] (the UI); the handshake blocks on that
//!   decision (never on a TTY read — same rule dbx-secrets' `prompt:` ref
//!   follows).
//! - **Changed** → a hard [`TunnelError::HostKeyChanged`], naming both
//!   fingerprints, never auto-accepted.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use russh::keys::{HashAlg, PublicKey};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::base64;
use crate::TunnelError;

/// Consulted once per handshake with the server's offered host key.
///
/// Implementations decide whether to proceed. `check` runs on the async
/// handshake path, so it must not block the thread (file I/O here goes
/// through blocking-free buffered reads done once at construction, not per
/// call — see [`TofuStore::open`]).
pub trait HostKeyPolicy: Send + Sync {
    /// `host`/`port` are the address actually dialed (post `~/.ssh/config`
    /// resolution). Ok(()) proceeds; Err aborts the handshake with that
    /// error.
    fn check(
        &self,
        host: &str,
        port: u16,
        key: &PublicKey,
    ) -> impl Future<Output = Result<(), TunnelError>> + Send;
}

/// A first-sight host key awaiting a UI decision. Sent on
/// [`TofuStore::decisions`]; the handshake that triggered it is suspended
/// awaiting the paired answer until [`accept`](Self::accept) or
/// [`reject`](Self::reject) is called (or this value is dropped, which
/// counts as reject — a UI that crashes must not silently trust a key).
#[derive(Debug)]
pub struct HostKeyDecision {
    host: String,
    port: u16,
    fingerprint: String,
    respond: oneshot::Sender<bool>,
}

impl HostKeyDecision {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `SHA256:base64…`, the same form `ssh-keygen -l` prints. Not secret —
    /// safe to show the user and to log.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Pin this key and let the handshake proceed.
    pub fn accept(self) {
        let _ = self.respond.send(true);
    }

    /// Refuse this key; the handshake fails with [`TunnelError::HostKeyRejected`].
    pub fn reject(self) {
        let _ = self.respond.send(false);
    }
}

/// Trust-on-first-use host key store, persisted as one `host:port
/// base64-key` line per pinned host (design §3.8).
pub struct TofuStore {
    path: PathBuf,
    // Raw SSH wire-format key bytes (`PublicKey::to_bytes`), keyed by the
    // exact (host, port) pair dialed — matches the file format, which has
    // no room for multiple key types per host.
    entries: Mutex<HashMap<(String, u16), Vec<u8>>>,
    decisions: mpsc::UnboundedSender<HostKeyDecision>,
}

impl std::fmt::Debug for TofuStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TofuStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl TofuStore {
    /// Open (creating if absent) the known-hosts file at `path`. Returns the
    /// store plus the receiving end of unknown-host prompts — the UI layer
    /// owns draining that channel and calling `accept`/`reject`; a store
    /// with nobody listening will fail every unknown-host check with
    /// [`TunnelError::NoPromptListener`] rather than hang forever.
    pub async fn open(
        path: impl Into<PathBuf>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<HostKeyDecision>), TunnelError> {
        let path = path.into();
        let entries = load(&path).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        Ok((
            Self {
                path,
                entries: Mutex::new(entries),
                decisions: tx,
            },
            rx,
        ))
    }

    /// Default location: `~/.dbx/known_hosts` (or platform config dir if
    /// `$HOME` is unset — see `dirs::config_dir`).
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("dbx")
            .join("known_hosts")
    }

    async fn persist(&self) -> Result<(), TunnelError> {
        let entries = self.entries.lock().await;
        let mut body = String::new();
        // BTreeMap-free deterministic-enough output: sort for stable diffs.
        let mut rows: Vec<_> = entries.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        for ((host, port), key) in rows {
            body.push_str(host);
            body.push(':');
            body.push_str(&port.to_string());
            body.push(' ');
            body.push_str(&base64::encode(key));
            body.push('\n');
        }
        drop(entries);
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| TunnelError::KnownHosts {
                    path: self.path.clone(),
                    source,
                })?;
        }
        tokio::fs::write(&self.path, body)
            .await
            .map_err(|source| TunnelError::KnownHosts {
                path: self.path.clone(),
                source,
            })
    }
}

async fn load(path: &Path) -> Result<HashMap<(String, u16), Vec<u8>>, TunnelError> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(source) => {
            return Err(TunnelError::KnownHosts {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let mut out = HashMap::new();
    for (lineno, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (addr, key_b64) = line
            .split_once(' ')
            .ok_or_else(|| TunnelError::KnownHostsParse {
                path: path.to_path_buf(),
                line: lineno + 1,
                reason: "expected `host:port base64-key`".to_owned(),
            })?;
        let (host, port) = addr
            .rsplit_once(':')
            .ok_or_else(|| TunnelError::KnownHostsParse {
                path: path.to_path_buf(),
                line: lineno + 1,
                reason: "expected `host:port` before the key".to_owned(),
            })?;
        let port: u16 = port.parse().map_err(|_| TunnelError::KnownHostsParse {
            path: path.to_path_buf(),
            line: lineno + 1,
            reason: format!("`{port}` is not a valid port"),
        })?;
        let key = base64::decode(key_b64).ok_or_else(|| TunnelError::KnownHostsParse {
            path: path.to_path_buf(),
            line: lineno + 1,
            reason: "key is not valid base64".to_owned(),
        })?;
        out.insert((host.to_owned(), port), key);
    }
    Ok(out)
}

/// SHA256 fingerprint, `ssh-keygen -l` form. Falls back to a fixed string on
/// (unreachable in practice) decode failure rather than panicking — this
/// runs on the handshake path and must never `unwrap`.
fn fingerprint_of(bytes: &[u8]) -> String {
    match PublicKey::from_bytes(bytes) {
        Ok(key) => key.fingerprint(HashAlg::Sha256).to_string(),
        Err(_) => "<unparseable pinned key>".to_owned(),
    }
}

impl HostKeyPolicy for TofuStore {
    async fn check(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), TunnelError> {
        let offered = key
            .to_bytes()
            .map_err(|source| TunnelError::HostKeyEncode {
                host: host.to_owned(),
                port,
                reason: source.to_string(),
            })?;

        let existing = {
            let entries = self.entries.lock().await;
            entries.get(&(host.to_owned(), port)).cloned()
        };

        match existing {
            Some(pinned) if pinned == offered => Ok(()),
            Some(pinned) => Err(TunnelError::HostKeyChanged {
                host: host.to_owned(),
                port,
                expected_fingerprint: fingerprint_of(&pinned),
                offered_fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            }),
            None => {
                let (respond, await_decision) = oneshot::channel();
                let decision = HostKeyDecision {
                    host: host.to_owned(),
                    port,
                    fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                    respond,
                };
                self.decisions
                    .send(decision)
                    .map_err(|_| TunnelError::NoPromptListener {
                        host: host.to_owned(),
                        port,
                    })?;

                match await_decision.await {
                    Ok(true) => {
                        self.entries
                            .lock()
                            .await
                            .insert((host.to_owned(), port), offered);
                        self.persist().await?;
                        Ok(())
                    }
                    Ok(false) | Err(_) => Err(TunnelError::HostKeyRejected {
                        host: host.to_owned(),
                        port,
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dbx-tunnel-test-{name}-{nanos}-{:?}",
            std::thread::current().id()
        ))
    }

    // Two distinct throwaway ed25519 public keys (`ssh-keygen -t ed25519`,
    // no matching private key kept anywhere) — enough to exercise
    // known/unknown/changed without needing a signing capability, so no
    // extra `rand`/`rand_core` dev-dependency is required (deps are pinned).
    const TEST_KEY_1: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKsDzHtaiI1omYo/DkchNpnOQStfPXYZBi/N82zxsxSA test1";
    const TEST_KEY_2: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKsItM3o1/M4G5CylJPyp1dbk9q6xHchRRy+NwIdQUuw test2";

    fn test_key(seed: u8) -> PublicKey {
        let s = if seed % 2 == 0 {
            TEST_KEY_1
        } else {
            TEST_KEY_2
        };
        PublicKey::from_openssh(s).expect("static test key parses")
    }

    #[tokio::test]
    async fn unknown_host_accept_persists_and_reconnect_is_known() {
        let path = unique_temp_path("accept");
        let (store, mut decisions) = TofuStore::open(&path).await.unwrap();
        let key = test_key(1);

        let check = tokio::spawn({
            let key = key.clone();
            async move { store.check("example.com", 22, &key).await.map(|_| store) }
        });

        let decision = decisions.recv().await.expect("prompt sent");
        assert_eq!(decision.host(), "example.com");
        assert_eq!(decision.port(), 22);
        decision.accept();

        let store = check.await.unwrap().unwrap();
        // Reconnecting to the same host+key now needs no prompt.
        store.check("example.com", 22, &key).await.unwrap();

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn unknown_host_reject_fails_and_does_not_persist() {
        let path = unique_temp_path("reject");
        let (store, mut decisions) = TofuStore::open(&path).await.unwrap();
        let key = test_key(2);

        let check = tokio::spawn({
            let key = key.clone();
            async move { store.check("reject.example", 22, &key).await }
        });
        let decision = decisions.recv().await.expect("prompt sent");
        decision.reject();

        let err = check.await.unwrap().unwrap_err();
        assert!(matches!(err, TunnelError::HostKeyRejected { .. }));
        assert!(
            !tokio::fs::try_exists(&path).await.unwrap_or(false),
            "a rejected key must never be persisted"
        );
    }

    #[tokio::test]
    async fn changed_key_is_a_hard_error_naming_both_fingerprints() {
        let path = unique_temp_path("changed");
        let (store, mut decisions) = TofuStore::open(&path).await.unwrap();
        let first = test_key(3);

        let check = tokio::spawn({
            let first = first.clone();
            async move {
                store
                    .check("rotates.example", 22, &first)
                    .await
                    .map(|_| store)
            }
        });
        decisions.recv().await.unwrap().accept();
        let store = check.await.unwrap().unwrap();

        let second = test_key(4);
        let err = store
            .check("rotates.example", 22, &second)
            .await
            .unwrap_err();
        match &err {
            TunnelError::HostKeyChanged {
                expected_fingerprint,
                offered_fingerprint,
                ..
            } => {
                assert_ne!(expected_fingerprint, offered_fingerprint);
                assert!(expected_fingerprint.starts_with("SHA256:"));
                assert!(offered_fingerprint.starts_with("SHA256:"));
            }
            other => panic!("expected HostKeyChanged, got {other:?}"),
        }
        let rendered = err.to_string();
        assert!(rendered.contains("HOST KEY CHANGED"));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn no_listener_errors_instead_of_hanging() {
        let path = unique_temp_path("no-listener");
        let (store, decisions) = TofuStore::open(&path).await.unwrap();
        drop(decisions); // nobody is listening for the prompt
        let key = test_key(5);

        let err = store
            .check("nobody-home.example", 22, &key)
            .await
            .unwrap_err();
        assert!(matches!(err, TunnelError::NoPromptListener { .. }));
    }
}
