//! Error type for `datagrep-tunnel`.
//!
//! Security invariant, mirroring `datagrep-secrets`: no variant's
//! `Display`/`Debug` may ever contain secret material (passphrases,
//! passwords, private key bytes). Those fields are always
//! `datagrep_api::SecretString`, which redacts itself, or are simply not carried
//! into the error at all.

use std::path::PathBuf;

/// Everything that can go wrong opening or using an SSH tunnel.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    /// TCP connect, protocol negotiation, or key exchange with the SSH host
    /// failed. Carries the underlying `russh::Error`, which never contains
    /// credential material — it is transport/protocol-level (connection
    /// refused, bad version string, unsupported algorithm, disconnect,
    /// etc). `host`/`port` are attached by [`crate::SshTunnel::connect`]
    /// once the error surfaces (the low-level conversion site inside the
    /// handshake has no address context to attach).
    #[error("SSH connection to {host}:{port} failed: {source}")]
    Ssh {
        host: String,
        port: u16,
        #[source]
        source: russh::Error,
    },

    /// Every configured authentication method was rejected by the server.
    #[error("authentication to {user}@{host}:{port} failed: {remaining_methods}")]
    AuthFailed {
        user: String,
        host: String,
        port: u16,
        /// Server-advertised methods that could still succeed, joined with
        /// `,` (e.g. "publickey,password"). Never includes secret material.
        remaining_methods: String,
    },

    /// A private key file could not be read or decrypted. The passphrase
    /// itself never appears here even on failure.
    #[error("could not load key file {path}: {reason}")]
    KeyFile { path: PathBuf, reason: String },

    /// No `SSH_AUTH_SOCK` in the environment, or the agent socket could not
    /// be reached.
    #[error("SSH agent unavailable: {reason}")]
    AgentUnavailable { reason: String },

    /// The agent has no identity the server accepted.
    #[error("SSH agent at {socket} offered no usable identity")]
    NoAgentIdentity { socket: String },

    /// The host key differs from the one pinned in the `TofuStore` — the
    /// signature of a man-in-the-middle, so it is a hard error and is never
    /// auto-accepted. Both fingerprints are SHA256, in
    /// the same `SHA256:base64…` form `ssh-keygen -l` prints.
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

    /// The user (via the UI, through a [`crate::HostKeyDecision`]) declined
    /// an unknown host key.
    #[error("host key for {host}:{port} was not accepted")]
    HostKeyRejected { host: String, port: u16 },

    /// [`crate::TofuStore::check`] hit an `Unknown` key but nothing is
    /// listening on [`crate::TofuStore::decisions`] to answer it.
    #[error("unknown host key for {host}:{port}, but no UI is listening for a decision")]
    NoPromptListener { host: String, port: u16 },

    /// The known-hosts file on disk could not be read or written.
    #[error("known-hosts file {path}: {source}")]
    KnownHosts {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A known-hosts line didn't parse (`host:port base64-key`).
    #[error("malformed known-hosts entry at {path} line {line}: {reason}")]
    KnownHostsParse {
        path: PathBuf,
        line: usize,
        reason: String,
    },

    /// The server's offered host key could not be serialized to compare
    /// against the pinned entry. Not expected in practice — `ssh_key`
    /// rejects malformed keys earlier in the handshake — but handled rather
    /// than unwrapped.
    #[error("could not encode offered host key for {host}:{port}: {reason}")]
    HostKeyEncode {
        host: String,
        port: u16,
        reason: String,
    },

    /// Opening a `direct-tcpip` channel over an established session failed.
    #[error("could not open channel to {target_host}:{target_port}: {source}")]
    ChannelOpen {
        target_host: String,
        target_port: u16,
        #[source]
        source: russh::Error,
    },

    /// `~/.ssh/config` (or an explicit path) could not be parsed.
    #[error("could not parse ssh config {path}: {reason}")]
    SshConfigParse { path: PathBuf, reason: String },

    /// Generic I/O failure (reading `~/.ssh/config`, the known-hosts file,
    /// a key file's bytes before parsing, etc).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
