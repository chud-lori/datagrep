//! Row types for every table in design §3.7, plus the small enums (`Env`,
//! `HistoryStatus`) that back their `CHECK`-constrained columns.
//!
//! Timestamps are milliseconds since the Unix epoch (`i64`), matching
//! SQLite's `INTEGER` affinity and avoiding a chrono dependency this crate
//! doesn't otherwise need.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use dbx_api::ConnectionConfig;
use serde::{Deserialize, Serialize};

/// Current time in milliseconds since the Unix epoch. `Ok(0)` on a clock
/// before 1970 is intentionally impossible to hit in practice; we still fall
/// back to `0` rather than panic (never `unwrap` outside tests).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Generates a locally-unique id for a new row.
///
/// Deviation from the ticket: the allowed dependency list has no `uuid`
/// crate, and `dbx-profiles` has no opinion on id *format* — callers that
/// already carry a UUID generator (e.g. `dbx-core`) are free to hand in
/// their own `id` on every `create_*` call instead. This helper exists so
/// the crate (and its tests) are self-sufficient: `<millis>-<counter>-<pid>`
/// is unique per-process and monotonically increasing, which is all a local
/// SQLite primary key needs.
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", now_ms(), n, std::process::id())
}

/// design §3.7: a folder in the profile tree. `parent_id = None` is the root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub sort_order: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// design §3.8: the deployment tier a profile targets, driving the red
/// window chrome / confirm-on-write / blocked-unqualified-write guardrails.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Env {
    #[default]
    Dev,
    Staging,
    Prod,
}

impl Env {
    pub fn as_str(self) -> &'static str {
        match self {
            Env::Dev => "dev",
            Env::Staging => "staging",
            Env::Prod => "prod",
        }
    }

    pub fn parse(s: &str) -> Option<Env> {
        match s {
            "dev" => Some(Env::Dev),
            "staging" => Some(Env::Staging),
            "prod" => Some(Env::Prod),
            _ => None,
        }
    }
}

impl fmt::Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// design §3.7 `profile` table. `config` is `dbx_api::ConnectionConfig` —
/// the exact type drivers consume — serialized to `config_json`; it is
/// validated on every save to reject secret-shaped keys (design §3.8, see
/// `crate::secrets::validate_no_secrets`). There is deliberately no `secret`
/// field on this type: nothing secret can flow through it by construction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub folder_id: Option<String>,
    pub name: String,
    pub driver_id: String,
    pub config: ConnectionConfig,
    /// Opaque pointer into the OS keychain, e.g. `keychain:dbx:profile:<id>`.
    /// Never a secret value itself (design §3.8).
    pub secret_ref: Option<String>,
    pub tunnel_id: Option<String>,
    pub env: Env,
    pub color: Option<String>,
    pub read_only: bool,
    pub confirm_writes: bool,
    pub auto_limit: Option<i64>,
    pub idle_timeout_s: Option<i64>,
    pub last_used_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// design §3.7 `tunnel` table. SSH is in-process `russh` (design §3.8) —
/// only connection coordinates and a `secret_ref` for the auth material live
/// here, never a password or key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tunnel {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub secret_ref: Option<String>,
    pub known_hosts_pin: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// How a recorded query finished. Backs `query_history.status` (`CHECK`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryStatus {
    Ok,
    Error,
    Cancelled,
}

impl HistoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HistoryStatus::Ok => "ok",
            HistoryStatus::Error => "error",
            HistoryStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<HistoryStatus> {
        match s {
            "ok" => Some(HistoryStatus::Ok),
            "error" => Some(HistoryStatus::Error),
            "cancelled" => Some(HistoryStatus::Cancelled),
            _ => None,
        }
    }
}

/// What the caller submits to `Store::record_history` — no `id`/`text_hash`,
/// the store computes those (design §3.7 dedupe window).
#[derive(Debug, Clone, PartialEq)]
pub struct NewHistoryEntry {
    pub profile_id: String,
    pub text: String,
    pub started_at: i64,
    pub duration_ms: Option<i64>,
    pub row_count: Option<i64>,
    pub status: HistoryStatus,
    pub error: Option<String>,
}

/// design §3.7 `query_history` row, as persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub profile_id: String,
    pub text: String,
    pub text_hash: String,
    pub started_at: i64,
    pub duration_ms: Option<i64>,
    pub row_count: Option<i64>,
    pub status: HistoryStatus,
    pub error: Option<String>,
}

/// design §3.7 `saved_query` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedQuery {
    pub id: String,
    pub folder_id: Option<String>,
    pub profile_id: Option<String>,
    pub name: String,
    pub text: String,
    pub params_json: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// design §3.7 `editor_tab` table — the whole open-tabs set is persisted
/// together via `Store::save_all_tabs` for crash-safe session restore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditorTab {
    pub id: String,
    pub profile_id: Option<String>,
    pub title: Option<String>,
    pub text: String,
    pub cursor_pos: Option<i64>,
    pub sort_order: i64,
    pub updated_at: i64,
}
