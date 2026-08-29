use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use datagrep_api::safety::SafetyLevel;
use datagrep_api::ConnectionConfig;
use serde::{Deserialize, Serialize};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", now_ms(), n, std::process::id())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub sort_order: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub folder_id: Option<String>,
    pub name: String,
    pub driver_id: String,
    pub config: ConnectionConfig,
    pub secret_ref: Option<String>,
    pub tunnel_id: Option<String>,
    pub color: Option<String>,
    pub read_only: bool,
    #[serde(default)]
    pub safety: SafetyLevel,
    pub auto_limit: Option<i64>,
    pub idle_timeout_s: Option<i64>,
    pub last_used_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

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
