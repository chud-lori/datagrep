//! [`SshTunnel`] — an authenticated SSH session plus `direct-tcpip` channel
//! opening (design §3.5, §4 killer feature #5's SSH leg, §3.8).

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use datagrep_api::SecretString;
use russh::client::{self, AuthResult};
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::AgentIdentity;
use russh::keys::{self, PrivateKeyWithHashAlg, PublicKey};

use crate::auth::Auth;
use crate::bridge::{spawn_bridge, LocalEnd};
use crate::error::TunnelError;
use crate::host_key::HostKeyPolicy;

/// An authenticated SSH connection. Cheap to clone (an `Arc` around the
/// underlying `russh` session handle) — [`crate::TunnelPool`] hands out
/// clones and lets the last one dropping tear the connection down.
///
/// `direct-tcpip` channels are opened on demand; the *transport* handed to
/// a driver is never this tunnel itself, always the
/// [`LocalEnd`][crate::bridge::LocalEnd] returned by
/// [`open_channel`](Self::open_channel) (design §3.5: "not a real listening
/// TCP port").
pub struct SshTunnel<P: HostKeyPolicy + 'static> {
    handle: Arc<client::Handle<TunnelHandler<P>>>,
    host: String,
    port: u16,
}

// Manual `Clone`: `#[derive(Clone)]` would require `P: Clone`, but we only
// ever need to clone the `Arc`.
impl<P: HostKeyPolicy + 'static> Clone for SshTunnel<P> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            host: self.host.clone(),
            port: self.port,
        }
    }
}

impl<P: HostKeyPolicy + 'static> std::fmt::Debug for SshTunnel<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTunnel")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl<P: HostKeyPolicy + 'static> SshTunnel<P> {
    /// Dial `host:port`, verify its host key against `policy` (design
    /// §3.8), and authenticate as `user` via `auth`.
    ///
    /// **Keepalive: none, by default — and this is deliberate, not an
    /// oversight.** `russh::client::Config::keepalive_interval` defaults to
    /// `None` and we don't override it: a periodic keepalive is a timer
    /// that fires forever whether or not anyone is using the tunnel. Design
    /// §5.1's no-polling rule is explicit about the cost of exactly this
    /// pattern ("a 30 s ping across 5 connections is 10 wakeups/min
    /// forever, for nothing"). Instead, death is detected lazily: the next
    /// `open_channel` (or a read/write on an already-open channel) fails,
    /// and the caller (ultimately `TunnelPool`) reconnects then. A dead
    /// idle tunnel costs nothing until something tries to use it.
    pub async fn connect(
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        auth: Auth,
        policy: Arc<P>,
    ) -> Result<Self, TunnelError> {
        let host = host.into();
        let user = user.into();
        let config = Arc::new(client::Config::default());
        let handler = TunnelHandler {
            host: host.clone(),
            port,
            policy,
        };

        let mut handle = client::connect(config, (host.as_str(), port), handler)
            .await
            .map_err(|e| e.into_tunnel_error(&host, port))?;

        authenticate(&mut handle, &user, auth, &host, port).await?;

        Ok(Self {
            handle: Arc::new(handle),
            host,
            port,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Open a `direct-tcpip` channel to `target_host:target_port` *through*
    /// this SSH connection, bridged to an in-process duplex stream (design
    /// §3.5). The returned stream is what a driver's `ConnectCtx::transport`
    /// should be — nothing on the local machine can connect to it, because
    /// nothing is listening; it is not a socket at all.
    pub async fn open_channel(
        &self,
        target_host: impl Into<String>,
        target_port: u16,
    ) -> Result<LocalEnd, TunnelError> {
        let target_host = target_host.into();
        // Originator address/port are advisory (RFC 4254 §7) and, because
        // there is no real originating socket in an in-process bridge,
        // filled with the conventional loopback placeholder.
        let channel = self
            .handle
            .channel_open_direct_tcpip(target_host.clone(), target_port as u32, "127.0.0.1", 0)
            .await
            .map_err(|source| TunnelError::ChannelOpen {
                target_host,
                target_port,
                source,
            })?;
        Ok(spawn_bridge(channel.into_stream()))
    }
}

async fn authenticate<P: HostKeyPolicy + 'static>(
    handle: &mut client::Handle<TunnelHandler<P>>,
    user: &str,
    auth: Auth,
    host: &str,
    port: u16,
) -> Result<(), TunnelError> {
    let result = match auth {
        Auth::Password(secret) => handle
            .authenticate_password(user, secret.expose())
            .await
            .map_err(|source| TunnelError::Ssh {
                host: host.to_owned(),
                port,
                source,
            })?,
        Auth::KeyFile { path, passphrase } => {
            authenticate_keyfile(handle, user, path, passphrase, host, port).await?
        }
        Auth::Agent => return authenticate_agent(handle, user).await,
    };
    finish_auth(result, user, host, port)
}

#[allow(clippy::too_many_arguments)]
async fn authenticate_keyfile<P: HostKeyPolicy + 'static>(
    handle: &mut client::Handle<TunnelHandler<P>>,
    user: &str,
    path: PathBuf,
    passphrase: Option<SecretString>,
    host: &str,
    port: u16,
) -> Result<AuthResult, TunnelError> {
    let password = passphrase.as_ref().map(SecretString::expose);
    // Blocking file read + key decryption; key files are small (a few KB)
    // and this runs once per connect, not on any hot path — unlike keychain
    // access (datagrep-secrets §3.8) it doesn't warrant `spawn_blocking`.
    let key = keys::load_secret_key(&path, password).map_err(|source| TunnelError::KeyFile {
        path: path.clone(),
        reason: source.to_string(),
    })?;
    // `hash_alg: None` maps RSA keys to the legacy `ssh-rsa` (SHA-1)
    // signature scheme rather than negotiating `rsa-sha2-256/512` via
    // `Handle::best_supported_rsa_hash`. Deviation, deliberately minor: it
    // only affects RSA keys against very old servers that lack SHA-2
    // support, and ed25519/ecdsa (the keys we recommend) ignore this field
    // entirely (see `PrivateKeyWithHashAlg::new`).
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
    handle
        .authenticate_publickey(user, key)
        .await
        .map_err(|source| TunnelError::Ssh {
            host: host.to_owned(),
            port,
            source,
        })
}

async fn authenticate_agent<P: HostKeyPolicy + 'static>(
    handle: &mut client::Handle<TunnelHandler<P>>,
    user: &str,
) -> Result<(), TunnelError> {
    let socket = std::env::var("SSH_AUTH_SOCK").unwrap_or_default();
    let mut agent =
        AgentClient::connect_env()
            .await
            .map_err(|source| TunnelError::AgentUnavailable {
                reason: source.to_string(),
            })?;
    let identities =
        agent
            .request_identities()
            .await
            .map_err(|source| TunnelError::AgentUnavailable {
                reason: source.to_string(),
            })?;

    for identity in identities {
        // Only plain public-key identities are tried; OpenSSH-certificate
        // identities need `authenticate_certificate_with`, a separate call
        // this first pass doesn't wire up. Deviation, documented: an agent
        // offering only certificates falls through to `NoAgentIdentity`
        // below rather than being tried.
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        match handle
            .authenticate_publickey_with(user, key, None, &mut agent)
            .await
        {
            Ok(AuthResult::Success) => return Ok(()),
            Ok(AuthResult::Failure { .. }) => continue,
            Err(source) => {
                // Not secret: agent protocol/signing errors, never key
                // material. Try the next identity rather than aborting —
                // an agent commonly holds several keys and only one may be
                // authorized for this host.
                tracing::debug!(error = %source, "agent identity rejected, trying next");
                continue;
            }
        }
    }
    Err(TunnelError::NoAgentIdentity { socket })
}

