use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("SSH connection to {host}:{port} failed: {source}")]
    Ssh {
        host: String,
        port: u16,
        #[source]
        source: russh::Error,
    },

    #[error("authentication to {user}@{host}:{port} failed: {remaining_methods}")]
    AuthFailed {
        user: String,
        host: String,
        port: u16,
        remaining_methods: String,
    },

    #[error("could not load key file {path}: {reason}")]
    KeyFile { path: PathBuf, reason: String },

    #[error("SSH agent unavailable: {reason}")]
    AgentUnavailable { reason: String },

    #[error("SSH agent at {socket} offered no usable identity")]
    NoAgentIdentity { socket: String },

    #[error(
        "HOST KEY CHANGED for {host}:{port}\n  \
         pinned:  {expected_fingerprint}\n  \
         offered: {offered_fingerprint}\n\
         Someone could be eavesdropping, or the host key was legitimately \
         rotated — confirm out of band before trusting it."
    )]
    HostKeyChanged {
        host: String,
        port: u16,
        expected_fingerprint: String,
        offered_fingerprint: String,
    },

    #[error("host key for {host}:{port} was not accepted")]
    HostKeyRejected { host: String, port: u16 },

    #[error("unknown host key for {host}:{port}, but no UI is listening for a decision")]
    NoPromptListener { host: String, port: u16 },

    #[error("known-hosts file {path}: {source}")]
    KnownHosts {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed known-hosts entry at {path} line {line}: {reason}")]
    KnownHostsParse {
        path: PathBuf,
        line: usize,
        reason: String,
    },

    #[error("could not encode offered host key for {host}:{port}: {reason}")]
    HostKeyEncode {
        host: String,
        port: u16,
        reason: String,
    },

    #[error("could not open channel to {target_host}:{target_port}: {source}")]
    ChannelOpen {
        target_host: String,
        target_port: u16,
        #[source]
        source: russh::Error,
    },

    #[error("could not parse ssh config {path}: {reason}")]
    SshConfigParse { path: PathBuf, reason: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
