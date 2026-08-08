//! [`Auth`] — how [`crate::SshTunnel::connect`] proves identity to the SSH
//! server (design §3.5: "Agent auth, `~/.ssh/config` including `ProxyJump`."
//! — `ProxyJump` is deferred, see crate root docs).

use std::path::PathBuf;

use datagrep_api::SecretString;

/// Authentication method for an SSH connection.
///
/// `Debug` is safe to log: [`SecretString`] redacts itself, and no other
/// variant carries secret material (a key *file path* is not a secret; its
/// decrypted bytes never appear in this enum at all — they're loaded and
/// consumed inside [`crate::SshTunnel::connect`]).
#[derive(Debug)]
pub enum Auth {
    /// Delegate signing to a running `ssh-agent`, reached via `SSH_AUTH_SOCK`.
    /// Tries every identity the agent offers until one is accepted.
    Agent,
    /// A private key file on disk, optionally passphrase-protected.
    KeyFile {
        path: PathBuf,
        passphrase: Option<SecretString>,
    },
    /// Plain password authentication.
    Password(SecretString),
}