fn finish_auth(result: AuthResult, user: &str, host: &str, port: u16) -> Result<(), TunnelError> {
    match result {
        AuthResult::Success => Ok(()),
        AuthResult::Failure {
            remaining_methods, ..
        } => Err(TunnelError::AuthFailed {
            user: user.to_owned(),
            host: host.to_owned(),
            port,
            remaining_methods: remaining_methods
                .iter()
                .map(<&str>::from)
                .collect::<Vec<_>>()
                .join(","),
        }),
    }
}

/// The `russh::client::Handler` backing every [`SshTunnel`]. Not public:
/// callers interact with host-key decisions through [`HostKeyPolicy`] and
/// [`crate::HostKeyDecision`], never this type directly.
struct TunnelHandler<P: HostKeyPolicy + 'static> {
    host: String,
    port: u16,
    policy: Arc<P>,
}

/// `client::Handler::Error`. Wraps a [`TunnelError`] so `check_server_key`
/// can return our own rich errors (host-key changed/rejected, both carrying
/// their own host/port) while still satisfying `From<russh::Error>` for
/// the `?` the library uses internally during the handshake.
#[derive(Debug)]
struct HandshakeError(TunnelError);

impl From<russh::Error> for HandshakeError {
    fn from(source: russh::Error) -> Self {
        // No address context available at this conversion site (it's a
        // blanket `From`, called deep inside russh's handshake internals);
        // `into_tunnel_error` fills it in once the error reaches
        // `SshTunnel::connect`, which does know the dialed address.
        HandshakeError(TunnelError::Ssh {
            host: String::new(),
            port: 0,
            source,
        })
    }
}

impl HandshakeError {
    fn into_tunnel_error(self, host: &str, port: u16) -> TunnelError {
        match self.0 {
            TunnelError::Ssh { source, .. } => TunnelError::Ssh {
                host: host.to_owned(),
                port,
                source,
            },
            // Host-key errors already carry their own (correct) host/port
            // from `HostKeyPolicy::check`; auth errors don't reach here
            // (auth happens after `connect` returns, via `Handle` methods
            // that return `russh::Error` directly, not `H::Error`).
            other => other,
        }
    }
}

impl<P: HostKeyPolicy + 'static> client::Handler for TunnelHandler<P> {
    type Error = HandshakeError;

    fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        let host = self.host.clone();
        let port = self.port;
        let policy = self.policy.clone();
        let key = server_public_key.clone();
        async move {
            match policy.check(&host, port, &key).await {
                Ok(()) => Ok(true),
                Err(e) => Err(HandshakeError(e)),
            }
        }
    }
}
