//! Out-of-process JavaScript via the real `mongosh`, offered only when the
//! native expression parser refuses a script.
//!
//! The native parser (`datagrep_lang::mongo`) is the fast path and handles
//! every query expression. A variable, loop, or arithmetic makes it return
//! [`MongoError::UnsupportedJs`]; that single case is where a UI may offer to
//! re-run the script through `mongosh`. datagrep detects `mongosh` on `PATH`
//! and never bundles it.
//!
//! Credentials never reach `mongosh`'s argv. The URI is written to a `0600`
//! temp file and read back inside the `--eval` bootstrap via `require("fs")`,
//! so `ps`/`/proc` on a shared machine see only the script and a file path.
//! `--json=relaxed` makes `mongosh` emit extended JSON, which maps onto the
//! same [`Value`] the native driver produces.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use datagrep_api::value::Value;
use datagrep_lang::mongo::error::MongoError;

use crate::value::bson_to_value;

/// Shown in the UI before anything spawns. Running mongosh crosses a process
/// boundary that in-process drivers never do.
pub const CREDENTIAL_WARNING: &str = "Running this with mongosh hands your connection credentials to a separate program (mongosh). datagrep otherwise keeps them in-process and off the command line.";

/// Shown when `mongosh` is not on `PATH`. datagrep never downloads or bundles it.
pub const INSTALL_HINT: &str =
    "mongosh is not on your PATH. Install it with `brew install mongosh`.";

#[cfg(windows)]
const MONGOSH_BIN: &str = "mongosh.exe";
#[cfg(not(windows))]
const MONGOSH_BIN: &str = "mongosh";

/// Env var carrying the path (not the URI) of the temp file to the child.
const URI_FILE_ENV: &str = "DATAGREP_MONGOSH_URI_FILE";

/// Reads the URI from the `0600` temp file and opens the connection as `db`
/// before the user's script runs. The URI never appears here — only the env
/// var naming the file does.
const BOOTSTRAP: &str = "globalThis.db = connect(require(\"fs\").readFileSync(process.env.DATAGREP_MONGOSH_URI_FILE, \"utf8\").trim());";

#[derive(Debug, thiserror::Error)]
pub enum MongoshError {
    #[error("{INSTALL_HINT}")]
    NotFound,
    #[error("could not create a private file for the connection string: {0}")]
    TempFile(std::io::Error),
    #[error("could not run mongosh: {0}")]
    Spawn(std::io::Error),
    #[error("mongosh failed: {0}")]
    Failed(String),
    #[error("could not read mongosh output as extended JSON: {0}")]
    Output(String),
}

/// `true` only when the native parser refused a script as non-expression
/// JavaScript — the one case worth offering mongosh for. Every other parse
/// error is a syntax mistake the user should fix, not a scripting need.
pub fn can_offer(err: &MongoError) -> bool {
    matches!(err, MongoError::UnsupportedJs)
}

/// First `mongosh` on `PATH`, or `None` when it is not installed.
pub fn find_mongosh() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| find_on_path(&p))
}

fn find_on_path(path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(MONGOSH_BIN))
        .find(|cand| is_executable(cand))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The full `--eval` string: the connection bootstrap followed by the user's
/// refused script. Kept separate so a test can assert it carries no credential.
fn build_eval(script: &str) -> String {
    format!("{BOOTSTRAP}\n{script}")
}

/// Runs `script` against `uri` through `mongosh` and maps its extended-JSON
/// output onto [`Value`]. `uri` carries the credentials and is written only to
/// a `0600` temp file — never to argv or the environment.
pub fn run(mongosh: &Path, uri: &str, script: &str) -> Result<Value, MongoshError> {
    let uri_file = TempUri::create(uri).map_err(MongoshError::TempFile)?;

    let output = Command::new(mongosh)
        .args(["--nodb", "--quiet", "--json=relaxed", "--eval"])
        .arg(build_eval(script))
        .env(URI_FILE_ENV, &uri_file.path)
        .output()
        .map_err(MongoshError::Spawn)?;

    drop(uri_file);

    if !output.status.success() {
        return Err(MongoshError::Failed(describe_failure(&output)));
    }
    ejson_to_value(&String::from_utf8_lossy(&output.stdout))
}

/// Maps `mongosh --json=relaxed` output onto [`Value`] by routing it through
/// the same `bson` extended-JSON reader and [`bson_to_value`] the native driver
/// uses, rather than scraping formatted text.
pub fn ejson_to_value(stdout: &str) -> Result<Value, MongoshError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    let json: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| MongoshError::Output(e.to_string()))?;
    let bson = bson::Bson::try_from(json).map_err(|e| MongoshError::Output(e.to_string()))?;
    Ok(bson_to_value(&bson))
}

