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

pub struct SshTunnel<P: HostKeyPolicy + 'static> {
    handle: Arc<client::Handle<TunnelHandler<P>>>,
    host: String,
    port: u16,
}

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

    pub async fn open_channel(
        &self,
        target_host: impl Into<String>,
        target_port: u16,
    ) -> Result<LocalEnd, TunnelError> {
        let target_host = target_host.into();
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
    let key = keys::load_secret_key(&path, password).map_err(|source| TunnelError::KeyFile {
        path: path.clone(),
        reason: source.to_string(),
    })?;
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

struct TunnelHandler<P: HostKeyPolicy + 'static> {
    host: String,
    port: u16,
    policy: Arc<P>,
}

#[derive(Debug)]
struct HandshakeError(TunnelError);

impl From<russh::Error> for HandshakeError {
    fn from(source: russh::Error) -> Self {
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
