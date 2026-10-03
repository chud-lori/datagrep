// A scripted sidecar: argv[1] picks the handshake, the statement text picks the behaviour.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value as Json};

const SERVER_CANCEL: u32 = 1 << 4;

enum Kind {
    Stream {
        left: u64,
        fetches: u32,
        crash_at: Option<u32>,
    },
    Types,
    Hang,
    Deaf,
    Flood,
}

#[derive(Default)]
struct State {
    cursors: HashMap<u64, Kind>,
    next_cursor: u64,
    cancels: HashMap<u64, u64>,
    password: String,
}

struct Fake {
    mode: String,
    state: Mutex<State>,
    wake: Condvar,
    out: Mutex<std::io::Stdout>,
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let fake = Arc::new(Fake {
        mode,
        state: Mutex::new(State {
            next_cursor: 1,
            ..State::default()
        }),
        wake: Condvar::new(),
        out: Mutex::new(std::io::stdout()),
    });
    let mut stdin = std::io::stdin().lock();
    loop {
        let mut header = [0u8; 4];
        if stdin.read_exact(&mut header).is_err() {
            return;
        }
        let mut body = vec![0u8; u32::from_be_bytes(header) as usize];
        if stdin.read_exact(&mut body).is_err() {
            return;
        }
        let req: Json = serde_json::from_slice(&body).expect("parent sent bad JSON");
        let fake = fake.clone();
        thread::spawn(move || fake.handle(req));
    }
}

impl Fake {
    fn raw(&self, body: &[u8], len: u32) {
        let mut out = self.out.lock().unwrap();
        out.write_all(&len.to_be_bytes()).unwrap();
        out.write_all(body).unwrap();
        out.flush().unwrap();
    }

    fn send(&self, frame: Json) {
        let body = serde_json::to_vec(&frame).unwrap();
        self.raw(&body, body.len() as u32);
    }

    fn ok(&self, id: &Json, ok: Json) {
        self.send(json!({ "id": id, "ok": ok }));
    }

    fn err(&self, id: &Json, kind: &str, message: &str) {
        self.send(
            json!({ "id": id, "err": { "kind": kind, "code": "FAKE-1", "message": message } }),
        );
    }

    fn handle(&self, req: Json) {
        let id = &req["id"];
        let p = &req["p"];
        let conn = p["conn"].as_u64().unwrap_or(0);
        match req["m"].as_str().unwrap_or("") {
            "hello" => self.hello(id),
            "connect" => self.connect(id, p),
            "cancel" => {
                *self.state.lock().unwrap().cancels.entry(conn).or_default() += 1;
                self.wake.notify_all();
            }
            "execute" => self.execute(id, p),
            "fetch" => self.fetch(id, conn, p),
            "close_cursor" => {
                let cursor = p["cursor"].as_u64().unwrap_or(0);
                self.state.lock().unwrap().cursors.remove(&cursor);
                self.wake.notify_all();
                self.ok(id, json!({}));
            }
            "set_read_only" => self.ok(id, json!({ "enforcement": "client" })),
            "children" => self.ok(
                id,
                json!({ "items": [{ "path": ["HR"], "kind": "Schema", "has_children": true }] }),
            ),
            "describe" => self.ok(
                id,
                json!({
                    "node": { "path": p["path"], "kind": "Table" },
                    "fields": [{ "name": "ID", "logical": "Decimal", "native_type": "NUMBER" }],
                }),
            ),
            "ping" | "disconnect" => self.ok(id, json!({})),
            other => self.err(id, "unsupported", other),
        }
    }

    fn hello(&self, id: &Json) {
        let (language, caps) = match self.mode.as_str() {
            "silent" => return,
            "wrong-language" => (json!({ "Sql": "Postgres" }), SERVER_CANCEL),
            "widen-caps" => (json!("Unclassified"), SERVER_CANCEL | 1),
            _ => (json!("Unclassified"), SERVER_CANCEL),
        };
        self.ok(
            id,
            json!({
                "protocol": 1,
                "engine": "fake",
                "engine_version": "0",
                "language": language,
                "caps": caps,
            }),
        );
    }

    fn connect(&self, id: &Json, p: &Json) {
        let Some(password) = p["secrets"]["password"].as_str() else {
            return self.err(id, "auth", "no password in the connect frame");
        };
        if p["config"].get("password").is_some() {
            return self.err(id, "config", "the password travelled in config");
        }
        let in_argv = std::env::args().any(|a| a.contains(password));
        let in_env = std::env::vars().any(|(k, v)| k.contains(password) || v.contains(password));
        if in_argv || in_env {
            return self.err(id, "config", "the password is visible in argv or env");
        }
        self.state.lock().unwrap().password = password.to_string();
        let env: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
        self.ok(
            id,
            json!({ "server": { "product": "fake", "version": "0", "details": [["env", env.join(",")]] } }),
        );
    }

