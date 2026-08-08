//! Live SSH integration tests — `#[ignore]`d, opt-in.
//!
//! These need a real SSH server and are never run by `cargo test` by
//! default (design brief: "live SSH tests behind `#[ignore]`"). Run
//! explicitly once the environment below is set:
//!
//! ```text
//! DATAGREP_TUNNEL_TEST_HOST=example.com \
//! DATAGREP_TUNNEL_TEST_PORT=22 \
//! DATAGREP_TUNNEL_TEST_USER=myuser \
//! cargo test -p datagrep-tunnel --test live_ssh -- --ignored --nocapture
//! ```
//!
//! Auth defaults to `Auth::Agent` (`SSH_AUTH_SOCK`). Set
//! `DATAGREP_TUNNEL_TEST_PASSWORD` to use password auth instead, or
//! `DATAGREP_TUNNEL_TEST_KEYFILE` (optionally with
//! `DATAGREP_TUNNEL_TEST_KEYFILE_PASSPHRASE`) for a key file.
//!
//! **Not run as part of this task's verification**: no SSH server was
//! reachable in the sandbox this crate was built in (no local `sshd`, no
//! passwordless `sudo` to enable one, no outbound test fixture provided).
//! These compile and are believed correct against the pinned `russh` API,
//! but have not been exercised against a live server — flag this when
//! reviewing.

use std::sync::Arc;

use datagrep_api::SecretString;
use datagrep_tunnel::{Auth, SshTunnel, TofuStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

fn test_auth() -> Auth {
    if let Some(password) = env("DATAGREP_TUNNEL_TEST_PASSWORD") {
        return Auth::Password(SecretString::new(password));
    }
    if let Some(path) = env("DATAGREP_TUNNEL_TEST_KEYFILE") {
        return Auth::KeyFile {
            path: path.into(),
            passphrase: env("DATAGREP_TUNNEL_TEST_KEYFILE_PASSPHRASE").map(SecretString::new),
        };
    }
    Auth::Agent
}

/// Connects, opens a `direct-tcpip` channel back to the server's own SSH
/// port (as seen from the server's loopback — always listening, since we
/// just used it to get in), and checks the bridged stream carries a real
/// `SSH-2.0-` banner. This exercises the whole path: handshake, TOFU accept
/// of a first-seen host key, auth, channel open, and the in-process
/// duplex bridge — against a real server, not a mock.
#[tokio::test]
#[ignore = "needs a real SSH server; see module docs for the env vars"]
async fn connect_and_open_channel_reads_a_real_ssh_banner() {
    let host = env("DATAGREP_TUNNEL_TEST_HOST").expect("set DATAGREP_TUNNEL_TEST_HOST");
    let port: u16 = env("DATAGREP_TUNNEL_TEST_PORT")
        .map(|p| p.parse().expect("DATAGREP_TUNNEL_TEST_PORT must be a u16"))
        .unwrap_or(22);
    let user = env("DATAGREP_TUNNEL_TEST_USER").expect("set DATAGREP_TUNNEL_TEST_USER");

    let known_hosts = std::env::temp_dir().join(format!(
        "datagrep-tunnel-live-test-known-hosts-{}",
        std::process::id()
    ));
    let (store, mut decisions) = TofuStore::open(&known_hosts).await.expect("open TofuStore");
    let store = Arc::new(store);

    // Auto-accept the first-seen key for this test run only — a real UI
    // would show `decision.fingerprint()` to the user instead.
    tokio::spawn(async move {
        while let Some(decision) = decisions.recv().await {
            decision.accept();
        }
    });

    let tunnel = SshTunnel::connect(host.clone(), port, user, test_auth(), store)
        .await
        .expect("connect + authenticate");

    let mut channel = tunnel
        .open_channel("127.0.0.1", port)
        .await
        .expect("open direct-tcpip channel back to the server's own sshd");

    let mut banner = [0u8; 4];
    channel
        .read_exact(&mut banner)
        .await
        .expect("read from the bridged channel");
    assert_eq!(
        &banner, b"SSH-",
        "expected an SSH banner over the bridged channel"
    );

    channel.shutdown().await.expect("shutdown the local end");

    let _ = tokio::fs::remove_file(&known_hosts).await;
}

/// A second connect to the same host should find the key already pinned
/// and need no prompt at all — proves TOFU persistence survives a fresh
/// `TofuStore::open` (a new process, in the real app).
#[tokio::test]
#[ignore = "needs a real SSH server; see module docs for the env vars"]
async fn reconnect_after_pinning_needs_no_prompt() {
    let host = env("DATAGREP_TUNNEL_TEST_HOST").expect("set DATAGREP_TUNNEL_TEST_HOST");
    let port: u16 = env("DATAGREP_TUNNEL_TEST_PORT")
        .map(|p| p.parse().expect("DATAGREP_TUNNEL_TEST_PORT must be a u16"))
        .unwrap_or(22);
    let user = env("DATAGREP_TUNNEL_TEST_USER").expect("set DATAGREP_TUNNEL_TEST_USER");

    let known_hosts = std::env::temp_dir().join(format!(
        "datagrep-tunnel-live-test-known-hosts-reconnect-{}",
        std::process::id()
    ));

    {
        let (store, mut decisions) = TofuStore::open(&known_hosts).await.expect("open TofuStore");
        tokio::spawn(async move {
            if let Some(decision) = decisions.recv().await {
                decision.accept();
            }
        });
        let _tunnel = SshTunnel::connect(
            host.clone(),
            port,
            user.clone(),
            test_auth(),
            Arc::new(store),
        )
        .await
        .expect("first connect pins the host key");
    }

    // Fresh store, same file: no prompt should fire this time. If one does,
    // the unbounded channel just buffers it — dropping `decisions` here
    // without ever calling `accept`/`reject` would make a genuinely-unknown
    // key fail loudly instead of silently, which is what we want to assert
    // implicitly by the connect below succeeding.
    let (store, _decisions) = TofuStore::open(&known_hosts)
        .await
        .expect("reopen TofuStore");
    let _tunnel = SshTunnel::connect(host, port, user, test_auth(), Arc::new(store))
        .await
        .expect("second connect should succeed with the pinned key and no prompt");

    let _ = tokio::fs::remove_file(&known_hosts).await;
}
