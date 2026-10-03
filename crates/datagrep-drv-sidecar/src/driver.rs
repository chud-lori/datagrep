use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use serde_json::{json, Map, Value as Json};

use datagrep_api::caps::{Capabilities, Caps};
use datagrep_api::config::{
    ConfigError, ConfigSchema, ConfigValue, ConnectionConfig, ResolvedConfig,
};
use datagrep_api::driver::{ConnectCtx, Connection, Driver, DriverMeta};
use datagrep_api::error::DbError;

use crate::connection::SidecarConnection;
use crate::manifest::EngineManifest;
use crate::process::{Process, Spawn};
use crate::wire::{config_json, ConnectReply};

pub struct SidecarDriver {
    manifest: &'static EngineManifest,
    command: Option<(PathBuf, Vec<OsString>)>,
    hasher: std::collections::hash_map::RandomState,
    // One process per profile: keyed by a fingerprint of the resolved config, secret included.
    processes: tokio::sync::Mutex<HashMap<u64, Weak<Process>>>,
}

impl std::fmt::Debug for SidecarDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarDriver")
            .field("engine", &self.manifest.id)
            .finish()
    }
}

impl SidecarDriver {
    pub fn new(manifest: &'static EngineManifest) -> Self {
        Self {
            manifest,
            command: None,
            hasher: Default::default(),
            processes: Default::default(),
        }
    }

    // Runs `program args..` instead of looking the engine binary up; tests point this at a fake.
    pub fn with_program(
        manifest: &'static EngineManifest,
        program: impl Into<PathBuf>,
        args: Vec<OsString>,
    ) -> Self {
        Self {
            command: Some((program.into(), args)),
            ..Self::new(manifest)
        }
    }

    fn command(&self) -> Result<(PathBuf, Vec<OsString>), DbError> {
        match &self.command {
            Some(cmd) => Ok(cmd.clone()),
            None => Ok((locate_program(self.manifest)?, Vec::new())),
        }
    }

    fn fingerprint(&self, config: &Map<String, Json>, secrets: &Map<String, Json>) -> u64 {
        let mut h = self.hasher.build_hasher();
        self.manifest.id.hash(&mut h);
        Json::Object(config.clone()).to_string().hash(&mut h);
        Json::Object(secrets.clone()).to_string().hash(&mut h);
        h.finish()
    }

    async fn process_for(
        &self,
        fingerprint: u64,
        secrets: Vec<String>,
    ) -> Result<Arc<Process>, DbError> {
        let mut processes = self.processes.lock().await;
        processes.retain(|_, weak| weak.strong_count() > 0);
        if let Some(process) = processes.get(&fingerprint).and_then(Weak::upgrade) {
            if process.is_alive() {
                return Ok(process);
            }
        }
        let (program, args) = self.command()?;
        let process = Process::spawn(Spawn {
            engine: self.manifest.id,
            program,
            args,
            extra_env: self.manifest.env,
            secrets,
        })
        .await?;
        self.check_handshake(&process)?;
        processes.insert(fingerprint, Arc::downgrade(&process));
        Ok(process)
    }

    // The sidecar may narrow what the manifest grants, never widen it or pick its own language.
    fn check_handshake(&self, process: &Process) -> Result<(), DbError> {
        let hello = process.hello();
        let fail = |why: String| {
            process.kill();
            Err(DbError::Protocol(why))
        };
        if hello.engine != self.manifest.id {
            return fail(format!(
                "sidecar says it serves `{}`, expected `{}`",
                hello.engine, self.manifest.id
            ));
        }
        let declared = serde_json::to_value(self.manifest.language).unwrap_or(Json::Null);
        if hello.language != declared {
            return fail(format!(
                "sidecar declares language {}, the app pins {declared}",
                hello.language
            ));
        }
        match Caps::from_bits(hello.caps) {
            Some(caps) if self.manifest.flags.contains(caps) => Ok(()),
            _ => fail(format!(
                "sidecar claims capabilities {:#x} beyond the manifest's {:#x}",
                hello.caps,
                self.manifest.flags.bits()
            )),
        }
    }
}

// Next to the running binary, then a macOS bundle's Helpers, then DATAGREP_ENGINE_DIR.
fn locate_program(manifest: &EngineManifest) -> Result<PathBuf, DbError> {
    let name = manifest.program;
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
    {
        candidates.push(dir.join(name));
        candidates.push(dir.join("../Helpers").join(name));
    }
    if let Some(dir) = std::env::var_os("DATAGREP_ENGINE_DIR") {
        candidates.push(PathBuf::from(dir).join(name));
    }
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            let looked: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            DbError::Connect(format!(
                "the {} engine is not installed (looked for {})",
                manifest.display_name,
                looked.join(", ")
            ))
        })
}

#[async_trait]
impl Driver for SidecarDriver {
    fn meta(&self) -> DriverMeta {
        DriverMeta {
            id: Arc::from(self.manifest.id),
            display_name: Arc::from(self.manifest.display_name),
            version: Arc::from(env!("CARGO_PKG_VERSION")),
        }
    }

    fn capabilities(&self) -> Capabilities {
        self.manifest.capabilities()
    }

    fn config_schema(&self) -> ConfigSchema {
        self.manifest.config_schema()
    }

