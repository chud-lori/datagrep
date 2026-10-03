use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value as Json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Notify};

use datagrep_api::error::DbError;

use crate::frame::{read_frame, write_frame, MAX_FRAME};
use crate::wire::{HelloReply, Incoming, Outgoing, PROTOCOL_MAX, PROTOCOL_MIN};

pub(crate) const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(2);
const TAIL_BYTES: usize = 64 * 1024;
const TAIL_IN_ERRORS: usize = 1024;
const LOG_LINES_PER_SEC: u32 = 50;
const MASK: &str = "••••";

const ENV_ALLOWLIST: &[&str] = &[
    "HOME",
    "TMPDIR",
    "LANG",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

type Reply = Result<(Json, usize), DbError>;

struct Pending {
    conn: Option<u64>,
    tx: oneshot::Sender<Reply>,
}

// An abandoned request stays pending until the sidecar answers it: that is what "in flight" means.
struct State {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    dead: Option<String>,
}

struct Shared {
    engine: &'static str,
    state: Mutex<State>,
    secrets: Vec<String>,
    tail: Mutex<String>,
    kill: Notify,
    settled: Notify,
}

pub(crate) struct Process {
    shared: Arc<Shared>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    hello: HelloReply,
    next_conn: AtomicU64,
}

#[derive(Debug, Clone)]
pub(crate) struct Spawn {
    pub engine: &'static str,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub extra_env: &'static [&'static str],
    pub secrets: Vec<String>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in self.secrets.iter().filter(|s| !s.is_empty()) {
            out = out.replace(secret.as_str(), MASK);
        }
        out
    }

    fn tail_excerpt(&self) -> String {
        let tail = lock(&self.tail);
        let mut start = tail.len().saturating_sub(TAIL_IN_ERRORS);
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail[start..].trim().to_string()
    }

    fn fail_all(&self, reason: String) {
        let pending = {
            let mut state = lock(&self.state);
            if state.dead.is_none() {
                state.dead = Some(reason.clone());
            }
            std::mem::take(&mut state.pending)
        };
        for (_, p) in pending {
            let _ = p.tx.send(Err(DbError::Protocol(reason.clone())));
        }
        self.settled.notify_waiters();
    }

    fn dead_error(&self) -> DbError {
        let reason = lock(&self.state)
            .dead
            .clone()
            .unwrap_or_else(|| format!("the {} sidecar went away", self.engine));
        DbError::Protocol(reason)
    }
}

impl Process {
    pub(crate) async fn spawn(spec: Spawn) -> Result<Arc<Process>, DbError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .env_clear()
            .envs(child_env(spec.extra_env))
            .env("GOMEMLIMIT", "256MiB")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        // SAFETY: the closure only makes async-signal-safe syscalls on plain stack values.
        unsafe {
            cmd.pre_exec(harden_child);
        }
        let mut child = cmd.spawn().map_err(|e| {
            DbError::Connect(format!(
                "could not start the {} engine ({}): {e}",
                spec.engine,
                spec.program.display()
            ))
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(DbError::Connect("sidecar pipes were not created".into()));
        };

        let shared = Arc::new(Shared {
            engine: spec.engine,
            state: Mutex::new(State {
                next_id: 1,
                pending: HashMap::new(),
                dead: None,
            }),
            secrets: spec.secrets,
            tail: Mutex::new(String::new()),
            kill: Notify::new(),
            settled: Notify::new(),
        });
        let stderr_task = tokio::spawn(drain_stderr(stderr, shared.clone()));
        tokio::spawn(supervise(child, stdout, stderr_task, shared.clone()));

        let mut process = Process {
            shared,
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            hello: HelloReply {
                protocol: 0,
                engine: String::new(),
                engine_version: String::new(),
                language: Json::Null,
                caps: 0,
            },
            next_conn: AtomicU64::new(1),
        };
        let hello = tokio::time::timeout(
            HELLO_TIMEOUT,
            process.call(
                None,
                "hello",
                json!({
                    "protocol": [PROTOCOL_MIN, PROTOCOL_MAX],
                    "app_version": env!("CARGO_PKG_VERSION"),
                    "codec": "json",
                }),
            ),
        )
        .await;
        let hello = match hello {
            Ok(Ok((reply, _))) => match serde_json::from_value::<HelloReply>(reply) {
                Ok(hello) => hello,
                Err(e) => {
                    process.kill();
                    return Err(DbError::Protocol(format!("bad hello reply: {e}")));
                }
            },
            Ok(Err(e)) => {
                process.kill();
                return Err(DbError::Connect(format!(
                    "{} engine sidecar did not start: {e}",
                    spec.engine
                )));
            }
            Err(_) => {
                process.kill();
                return Err(DbError::Connect(format!(
                    "{} engine sidecar did not answer within {}s: {}",
                    spec.engine,
                    HELLO_TIMEOUT.as_secs(),
                    process.shared.tail_excerpt()
                )));
            }
        };
        if !(PROTOCOL_MIN..=PROTOCOL_MAX).contains(&hello.protocol) {
            process.kill();
            return Err(DbError::Protocol(format!(
                "sidecar speaks protocol {}, this build speaks {PROTOCOL_MIN}..={PROTOCOL_MAX}",
                hello.protocol
            )));
        }
        process.hello = hello;
        Ok(Arc::new(process))
    }

    pub(crate) fn hello(&self) -> &HelloReply {
        &self.hello
    }

    pub(crate) fn next_conn(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn is_alive(&self) -> bool {
        lock(&self.shared.state).dead.is_none()
    }

    pub(crate) fn kill(&self) {
        self.shared.kill.notify_one();
    }

    pub(crate) async fn call(&self, conn: Option<u64>, method: &str, params: Json) -> Reply {
        let (id, rx) = {
            let mut state = lock(&self.shared.state);
            if state.dead.is_some() {
                drop(state);
                return Err(self.shared.dead_error());
            }
            let id = state.next_id;
            state.next_id += 1;
            let (tx, rx) = oneshot::channel();
            state.pending.insert(id, Pending { conn, tx });
            (id, rx)
        };
        self.send(Some(id), method, params).await?;
        rx.await.unwrap_or_else(|_| Err(self.shared.dead_error()))
    }

    // Resolves once nothing sent for `conn` is still waiting on the sidecar, or the process is gone.
    pub(crate) async fn settled(&self, conn: u64) {
        loop {
            let wake = self.shared.settled.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            {
                let state = lock(&self.shared.state);
                if state.dead.is_some() || !state.pending.values().any(|p| p.conn == Some(conn)) {
                    return;
                }
            }
            wake.await;
        }
    }

    pub(crate) async fn notify(&self, method: &str, params: Json) -> Result<(), DbError> {
        self.send(None, method, params).await
    }

    async fn send(&self, id: Option<u64>, m: &str, p: Json) -> Result<(), DbError> {
        let body = serde_json::to_vec(&Outgoing { id, m, p })
            .map_err(|e| DbError::Protocol(format!("encoding `{m}`: {e}")))?;
        let mut stdin = self.stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else {
            return Err(self.shared.dead_error());
        };
        if let Err(e) = write_frame(pipe, &body).await {
            stdin.take();
            self.shared.fail_all(format!(
                "writing to the {} sidecar: {e}",
                self.shared.engine
            ));
            self.kill();
            return Err(self.shared.dead_error());
        }
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Closing stdin is the shutdown request; the kill is the backstop.
        if let Ok(mut stdin) = self.stdin.try_lock() {
            stdin.take();
        }
        let shared = self.shared.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    tokio::time::sleep(KILL_GRACE).await;
                    shared.kill.notify_one();
                });
            }
            Err(_) => shared.kill.notify_one(),
        }
    }
}