    fn execute(&self, id: &Json, p: &Json) {
        if self.mode == "no-execute" {
            std::process::exit(9);
        }
        let text = p["text"].as_str().unwrap_or("");
        let mut words = text.split_whitespace();
        let verb = words.next().unwrap_or("");
        let arg: Option<u64> = words.next().and_then(|w| w.parse().ok());
        let kind = match verb {
            "stream" => Kind::Stream {
                left: arg.unwrap_or(0),
                fetches: 0,
                crash_at: None,
            },
            "crash-after" => Kind::Stream {
                left: u64::MAX,
                fetches: 0,
                crash_at: Some(arg.unwrap_or(1) as u32),
            },
            "types" => Kind::Types,
            "hang" => Kind::Hang,
            "deaf" => Kind::Deaf,
            "flood" => Kind::Flood,
            "ack" => {
                return self.ok(
                    id,
                    json!({ "cursor": 0, "shape": { "kind": "ack", "affected": 3 } }),
                )
            }
            "boom" => return self.err(id, "query", "ORA-00942: table or view does not exist"),
            "panic" => return self.err(id, "panic", "nil pointer dereference"),
            "safety" => return self.err(id, "safety", "granted"),
            "oversized" => return self.raw(b"", 1 << 30),
            "garbage" => return self.raw(b"not json", 8),
            "unsolicited" => return self.ok(&json!(999_999), json!({})),
            "abort" => std::process::abort(),
            "leak" => {
                let password = self.state.lock().unwrap().password.clone();
                eprintln!("driver: dialing fake://scott:{password}@db");
                std::process::exit(3);
            }
            "sleep" => return thread::sleep(Duration::from_secs(3600)),
            other => return self.err(id, "query", &format!("unknown statement {other:?}")),
        };
        let fields = match kind {
            Kind::Types => json!([
                { "name": "b", "logical": "Bool" },
                { "name": "i", "logical": "I64" },
                { "name": "d", "logical": "Decimal", "native_type": "NUMBER" },
                { "name": "s", "logical": "Str", "nullable": true },
                { "name": "bytes", "logical": "Bytes" },
                { "name": "ts", "logical": "Timestamp" },
                { "name": "u", "logical": "Uuid" },
                { "name": "f", "logical": "F64" },
                { "name": "mixed", "logical": "I64" },
            ]),
            _ => json!([
                { "name": "ID", "logical": "I64" },
                { "name": "NAME", "logical": "Str" },
            ]),
        };
        let cursor = {
            let mut state = self.state.lock().unwrap();
            let cursor = state.next_cursor;
            state.next_cursor += 1;
            state.cursors.insert(cursor, kind);
            cursor
        };
        self.ok(
            id,
            json!({ "cursor": cursor, "shape": { "kind": "table", "fields": fields } }),
        );
    }

    fn fetch(&self, id: &Json, conn: u64, p: &Json) {
        let cursor = p["cursor"].as_u64().unwrap_or(0);
        let max_rows = p["max_rows"].as_u64().unwrap_or(1).max(1);
        let mut state = self.state.lock().unwrap();
        let generation = state.cancels.get(&conn).copied().unwrap_or(0);
        let password = state.password.clone();
        let Some(kind) = state.cursors.get_mut(&cursor) else {
            drop(state);
            return self.err(id, "query", "no such cursor");
        };
        let rows: Vec<Json> = match kind {
            Kind::Stream {
                left,
                fetches,
                crash_at,
            } => {
                *fetches += 1;
                if Some(*fetches) == *crash_at {
                    eprintln!("driver: lost connection to fake://scott:{password}@db");
                    std::process::exit(3);
                }
                if *left == 0 {
                    state.cursors.remove(&cursor);
                    drop(state);
                    return self.ok(id, json!({ "batch": null }));
                }
                let n = max_rows.min(*left);
                *left -= n;
                (0..n).map(|i| json!([i, format!("row {i}")])).collect()
            }
            Kind::Types => {
                state.cursors.remove(&cursor);
                vec![json!([
                    true,
                    i64::MIN,
                    "12345678901234567890.5",
                    null,
                    "AAH/",
                    { "us": 1_700_000_000_000_000i64, "tz": "utc" },
                    "01234567-89ab-cdef-0123-456789abcdef",
                    "NaN",
                    { "$t": "str", "v": "not a number" },
                ])]
            }
            Kind::Flood => (0..=max_rows).map(|i| json!([i, "x"])).collect(),
            Kind::Hang => {
                while state.cancels.get(&conn).copied().unwrap_or(0) == generation
                    && state.cursors.contains_key(&cursor)
                {
                    state = self.wake.wait(state).unwrap();
                }
                drop(state);
                return self.err(id, "cancelled", "cancelled by the parent");
            }
            Kind::Deaf => {
                drop(state);
                thread::sleep(Duration::from_secs(3600));
                return;
            }
        };
        drop(state);
        self.ok(id, json!({ "batch": { "rows": rows } }));
    }
}