    fn parse_url(&self, url: &str) -> Result<ConnectionConfig, ConfigError> {
        parse_url(self.manifest, url)
    }

    async fn connect(
        &self,
        cfg: &ResolvedConfig,
        ctx: ConnectCtx,
    ) -> Result<Box<dyn Connection>, DbError> {
        let schema = self.manifest.config_schema();
        let mut config = config_json(&cfg.config.values);
        // Secrets leave the config and travel only inside the connect frame.
        let mut secrets = Map::new();
        for field in schema.fields.iter().filter(|f| f.secret) {
            if let Some(v) = config.remove(&*field.key) {
                secrets.insert(field.key.to_string(), v);
            }
        }
        for (k, v) in &cfg.secrets {
            secrets.insert(k.clone(), Json::from(v.expose()));
        }
        let secret_values: Vec<String> = secrets
            .values()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();

        let process = self
            .process_for(self.fingerprint(&config, &secrets), secret_values)
            .await?;
        let conn = process.next_conn();
        let params = json!({
            "conn": conn,
            "config": config,
            "secrets": secrets,
            "connect_timeout_ms": ctx.connect_timeout.map(|d| d.as_millis() as u64),
            "application_name": ctx.application_name.as_deref(),
        });
        let (reply, _) = process.call(Some(conn), "connect", params).await?;
        let reply: ConnectReply = serde_json::from_value(reply)
            .map_err(|e| DbError::Protocol(format!("bad connect reply: {e}")))?;
        let mut caps = self.manifest.capabilities();
        caps.flags &= Caps::from_bits_truncate(process.hello().caps);
        Ok(Box::new(SidecarConnection::new(
            process,
            conn,
            reply.server.into(),
            caps,
            self.manifest,
        )))
    }
}

// scheme://user:password@host:port/<path_key>
pub fn parse_url(manifest: &EngineManifest, url: &str) -> Result<ConnectionConfig, ConfigError> {
    let invalid = |reason: &str| ConfigError::InvalidUrl {
        reason: reason.to_string(),
    };
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| invalid("expected scheme://"))?;
    if !manifest.url_schemes.contains(&scheme) {
        return Err(invalid(&format!(
            "`{scheme}` is not a {} URL",
            manifest.display_name
        )));
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut values = BTreeMap::new();
    let host_port = match authority.rsplit_once('@') {
        Some((userinfo, host_port)) => {
            let (user, password) = match userinfo.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (userinfo, None),
            };
            if !user.is_empty() {
                values.insert("user".into(), ConfigValue::Str(percent_decode(user)?));
            }
            if let Some(p) = password {
                values.insert("password".into(), ConfigValue::Str(percent_decode(p)?));
            }
            host_port
        }
        None => authority,
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p.parse().map_err(|_| invalid("port is not a number"))?;
            (h, port)
        }
        None => (host_port, manifest.default_port),
    };
    if host.is_empty() {
        return Err(invalid("missing host"));
    }
    values.insert("host".into(), ConfigValue::Str(host.to_string()));
    values.insert("port".into(), ConfigValue::Num(f64::from(port)));
    if !path.is_empty() {
        values.insert(
            manifest.path_key.to_string(),
            ConfigValue::Str(percent_decode(path)?),
        );
    }
    Ok(ConnectionConfig {
        driver: Arc::from(manifest.id),
        values,
    })
}

fn percent_decode(s: &str) -> Result<String, ConfigError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| ConfigError::InvalidUrl {
                    reason: "bad percent escape".into(),
                })?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ConfigError::InvalidUrl {
        reason: "percent escape is not UTF-8".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ORACLE;

    #[test]
    fn oracle_urls_fill_the_schema_fields() {
        let cfg = parse_url(&ORACLE, "oracle://scott:t%40ger@db.local:1522/FREEPDB1").unwrap();
        let s = |k: &str| match cfg.values.get(k) {
            Some(ConfigValue::Str(s)) => s.clone(),
            other => panic!("{k}: {other:?}"),
        };
        assert_eq!(s("user"), "scott");
        assert_eq!(s("password"), "t@ger");
        assert_eq!(s("host"), "db.local");
        assert_eq!(s("service"), "FREEPDB1");
        assert_eq!(cfg.values.get("port"), Some(&ConfigValue::Num(1522.0)));

        let bare = parse_url(&ORACLE, "oracle://db.local").unwrap();
        assert_eq!(bare.values.get("port"), Some(&ConfigValue::Num(1521.0)));
        assert!(!bare.values.contains_key("password"));

        assert!(parse_url(&ORACLE, "postgres://h/db").is_err());
        assert!(parse_url(&ORACLE, "oracle://h:notaport/x").is_err());
        assert!(parse_url(&ORACLE, "oracle://u:%zz@h/x").is_err());
    }

    #[test]
    fn every_schema_key_the_url_fills_is_declared() {
        let keys: Vec<String> = ORACLE
            .config_schema()
            .fields
            .iter()
            .map(|f| f.key.to_string())
            .collect();
        let cfg = parse_url(&ORACLE, "oracle://u:p@h:1/s").unwrap();
        for k in cfg.values.keys() {
            assert!(keys.contains(k), "{k} is not in the schema");
        }
    }
}