fn child_env(extra: &[&str]) -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(k, _)| {
            let k = k.to_string_lossy();
            ENV_ALLOWLIST.contains(&k.as_ref())
                || k.starts_with("LC_")
                || extra.contains(&k.as_ref())
        })
        .collect()
}

#[cfg(unix)]
fn harden_child() -> std::io::Result<()> {
    // No core file can hold the password this process is about to receive.
    let none = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setrlimit reads a valid, initialised struct.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    // SAFETY: prctl with PR_SET_PDEATHSIG takes a plain signal number.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    }
    Ok(())
}

async fn supervise(
    mut child: Child,
    mut stdout: ChildStdout,
    stderr_task: tokio::task::JoinHandle<()>,
    shared: Arc<Shared>,
) {
    let reason = tokio::select! {
        reason = read_replies(&mut stdout, &shared) => reason,
        _ = shared.kill.notified() => "the sidecar was stopped".to_string(),
    };
    let _ = child.start_kill();
    let status = match tokio::time::timeout(KILL_GRACE, child.wait()).await {
        Ok(Ok(status)) => status.to_string(),
        _ => "exit status unknown".to_string(),
    };
    let _ = tokio::time::timeout(Duration::from_millis(500), stderr_task).await;
    let tail = shared.tail_excerpt();
    let mut message = format!("{} sidecar: {reason} ({status})", shared.engine);
    if !tail.is_empty() {
        message.push_str("; stderr: ");
        message.push_str(&tail);
    }
    tracing::debug!(target: "sidecar", engine = shared.engine, %message, "sidecar ended");
    shared.fail_all(message);
}

