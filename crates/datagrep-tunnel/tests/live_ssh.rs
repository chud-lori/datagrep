// Needs a real SSH server: set DATAGREP_TUNNEL_TEST_{HOST,PORT,USER} (auth vars in test_auth) and run with --ignored.
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

    let (store, _decisions) = TofuStore::open(&known_hosts)
        .await
        .expect("reopen TofuStore");
    let _tunnel = SshTunnel::connect(host, port, user, test_auth(), Arc::new(store))
        .await
        .expect("second connect should succeed with the pinned key and no prompt");

    let _ = tokio::fs::remove_file(&known_hosts).await;
}
