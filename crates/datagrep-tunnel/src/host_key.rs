use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use russh::keys::{HashAlg, PublicKey};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::base64;
use crate::TunnelError;

pub trait HostKeyPolicy: Send + Sync {
    fn check(
        &self,
        host: &str,
        port: u16,
        key: &PublicKey,
    ) -> impl Future<Output = Result<(), TunnelError>> + Send;
}

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

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn accept(self) {
        let _ = self.respond.send(true);
    }

    pub fn reject(self) {
        let _ = self.respond.send(false);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    Trusted,
    Unknown,
    Changed { expected_fingerprint: String },
}

pub struct TofuStore {
    path: PathBuf,
    entries: Mutex<HashMap<(String, u16), Vec<u8>>>,
    decisions: Option<mpsc::UnboundedSender<HostKeyDecision>>,
    system_known_hosts: Option<PathBuf>,
}

impl std::fmt::Debug for TofuStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TofuStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl TofuStore {
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
                decisions: Some(tx),
                system_known_hosts: None,
            },
            rx,
        ))
    }

    /// Never prompts: an unknown key fails with `HostKeyUnknown` until `pin` records it.
    pub async fn open_unattended(
        path: impl Into<PathBuf>,
        system_known_hosts: Option<PathBuf>,
    ) -> Result<Self, TunnelError> {
        let path = path.into();
        let entries = load(&path).await?;
        Ok(Self {
            path,
            entries: Mutex::new(entries),
            decisions: None,
            system_known_hosts,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn system_default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".ssh").join("known_hosts"))
    }

    pub async fn status(
        &self,
        host: &str,
        port: u16,
        key: &PublicKey,
    ) -> Result<HostKeyStatus, TunnelError> {
        let offered = encode(host, port, key)?;
        let pinned = self
            .entries
            .lock()
            .await
            .get(&(host.to_owned(), port))
            .cloned();
        match pinned {
            Some(pinned) if pinned == offered => return Ok(HostKeyStatus::Trusted),
            Some(pinned) => {
                return Ok(HostKeyStatus::Changed {
                    expected_fingerprint: fingerprint_of(&pinned),
                })
            }
            None => {}
        }
        let Some(system) = &self.system_known_hosts else {
            return Ok(HostKeyStatus::Unknown);
        };
        // Read-only: the user's own file is consulted, never written.
        match russh::keys::check_known_hosts_path(host, port, key, system) {
            Ok(true) => Ok(HostKeyStatus::Trusted),
            Err(russh::keys::Error::KeyChanged { line }) => Ok(HostKeyStatus::Changed {
                expected_fingerprint: format!("the key at {} line {line}", system.display()),
            }),
            Ok(false) => Ok(HostKeyStatus::Unknown),
            Err(error) => {
                tracing::debug!(%error, "system known_hosts unreadable, treating host as unknown");
                Ok(HostKeyStatus::Unknown)
            }
        }
    }

    pub async fn pin(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), TunnelError> {
        let offered = encode(host, port, key)?;
        self.entries
            .lock()
            .await
            .insert((host.to_owned(), port), offered);
        self.persist().await
    }

    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("datagrep")
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

fn encode(host: &str, port: u16, key: &PublicKey) -> Result<Vec<u8>, TunnelError> {
    key.to_bytes().map_err(|source| TunnelError::HostKeyEncode {
        host: host.to_owned(),
        port,
        reason: source.to_string(),
    })
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

fn fingerprint_of(bytes: &[u8]) -> String {
    match PublicKey::from_bytes(bytes) {
        Ok(key) => key.fingerprint(HashAlg::Sha256).to_string(),
        Err(_) => "<unparseable pinned key>".to_owned(),
    }
}

impl HostKeyPolicy for TofuStore {
    async fn check(&self, host: &str, port: u16, key: &PublicKey) -> Result<(), TunnelError> {
        match self.status(host, port, key).await? {
            HostKeyStatus::Trusted => Ok(()),
            HostKeyStatus::Changed {
                expected_fingerprint,
            } => Err(TunnelError::HostKeyChanged {
                host: host.to_owned(),
                port,
                expected_fingerprint,
                offered_fingerprint: fingerprint(key),
            }),
            HostKeyStatus::Unknown => {
                let Some(decisions) = &self.decisions else {
                    return Err(TunnelError::HostKeyUnknown {
                        host: host.to_owned(),
                        port,
                        fingerprint: fingerprint(key),
                    });
                };
                let (respond, await_decision) = oneshot::channel();
                let decision = HostKeyDecision {
                    host: host.to_owned(),
                    port,
                    fingerprint: fingerprint(key),
                    respond,
                };
                decisions
                    .send(decision)
                    .map_err(|_| TunnelError::NoPromptListener {
                        host: host.to_owned(),
                        port,
                    })?;

                match await_decision.await {
                    Ok(true) => self.pin(host, port, key).await,
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
            "datagrep-tunnel-test-{name}-{nanos}-{:?}",
            std::thread::current().id()
        ))
    }

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

    #[tokio::test]
    async fn unattended_refuses_an_unknown_key_until_it_is_pinned() {
        let path = unique_temp_path("unattended");
        let store = TofuStore::open_unattended(&path, None).await.unwrap();
        let key = test_key(1);

        let err = store.check("bastion.example", 22, &key).await.unwrap_err();
        assert!(matches!(err, TunnelError::HostKeyUnknown { .. }), "{err:?}");
        assert!(!tokio::fs::try_exists(&path).await.unwrap_or(false));

        store.pin("bastion.example", 22, &key).await.unwrap();
        let reopened = TofuStore::open_unattended(&path, None).await.unwrap();
        reopened.check("bastion.example", 22, &key).await.unwrap();
        let err = reopened
            .check("bastion.example", 22, &test_key(2))
            .await
            .unwrap_err();
        assert!(matches!(err, TunnelError::HostKeyChanged { .. }), "{err:?}");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn the_system_known_hosts_file_is_trusted_and_a_mismatch_there_is_refused() {
        // A key offered on the wire carries no comment, and russh compares comments too.
        let bare = |line: &str| {
            let mut parts = line.split(' ');
            let (kind, body) = (parts.next().unwrap(), parts.next().unwrap());
            PublicKey::from_openssh(&format!("{kind} {body}")).unwrap()
        };
        let (known, other) = (bare(TEST_KEY_1), bare(TEST_KEY_2));
        let system = unique_temp_path("system");
        tokio::fs::write(
            &system,
            format!("[jump.example]:2222 {}\n", known.to_openssh().unwrap()),
        )
        .await
        .unwrap();
        let pins = unique_temp_path("pins");
        let store = TofuStore::open_unattended(&pins, Some(system.clone()))
            .await
            .unwrap();

        assert_eq!(
            store.status("jump.example", 2222, &known).await.unwrap(),
            HostKeyStatus::Trusted
        );
        assert!(matches!(
            store.status("jump.example", 2222, &other).await.unwrap(),
            HostKeyStatus::Changed { .. }
        ));
        assert_eq!(
            store.status("other.example", 22, &known).await.unwrap(),
            HostKeyStatus::Unknown
        );
        assert!(
            !tokio::fs::try_exists(&pins).await.unwrap_or(false),
            "trusting the system file must not copy it"
        );

        let _ = tokio::fs::remove_file(&system).await;
    }
}
