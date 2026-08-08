//! # datagrep-secrets — secret references and resolution
//!
//! Implements design §3.8 ("Secrets") and killer feature #5 (§4): connection
//! profiles are plain-text, git-committable TOML that never contain a secret —
//! each secret field stores only a **reference** ([`SecretRef`]), and the value
//! is resolved just-in-time at pool creation.
//!
//! Reference grammar (one string, stored in the profile):
//!
//! | Form | Meaning |
//! |---|---|
//! | `keychain:<service>:<account>` | OS keychain via the `keyring` crate |
//! | `env:<VAR>` | process environment variable |
//! | `exec:<command line>` | run via `sh -c`, trimmed stdout is the secret (covers `op read`, `aws rds generate-db-auth-token`, …) |
//! | `prompt:` | ask the user — resolution returns [`SecretError::NeedsPrompt`]; the UI owns the dialog, this crate never touches a TTY |
//!
//! Ground rules encoded here (design §3.8):
//! - Every resolved value is a [`datagrep_api::SecretString`] — zeroized on drop,
//!   redacted from `Debug`.
//! - **No secret material ever reaches a log line or an error `Display`.**
//!   Exec failures capture stderr, never stdout, and never echo the command
//!   next to any output.
//! - Keychain access happens on the blocking pool: Secret Service is DBus IPC
//!   (tens to hundreds of ms) and must never run on an async worker (§3.4).

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

mod error;
mod reference;
mod resolver;

pub use error::SecretError;
pub use reference::SecretRef;
pub use resolver::SecretResolver;

/// Overwrite a temporary string's bytes with zeros via volatile writes so the
/// compiler cannot elide the wipe. Mirrors `datagrep_api::SecretString`'s drop
/// behavior for intermediates that are not (yet) wrapped in a `SecretString`
/// (e.g. the full stdout of an `exec:` resolver before trimming).
pub(crate) fn wipe(s: &mut str) {
    // SAFETY: writing 0x00 keeps the buffer valid UTF-8 (NUL is a one-byte
    // scalar value), so the String invariants hold.
    let bytes = unsafe { s.as_bytes_mut() };
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference into the buffer.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::wipe;

    #[test]
    fn wipe_zeroes_bytes() {
        let mut s = String::from("swordfish");
        wipe(&mut s);
        assert!(s.as_bytes().iter().all(|&b| b == 0));
        assert_eq!(s.len(), 9);
    }
}
