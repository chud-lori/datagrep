//! # datagrep-tunnel — in-process SSH tunnels
//!
//! A tunnel endpoint here is an in-process duplex stream, not a real
//! listening TCP port: nothing else on the machine can connect to it, so a
//! local process cannot ride the tunnel into the remote database. Auth goes
//! through the SSH agent or `~/.ssh/config`, and host keys are pinned on
//! first use with an explicit prompt if one ever changes.
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
//! ## Known limitations (see also inline doc comments)
//!
//! - **No separate `russh-keys` dependency.** As of the pinned `russh`
//!   version (0.62.5), key/agent handling lives in `russh::keys` inside the
//!   main crate; the standalone `russh-keys` crate is a stale pre-merge
//!   artifact on crates.io (last released 0.49.2, itself pinned to an
//!   older, incompatible `ssh-key`). See `Cargo.toml`.
//! - **`ProxyJump` is parsed-but-ignored**, not implemented. See the
//!   `ssh_config` module's doc comment for why.
//! - **Keepalive is off by default and stays off** — see the
//!   [`SshTunnel::connect`] doc comment for why a timer that ticks forever
//!   is the wrong default.
//! - **Agent auth only tries plain public-key identities**, not
//!   OpenSSH-certificate identities an agent might also offer. See
//!   `tunnel.rs`'s `authenticate_agent`.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(clippy::unwrap_used)]
// No unwrap outside tests; inside them a panic *is* the failure report.
// `cfg(test)` covers this crate's own `#[cfg(test)] mod tests` blocks under
// `cargo test`/`clippy --all-targets`.
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
