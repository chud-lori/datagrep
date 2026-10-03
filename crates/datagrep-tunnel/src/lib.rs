#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(clippy::unwrap_used)]
#![cfg_attr(test, allow(clippy::unwrap_used))]

mod auth;
mod base64;
mod bridge;
mod error;
mod forward;
mod host_key;
mod pool;
mod ssh_config;
mod tunnel;

pub use auth::Auth;
pub use bridge::LocalEnd;
pub use error::TunnelError;
pub use forward::{Connector, LocalForward};
pub use host_key::{fingerprint, HostKeyDecision, HostKeyPolicy, HostKeyStatus, TofuStore};
pub use pool::{Checkout, TunnelPool};
pub use russh::keys::PublicKey;
pub use ssh_config::{HostConfig, SshConfig};
pub use tunnel::{probe_host_key, SshTunnel};
