//! [`SecretResolver`] — turns a [`SecretRef`] into a live
//! [`SecretString`], and writes/deletes keychain-backed refs.
//!
//! Called once per pool creation (design §3.5/§3.8: "fetched once at pool
//! creation on a blocking thread") — resolution is not a hot path, so every
//! choice here favors safety over speed.

use std::process::Stdio;
use std::time::Duration;

use dbx_api::SecretString;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::{wipe, SecretError, SecretRef};

/// Longest stderr excerpt carried in an [`SecretError::ExecFailed`]. Enough to
/// diagnose `op`/`aws` failures without dragging a novel into the error chain.
const MAX_STDERR: usize = 1024;

/// Default `exec:` timeout. Cloud credential helpers (SSO re-auth, MFA push)
/// can legitimately take many seconds; 30 s bounds a hung helper without
/// strangling a slow one.
const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolves [`SecretRef`]s to [`SecretString`]s (design §4, killer feature #5).
///
/// All methods are async; keychain work is shipped to the blocking pool
/// because Secret Service is DBus IPC — tens to hundreds of ms — and must
/// never stall an async worker (design §3.4, §3.8).
#[derive(Debug, Clone)]
pub struct SecretResolver {
    exec_timeout: Duration,
}

impl Default for SecretResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretResolver {
    /// Resolver with the default 30 s `exec:` timeout.
    pub fn new() -> Self {
        Self {
            exec_timeout: DEFAULT_EXEC_TIMEOUT,
        }
    }

    /// Override the `exec:` timeout (tests use ~1 s; UIs may shorten it).
    pub fn with_exec_timeout(exec_timeout: Duration) -> Self {
        Self { exec_timeout }
    }

    /// Resolve `reference` to its secret value.
    ///
    /// - `keychain:` → keyring crate, on the blocking pool.
    /// - `env:` → process environment.
    /// - `exec:` → `sh -c <command>`, trimmed stdout, bounded by the timeout.
    /// - `prompt:` → always `Err(NeedsPrompt)`; the UI owns prompting.
    pub async fn resolve(&self, reference: &SecretRef) -> Result<SecretString, SecretError> {
        // Refs are safe to log (they are what profiles commit to git);
        // resolved values never are, and never appear below.
        tracing::debug!(scheme = reference.scheme(), "resolving secret ref");
        match reference {
            SecretRef::Keychain { service, account } => {
                let entry = keychain_entry(service, account)?;
                let (service, account) = (service.clone(), account.clone());
                // Design §3.8: Secret Service is DBus IPC — blocking thread only.
                tokio::task::spawn_blocking(move || match entry.get_password() {
                    Ok(password) => Ok(SecretString::new(password)),
                    Err(source) => Err(SecretError::Keychain {
                        service,
                        account,
                        source,
                    }),
                })
                .await
                .map_err(|join| SecretError::ExecSpawn {
                    source: std::io::Error::other(join),
                })?
            }
            SecretRef::Env { var } => match std::env::var(var) {
                Ok(value) => Ok(SecretString::new(value)),
                Err(std::env::VarError::NotPresent) => Err(SecretError::Env {
                    var: var.clone(),
                    problem: "not set",
                }),
                Err(std::env::VarError::NotUnicode(_)) => Err(SecretError::Env {
                    var: var.clone(),
                    problem: "not valid unicode",
                }),
            },
            SecretRef::Exec { command } => self.resolve_exec(command).await,
            SecretRef::Prompt => Err(SecretError::NeedsPrompt {
                reference: reference.to_string(),
            }),
        }
    }

    /// Store `secret` at a **keychain** ref. Env/exec/prompt refs are
    /// read-only sources and return [`SecretError::ReadOnly`].
    pub async fn store(
        &self,
        reference: &SecretRef,
        secret: SecretString,
    ) -> Result<(), SecretError> {
        match reference {
            SecretRef::Keychain { service, account } => {
                let entry = keychain_entry(service, account)?;
                let (service, account) = (service.clone(), account.clone());
                tokio::task::spawn_blocking(move || {
                    // `secret` moved onto the blocking thread; zeroized on drop.
                    entry
                        .set_password(secret.expose())
                        .map_err(|source| SecretError::Keychain {
                            service,
                            account,
                            source,
                        })
                })
                .await
                .map_err(|join| SecretError::ExecSpawn {
                    source: std::io::Error::other(join),
                })?
            }
            other => Err(SecretError::ReadOnly {
                reference: other.to_string(),
                operation: "store",
            }),
        }
    }

    /// Delete a **keychain** ref's stored secret. Read-only refs error.
    pub async fn delete(&self, reference: &SecretRef) -> Result<(), SecretError> {
        match reference {
            SecretRef::Keychain { service, account } => {
                let entry = keychain_entry(service, account)?;
                let (service, account) = (service.clone(), account.clone());
                tokio::task::spawn_blocking(move || {
                    entry
                        .delete_credential()
                        .map_err(|source| SecretError::Keychain {
                            service,
                            account,
                            source,
                        })
                })
                .await
                .map_err(|join| SecretError::ExecSpawn {
                    source: std::io::Error::other(join),
                })?
            }
            other => Err(SecretError::ReadOnly {
                reference: other.to_string(),
                operation: "delete",
            }),
        }
    }

    /// Run `sh -c <command>`; trimmed stdout is the secret.
    ///
    /// SECURITY (design §3.8): the command line is never placed in an error or
    /// log, so it can never appear alongside its output; stdout is either the
    /// returned `SecretString` or wiped — it is never part of any error.
    async fn resolve_exec(&self, command: &str) -> Result<SecretString, SecretError> {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If we time out and drop the child, the process is killed rather
            // than left running with a secret on its stdout pipe.
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| SecretError::ExecSpawn { source })?;

        // Drive stdout/stderr reads concurrently with waiting, all under one
        // timeout. `wait_with_output` is avoided so we control the buffers.
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let run = async {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let (status, out_res, err_res) = tokio::join!(
                child.wait(),
                async {
                    match stdout_pipe.as_mut() {
                        Some(p) => p.read_to_end(&mut stdout).await.map(|_| ()),
                        None => Ok(()),
                    }
                },
                async {
                    match stderr_pipe.as_mut() {
                        Some(p) => p.read_to_end(&mut stderr).await.map(|_| ()),
                        None => Ok(()),
                    }
                },
            );
            let status = status.map_err(|source| SecretError::ExecSpawn { source })?;
            out_res.map_err(|source| SecretError::ExecSpawn { source })?;
            err_res.map_err(|source| SecretError::ExecSpawn { source })?;
            Ok::<_, SecretError>((status, stdout, stderr))
        };

        let (status, stdout, stderr) = match tokio::time::timeout(self.exec_timeout, run).await {
            Ok(result) => result?,
            Err(_elapsed) => {
                // kill_on_drop reaps the child; report only the timeout.
                return Err(SecretError::ExecTimeout {
                    timeout: self.exec_timeout,
                });
            }
        };

        if !status.success() {
            // stdout may hold a partial secret — wipe it, never surface it.
            drop(wipe_bytes(stdout));
            let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
            if stderr.len() > MAX_STDERR {
                stderr.truncate(floor_char_boundary(&stderr, MAX_STDERR));
                stderr.push('…');
            }
            return Err(SecretError::ExecFailed {
                status: status.to_string(),
                stderr: stderr.trim_end().to_owned(),
            });
        }

        let mut full = match String::from_utf8(stdout) {
            Ok(s) => s,
            Err(err) => {
                drop(wipe_bytes(err.into_bytes()));
                return Err(SecretError::ExecNotUtf8);
            }
        };
        let trimmed = full.trim();
        if trimmed.is_empty() {
            wipe(&mut full);
            return Err(SecretError::ExecEmpty);
        }
        let secret = SecretString::new(trimmed.to_owned());
        // The untrimmed buffer also held the secret — wipe before drop.
        wipe(&mut full);
        Ok(secret)
    }
}

