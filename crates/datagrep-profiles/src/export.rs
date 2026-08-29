use datagrep_api::safety::SafetyLevel;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::ProfilesError;
use crate::model::{Folder, Profile, Tunnel};
use crate::secrets::validate_no_secrets;

const EXPORT_VERSION: u32 = 1;

fn default_version() -> u32 {
    EXPORT_VERSION
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExportBundle {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub folder: Vec<Folder>,
    #[serde(default)]
    pub profile: Vec<Profile>,
    #[serde(default)]
    pub tunnel: Vec<Tunnel>,
}

impl ExportBundle {
    pub(crate) fn to_toml(&self) -> Result<String, ProfilesError> {
        toml::to_string_pretty(self).map_err(Into::into)
    }

    pub(crate) fn from_toml(text: &str) -> Result<ExportBundle, ProfilesError> {
        let mut doc: toml::Value = toml::from_str(text)?;
        upgrade_confirm_writes(&mut doc);
        doc.try_into().map_err(Into::into)
    }
}

// A bundle exported before the safety ladder carries `confirm_writes`; read it as the rung it meant.
fn upgrade_confirm_writes(doc: &mut toml::Value) {
    let Some(profiles) = doc.get_mut("profile").and_then(toml::Value::as_array_mut) else {
        return;
    };
    for profile in profiles {
        let Some(table) = profile.as_table_mut() else {
            continue;
        };
        let legacy = table.remove("confirm_writes").and_then(|v| v.as_bool());
        if table.contains_key("safety") {
            continue;
        }
        let level = SafetyLevel::from_confirm_writes(legacy.unwrap_or(false));
        table.insert("safety".to_string(), toml::Value::String(level.to_string()));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStrategy {
    Merge,
    Replace,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub folders_upserted: usize,
    pub profiles_upserted: usize,
    pub tunnels_upserted: usize,
    pub folders_removed: usize,
    pub profiles_removed: usize,
    pub tunnels_removed: usize,
}

pub(crate) fn apply_import(
    conn: &mut Connection,
    bundle: ExportBundle,
    strategy: ImportStrategy,
) -> Result<ImportSummary, ProfilesError> {
    let tx = conn.transaction()?;
    tx.pragma_update(None, "defer_foreign_keys", "ON")?;

    let mut summary = ImportSummary::default();

    if strategy == ImportStrategy::Replace {
        // Children before parents: profile references folder and tunnel.
        summary.profiles_removed = tx.execute("DELETE FROM profile", [])?;
        summary.folders_removed = tx.execute("DELETE FROM folder", [])?;
        summary.tunnels_removed = tx.execute("DELETE FROM tunnel", [])?;
    }

    for f in bundle.folder {
        tx.execute(
            "INSERT INTO folder (id, parent_id, name, sort_order, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(id) DO UPDATE SET
                parent_id = excluded.parent_id,
                name = excluded.name,
                sort_order = excluded.sort_order,
                updated_at = excluded.updated_at",
            params![
                f.id,
                f.parent_id,
                f.name,
                f.sort_order,
                f.created_at,
                f.updated_at
            ],
        )?;
        summary.folders_upserted += 1;
    }

    for t in bundle.tunnel {
        tx.execute(
            "INSERT INTO tunnel (id, name, host, port, username, secret_ref, known_hosts_pin, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                host = excluded.host,
                port = excluded.port,
                username = excluded.username,
                secret_ref = excluded.secret_ref,
                known_hosts_pin = excluded.known_hosts_pin,
                updated_at = excluded.updated_at",
            params![
                t.id, t.name, t.host, t.port, t.username, t.secret_ref, t.known_hosts_pin,
                t.created_at, t.updated_at,
            ],
        )?;
        summary.tunnels_upserted += 1;
    }

    for p in bundle.profile {
        validate_no_secrets(&p.config)?;
        let config_json = serde_json::to_string(&p.config)?;
        tx.execute(
            "INSERT INTO profile (
                id, folder_id, name, driver_id, config_json, secret_ref, tunnel_id, color,
                read_only, safety, auto_limit, idle_timeout_s, last_used_at, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
             ON CONFLICT(id) DO UPDATE SET
                folder_id = excluded.folder_id,
                name = excluded.name,
                driver_id = excluded.driver_id,
                config_json = excluded.config_json,
                secret_ref = excluded.secret_ref,
                tunnel_id = excluded.tunnel_id,
                color = excluded.color,
                read_only = excluded.read_only,
                safety = excluded.safety,
                auto_limit = excluded.auto_limit,
                idle_timeout_s = excluded.idle_timeout_s,
                updated_at = excluded.updated_at",
            params![
                p.id,
                p.folder_id,
                p.name,
                p.driver_id,
                config_json,
                p.secret_ref,
                p.tunnel_id,
                p.color,
                p.read_only,
                p.safety.as_str(),
                p.auto_limit,
                p.idle_timeout_s,
                p.last_used_at,
                p.created_at,
                p.updated_at,
            ],
        )?;
        summary.profiles_upserted += 1;
    }

    tx.commit()?;
    Ok(summary)
}
