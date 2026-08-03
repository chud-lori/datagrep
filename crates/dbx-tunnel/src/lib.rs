//! # dbx-tunnel — in-process SSH tunnels
//!
//! Implements design §3.5 ("Connections": "SSH tunnel endpoint is an
//! in-process duplex stream, not a real listening TCP port — nothing on the
//! machine can hijack it. Agent auth, `~/.ssh/config` including
//! `ProxyJump`.") and §3.8's host-key pinning ("SSH host-key pinning with
//! an explicit change prompt").
//!
//! ## Shape
//!
//! - [`Auth`] — agent / key file / password.
//! - [`HostKeyPolicy`] / [`TofuStore`] / [`HostKeyDecision`] — trust-on-
//!   first-use host key pinning with a UI-confirmable unknown-host path and
//!   a hard error on a changed key.
//! - [`SshConfig`] — minimal `~/.ssh/config` reader (`Host`, `HostName`,
//!   `User`, `Port`, `IdentityFile`; globs; `ProxyJump` deferred, see the
//!   `ssh_config` module's doc comment).
//! - [`SshTunnel`] — one authenticated session; [`SshTunnel::open_channel`]
//!   returns an in-process [`tokio::io::DuplexStream`] bridged to a
//!   `direct-tcpip` channel.
//! - [`TunnelPool`] / [`Checkout`] — refcounted sharing of [`SshTunnel`]s
//!   keyed by `(host, port, user)`, torn down after the last checkout drops
//!   plus an idle grace period.
//!
//! ## Deviations from the original brief (see also inline doc comments)
//!
//! - **No separate `russh-keys` dependency.** As of the pinned `russh`
//!   version (0.62.5), key/agent handling lives in `russh::keys` inside the
//!   main crate; the standalone `russh-keys` crate is a stale pre-merge
//!   artifact on crates.io (last released 0.49.2, itself pinned to an
//!   older, incompatible `ssh-key`). See `Cargo.toml`.
//! - **`ProxyJump` is parsed-but-ignored**, not implemented. See the
//!   `ssh_config` module's doc comment for why.
//! - **Keepalive is off by default and stays off** — see
//!   [`SshTunnel::connect`] doc comment, citing design §5.1's no-polling
//!   rule.
//! - **Agent auth only tries plain public-key identities**, not
//!   OpenSSH-certificate identities an agent might also offer. See
//!   `tunnel.rs`'s `authenticate_agent`.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(clippy::unwrap_used)]
// Tests are allowed to unwrap freely (design brief: "No unwrap outside
// tests"); `cfg(test)` covers this crate's own `#[cfg(test)] mod tests`
// blocks under `cargo test`/`clippy --all-targets`.
#![cfg_attr(test, allow(clippy::unwrap_used))]

mod auth;
mod base64;
mod bridge;
mod error;
mod host_key;
mod pool;
mod ssh_config;
mod tunnel;

pub use auth::Auth;
pub use bridge::LocalEnd;
pub use error::TunnelError;
pub use host_key::{HostKeyDecision, HostKeyPolicy, TofuStore};
pub use pool::{Checkout, TunnelPool};
pub use ssh_config::{HostConfig, SshConfig};
pub use tunnel::SshTunnel;