async fn read_replies(stdout: &mut ChildStdout, shared: &Shared) -> String {
    loop {
        let body = match read_frame(stdout, MAX_FRAME).await {
            Ok(Some(body)) => body,
            Ok(None) => return "exited".to_string(),
            Err(e) => return format!("protocol violation: {e}"),
        };
        let len = body.len();
        let reply: Incoming = match serde_json::from_slice(&body) {
            Ok(reply) => reply,
            Err(e) => return format!("protocol violation: unreadable frame ({e})"),
        };
        let result = match (reply.ok, reply.err) {
            (Some(ok), None) => Ok((ok, len)),
            (None, Some(mut err)) => {
                err.message = shared.redact(&err.message);
                Err(err.into_db_error())
            }
            _ => {
                return format!(
                    "protocol violation: reply {} is neither ok nor err",
                    reply.id
                )
            }
        };
        let Some(pending) = lock(&shared.state).pending.remove(&reply.id) else {
            return format!(
                "protocol violation: reply to id {} that is not pending",
                reply.id
            );
        };
        // The caller may have given up; the reply still settles the request.
        let _ = pending.tx.send(result);
        shared.settled.notify_waiters();
    }
}

async fn drain_stderr<R: AsyncRead + Unpin>(mut stderr: R, shared: Arc<Shared>) {
    let mut buf = vec![0u8; 4096];
    let mut line = Vec::new();
    let mut window = tokio::time::Instant::now();
    let mut logged = 0u32;
    loop {
        let n = match stderr.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            // An overlong line is cut in two rather than buffered without bound.
            if b == b'\n' || line.len() >= 4096 {
                let text = shared.redact(&String::from_utf8_lossy(&line));
                line.clear();
                if window.elapsed() >= Duration::from_secs(1) {
                    window = tokio::time::Instant::now();
                    logged = 0;
                }
                if logged < LOG_LINES_PER_SEC {
                    logged += 1;
                    tracing::info!(target: "sidecar", engine = shared.engine, "{text}");
                }
                push_tail(&shared, &text, true);
            }
            if b != b'\n' {
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        let text = shared.redact(&String::from_utf8_lossy(&line));
        push_tail(&shared, &text, false);
    }
}

fn push_tail(shared: &Shared, text: &str, newline: bool) {
    let mut tail = lock(&shared.tail);
    tail.push_str(text);
    if newline {
        tail.push('\n');
    }
    if tail.len() > TAIL_BYTES {
        let mut cut = tail.len() - TAIL_BYTES;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(secrets: &[&str]) -> Shared {
        Shared {
            engine: "test",
            state: Mutex::new(State {
                next_id: 1,
                pending: HashMap::new(),
                dead: None,
            }),
            secrets: secrets.iter().map(|s| s.to_string()).collect(),
            tail: Mutex::new(String::new()),
            kill: Notify::new(),
            settled: Notify::new(),
        }
    }

    #[test]
    fn redaction_masks_every_secret_and_ignores_empty_ones() {
        let s = shared(&["hunter2", ""]);
        assert_eq!(
            s.redact("dsn=scott/hunter2@db hunter2"),
            "dsn=scott/••••@db ••••"
        );
    }

    #[tokio::test]
    async fn stderr_reaches_the_tail_redacted_and_split_lines_are_joined() {
        let s = Arc::new(shared(&["hunter2"]));
        let input: &[u8] = b"connecting with hunt";
        let rest: &[u8] = b"er2\nok\npartial";
        drain_stderr(input.chain(rest), s.clone()).await;
        let tail = lock(&s.tail).clone();
        assert!(!tail.contains("hunter2"), "{tail}");
        assert_eq!(tail, "connecting with ••••\nok\npartial");
    }

    #[test]
    fn the_child_env_is_an_allowlist() {
        let env = child_env(&[]);
        for (k, _) in &env {
            let k = k.to_string_lossy();
            assert!(
                ENV_ALLOWLIST.contains(&k.as_ref()) || k.starts_with("LC_"),
                "{k} leaked into the sidecar env"
            );
        }
        assert!(!env.iter().any(|(k, _)| k == "PATH"));
    }
}