/// On failure `mongosh --json` serializes the thrown error to stdout as JSON;
/// pull out its `message` for a readable line, falling back to raw streams.
fn describe_failure(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(stdout.trim()) {
        if let Some(serde_json::Value::String(msg)) = map.get("message") {
            return msg.clone();
        }
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if text.is_empty() {
        format!("exited with {}", output.status)
    } else {
        text.to_string()
    }
}

/// A `0600` temp file holding the connection URI, removed on drop.
struct TempUri {
    path: PathBuf,
}

impl TempUri {
    fn create(uri: &str) -> std::io::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir();
        for _ in 0..16 {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("datagrep-mongosh-{}-{n}.uri", std::process::id()));
            match create_private(&path) {
                Ok(mut file) => {
                    file.write_all(uri.as_bytes())?;
                    file.flush()?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not find a free temp file name for the mongosh connection string",
        ))
    }
}

impl Drop for TempUri {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::value::{Document, Value};
    use datagrep_lang::mongo::parse;

    // Routing: only UnsupportedJs opens the mongosh offer.
    #[test]
    fn offers_only_for_unsupported_js() {
        let refused = parse("for (let i = 0; i < 3; i++) { db.a.insertOne({i: i}) }").unwrap_err();
        assert_eq!(refused, MongoError::UnsupportedJs);
        assert!(can_offer(&refused));

        let syntax = parse("db.a.find(").unwrap_err();
        assert!(!matches!(syntax, MongoError::UnsupportedJs));
        assert!(!can_offer(&syntax));

        // A plain expression the native driver handles never reaches mongosh.
        assert!(parse("db.users.find({x: 1})").is_ok());
    }

    #[test]
    fn eval_carries_the_script_but_never_the_uri() {
        let eval = build_eval("db.users.find({name: secret})");
        assert!(eval.contains("db.users.find({name: secret})"));
        assert!(eval.contains("process.env.DATAGREP_MONGOSH_URI_FILE"));
        assert!(!eval.contains("mongodb://"));
        assert!(!eval.contains("hunter2"));
    }

    // Extended JSON (relaxed) -> Value::Document, the shape the result grid reads.
    #[test]
    fn ejson_relaxed_maps_to_value_document() {
        let out = r#"[
            {"_id": {"$oid": "507f1f77bcf86cd799439011"}, "n": 42, "when": {"$date": "1970-01-01T00:00:00Z"}},
            {"_id": {"$oid": "507f191e810c19729de860ea"}, "n": 7}
        ]"#;
        let value = ejson_to_value(out).unwrap();
        let Value::Array(rows) = value else {
            panic!("expected an array, got {value:?}");
        };
        assert_eq!(rows.len(), 2);
        let Value::Document(first) = &rows[0] else {
            panic!("expected a document");
        };
        assert_eq!(first.get("n"), Some(&Value::I64(42)));
        assert!(matches!(first.get("when"), Some(Value::Timestamp { .. })));
        assert!(matches!(
            first.get("_id"),
            Some(Value::Unsupported { type_name, .. }) if &**type_name == "ObjectId"
        ));
    }

    #[test]
    fn ejson_scalar_and_empty() {
        assert_eq!(ejson_to_value("").unwrap(), Value::Null);
        assert_eq!(ejson_to_value("  ").unwrap(), Value::Null);
        let doc = ejson_to_value(r#"{"ok": true}"#).unwrap();
        assert_eq!(
            doc,
            Value::Document(std::sync::Arc::new(Document::from_fields(vec![(
                std::sync::Arc::from("ok"),
                Value::Bool(true)
            )])))
        );
    }

    #[test]
    fn invalid_output_is_an_error_not_a_panic() {
        assert!(matches!(
            ejson_to_value("not json at all"),
            Err(MongoshError::Output(_))
        ));
    }

    // mongosh-missing path: both the PATH scan and a spawn against a bogus
    // binary fail without a live mongosh.
    #[test]
    fn find_on_path_returns_none_when_absent() {
        let dir = std::env::temp_dir().join(format!("dg-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(find_on_path(dir.as_os_str()).is_none());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn run_against_a_missing_binary_is_a_spawn_error() {
        let missing = Path::new("/nonexistent/datagrep/mongosh");
        let err = run(missing, "mongodb://localhost:27017/test", "db.a.find({})").unwrap_err();
        assert!(matches!(err, MongoshError::Spawn(_)));
    }

    #[test]
    fn temp_uri_is_private_and_removed() {
        let path;
        {
            let uri = TempUri::create("mongodb://u:p@h/db").unwrap();
            path = uri.path.clone();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "mongodb://u:p@h/db"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }
        assert!(!path.exists(), "temp file should be removed on drop");
    }
}