fn keychain_entry(service: &str, account: &str) -> Result<keyring::Entry, SecretError> {
    keyring::Entry::new(service, account).map_err(|source| SecretError::Keychain {
        service: service.to_owned(),
        account: account.to_owned(),
        source,
    })
}

/// Volatile-wipe a byte buffer (stdout that may contain secret material).
fn wipe_bytes(mut bytes: Vec<u8>) -> Vec<u8> {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference into the Vec.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    bytes
}

/// Largest byte index `<= max` that is a char boundary (stable-Rust stand-in
/// for `str::floor_char_boundary`).
fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretRef;

    fn resolver() -> SecretResolver {
        SecretResolver::new()
    }

    // --- env: round-trip -----------------------------------------------

    #[tokio::test]
    async fn env_round_trip() {
        // SAFETY: test-only env mutation; no other test in this crate reads
        // this var, and tests in this module run single-threaded by default
        // under `cargo test` per-crate (no `#[tokio::test(flavor = ...)]`
        // parallel env sharing concern beyond the usual std::env caveats).
        unsafe { std::env::set_var("DBX_SECRETS_TEST_ENV_ROUNDTRIP", "s3cret-value") };
        let r: SecretRef = "env:DBX_SECRETS_TEST_ENV_ROUNDTRIP".parse().unwrap();
        let got = resolver().resolve(&r).await.unwrap();
        assert_eq!(got.expose(), "s3cret-value");
        unsafe { std::env::remove_var("DBX_SECRETS_TEST_ENV_ROUNDTRIP") };
    }

    #[tokio::test]
    async fn env_missing_var_errors() {
        unsafe { std::env::remove_var("DBX_SECRETS_TEST_ENV_MISSING") };
        let r: SecretRef = "env:DBX_SECRETS_TEST_ENV_MISSING".parse().unwrap();
        let err = resolver().resolve(&r).await.unwrap_err();
        assert!(matches!(
            err,
            SecretError::Env {
                problem: "not set",
                ..
            }
        ));
    }

    // --- exec: happy path -------------------------------------------------

    #[tokio::test]
    async fn exec_echo_resolves_trimmed_stdout() {
        let r: SecretRef = "exec:echo '  hunter2  '".parse().unwrap();
        let got = resolver().resolve(&r).await.unwrap();
        assert_eq!(got.expose(), "hunter2");
    }

    #[tokio::test]
    async fn exec_empty_output_errors() {
        let r: SecretRef = "exec:printf ''".parse().unwrap();
        let err = resolver().resolve(&r).await.unwrap_err();
        assert!(matches!(err, SecretError::ExecEmpty));
    }

    // --- exec: timeout ------------------------------------------------

    #[tokio::test]
    async fn exec_timeout_kills_slow_command() {
        let short = SecretResolver::with_exec_timeout(Duration::from_secs(1));
        let r: SecretRef = "exec:sleep 60".parse().unwrap();
        let start = std::time::Instant::now();
        let err = short.resolve(&r).await.unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "should have been killed near the 1s timeout, took {:?}",
            start.elapsed()
        );
        match err {
            SecretError::ExecTimeout { timeout } => assert_eq!(timeout, Duration::from_secs(1)),
            other => panic!("expected ExecTimeout, got {other:?}"),
        }
    }

    // --- exec: failure captures stderr, never stdout/secret -----------

    #[tokio::test]
    async fn exec_failure_captures_stderr_only() {
        let r: SecretRef = "exec:echo 'top-secret-stdout'; echo 'diagnostic-stderr' 1>&2; exit 3"
            .parse()
            .unwrap();
        let err = resolver().resolve(&r).await.unwrap_err();
        match &err {
            SecretError::ExecFailed { status, stderr } => {
                assert!(stderr.contains("diagnostic-stderr"));
                assert!(status.contains('3'));
            }
            other => panic!("expected ExecFailed, got {other:?}"),
        }
        // SECURITY: the Display of the error must never contain the
        // command's stdout (the secret channel) nor the command line itself.
        let rendered = err.to_string();
        assert!(!rendered.contains("top-secret-stdout"));
        assert!(!rendered.contains("echo"));
    }

    #[tokio::test]
    async fn exec_spawn_failure_is_reported() {
        let r: SecretRef = "exec:/definitely/not/a/real/binary --flag".parse().unwrap();
        // `sh -c` itself spawns fine; the *inner* command fails to exec,
        // which sh reports via a non-zero exit and stderr, not ExecSpawn.
        let err = resolver().resolve(&r).await.unwrap_err();
        assert!(matches!(err, SecretError::ExecFailed { .. }));
    }

    // --- prompt: never resolves here -----------------------------------

    #[tokio::test]
    async fn prompt_always_needs_prompt() {
        let r: SecretRef = "prompt:".parse().unwrap();
        let err = resolver().resolve(&r).await.unwrap_err();
        assert!(matches!(err, SecretError::NeedsPrompt { reference } if reference == "prompt:"));
    }

    // --- read-only sources reject store/delete -------------------------

    #[tokio::test]
    async fn env_exec_prompt_are_read_only() {
        use dbx_api::SecretString;

        for reference in [
            "env:SOME_VAR".parse::<SecretRef>().unwrap(),
            "exec:true".parse::<SecretRef>().unwrap(),
            "prompt:".parse::<SecretRef>().unwrap(),
        ] {
            let err = resolver()
                .store(&reference, SecretString::new("x".into()))
                .await
                .unwrap_err();
            assert!(matches!(err, SecretError::ReadOnly { .. }));

            let err = resolver().delete(&reference).await.unwrap_err();
            assert!(matches!(err, SecretError::ReadOnly { .. }));
        }
    }

    // --- keychain: live round-trip, opt-in only -------------------------
    //
    // Talks to the real OS credential store (Keychain on macOS, Secret
    // Service on Linux, Credential Manager on Windows). `#[ignore]`d so
    // normal `cargo test` runs stay hermetic; run explicitly with
    // `cargo test -- --ignored keychain_live_round_trip`.
    #[tokio::test]
    #[ignore = "touches the real OS keychain; run explicitly"]
    async fn keychain_live_round_trip() {
        use dbx_api::SecretString;

        let r: SecretRef = "keychain:dbx-secrets-test:ci-round-trip".parse().unwrap();
        let res = resolver();

        res.store(&r, SecretString::new("round-trip-value".into()))
            .await
            .unwrap();
        let got = res.resolve(&r).await.unwrap();
        assert_eq!(got.expose(), "round-trip-value");
        res.delete(&r).await.unwrap();

        let err = res.resolve(&r).await.unwrap_err();
        assert!(matches!(err, SecretError::Keychain { .. }));
    }
}
