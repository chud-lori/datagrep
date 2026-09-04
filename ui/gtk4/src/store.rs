use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One tab as persisted: a `.sql` file plus a JSON sidecar, byte-compatible with the macOS and Qt stores.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedQueryRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What this tab is about when it has no name — a browsed object's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, rename = "cursorLocation")]
    pub cursor_location: i64,
    #[serde(default, rename = "cursorLength")]
    pub cursor_length: i64,
    #[serde(default, rename = "isDirty")]
    pub is_dirty: bool,
}

impl SavedQueryRecord {
    pub fn scratch() -> Self {
        Self {
            id: glib::uuid_string_random().to_string(),
            ..Self::default()
        }
    }

    pub fn is_scratch(&self) -> bool {
        self.name.as_deref().map_or(true, str::is_empty)
    }

    /// Basename shared by the `.sql` and the `.json`: a slug, or `scratch-<id>` for unnamed tabs.
    pub fn basename(&self) -> String {
        match self.name.as_deref().map(slug) {
            Some(s) if !s.is_empty() => s,
            _ => format!("scratch-{}", self.id),
        }
    }
}

/// One global order and ONE active tab; `active_connection` only seeds what a NEW tab binds to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EditorSession {
    pub order: Vec<String>,
    #[serde(rename = "activeID", skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
    #[serde(rename = "activeConnection", skip_serializing_if = "Option::is_none")]
    pub active_connection: Option<String>,
}

/// Read shape only — older builds wrote a per-connection `activeByConnection` map.
#[derive(Deserialize)]
struct SessionOnDisk {
    #[serde(default)]
    order: Vec<String>,
    #[serde(default, rename = "activeID")]
    active_id: Option<String>,
    #[serde(default, rename = "activeConnection")]
    active_connection: Option<String>,
    #[serde(default, rename = "activeByConnection")]
    active_by_connection: HashMap<String, String>,
}

impl From<SessionOnDisk> for EditorSession {
    fn from(d: SessionOnDisk) -> Self {
        let active_id = d.active_id.or_else(|| {
            let key = d.active_connection.clone().unwrap_or_default();
            d.active_by_connection
                .get(&key)
                .or_else(|| d.active_by_connection.values().next())
                .cloned()
        });
        Self {
            order: d.order,
            active_id,
            active_connection: d.active_connection,
        }
    }
}

/// `$DATAGREP_CONFIG_DIR`, else the app-data dir the Qt UI uses, so both Linux frontends share one `tabs/`.
pub fn support_dir() -> PathBuf {
    match std::env::var("DATAGREP_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => expand_tilde(&dir, &glib::home_dir()),
        _ => glib::user_data_dir().join("datagrep"),
    }
}

/// A launcher-set var arrives with `~` unexpanded; macOS and Qt expand it, so this must too.
fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

pub struct LoadedTab {
    pub record: SavedQueryRecord,
    pub text: String,
}

pub struct Loaded {
    pub tabs: Vec<LoadedTab>,
    pub session: EditorSession,
}

/// Pure file I/O, one file pair per tab (a half-written blob would lose every tab), best-effort like its peers.
pub struct SavedQueryStore {
    directory: PathBuf,
}

impl SavedQueryStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        let directory = directory.into();
        let _ = fs::create_dir_all(&directory);
        Self { directory }
    }

    pub fn default_directory() -> PathBuf {
        support_dir().join("tabs")
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn sql_path(&self, record: &SavedQueryRecord) -> PathBuf {
        self.directory.join(record.basename() + ".sql")
    }

    fn sidecar_path(&self, record: &SavedQueryRecord) -> PathBuf {
        self.directory.join(record.basename() + ".json")
    }

    fn session_path(&self) -> PathBuf {
        self.directory.join("session.json")
    }

    /// Atomic per file: a crash mid-write keeps the previous version, never a truncated one.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) {
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, bytes).is_ok() {
            let _ = fs::rename(&tmp, path);
        }
    }

    pub fn save(&self, record: &SavedQueryRecord, text: &str) {
        self.write_atomic(&self.sql_path(record), text.as_bytes());
        if let Ok(json) = serde_json::to_vec_pretty(record) {
            self.write_atomic(&self.sidecar_path(record), &json);
        }
    }

    pub fn delete(&self, record: &SavedQueryRecord) {
        let _ = fs::remove_file(self.sql_path(record));
        let _ = fs::remove_file(self.sidecar_path(record));
    }

    pub fn save_session(&self, session: &EditorSession) {
        if let Ok(json) = serde_json::to_vec_pretty(session) {
            self.write_atomic(&self.session_path(), &json);
        }
    }

    pub fn load_session(&self) -> EditorSession {
        fs::read(self.session_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SessionOnDisk>(&bytes).ok())
            .map(EditorSession::from)
            .unwrap_or_default()
    }

    pub fn text(&self, record: &SavedQueryRecord) -> Option<String> {
        fs::read_to_string(self.sql_path(record)).ok()
    }

    /// Every tab on disk; a bare `.sql` with no sidecar has no id, so it is ignored.
    pub fn all_records(&self) -> Vec<SavedQueryRecord> {
        let mut records: Vec<SavedQueryRecord> = fs::read_dir(&self.directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".json") && name != "session.json"
            })
            .filter_map(|e| fs::read(e.path()).ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect();
        records.sort_by(|a, b| {
            (a.name.as_deref().unwrap_or(""), &a.id).cmp(&(b.name.as_deref().unwrap_or(""), &b.id))
        });
        records
    }

    /// Session order; forgotten scratch tabs are appended (unsaved work has nowhere else), forgotten named ones stay closed.
    pub fn load(&self) -> Loaded {
        let mut session = self.load_session();
        let mut by_id: HashMap<String, LoadedTab> = HashMap::new();
        for record in self.all_records() {
            let Some(text) = self.text(&record) else {
                continue;
            };
            by_id.insert(record.id.clone(), LoadedTab { record, text });
        }

        let mut tabs = Vec::new();
        let mut seen = HashSet::new();
        for id in &session.order {
            if let Some(tab) = by_id.remove(id) {
                if seen.insert(id.clone()) {
                    tabs.push(tab);
                }
            }
        }
        let mut leftovers: Vec<LoadedTab> = by_id
            .into_values()
            .filter(|t| t.record.is_scratch())
            .collect();
        leftovers.sort_by(|a, b| a.record.id.cmp(&b.record.id));
        for tab in leftovers {
            seen.insert(tab.record.id.clone());
            tabs.push(tab);
        }

        if session
            .active_id
            .as_ref()
            .is_some_and(|id| !seen.contains(id))
        {
            session.active_id = None;
        }
        Loaded { tabs, session }
    }
}

/// Filesystem-safe lower-kebab, matching the macOS and Qt slugs.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut last_was_dash = false;
    for ch in name.to_lowercase().chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !out.is_empty() {
            out.push('-');
            last_was_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> SavedQueryStore {
        let dir = std::env::temp_dir().join(format!(
            "datagrep-tabs-{tag}-{}",
            glib::uuid_string_random()
        ));
        SavedQueryStore::new(dir)
    }

    fn record(id: &str, name: Option<&str>) -> SavedQueryRecord {
        SavedQueryRecord {
            id: id.into(),
            name: name.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn slug_matches_the_other_frontends() {
        assert_eq!(slug("Monthly Revenue (v2)"), "monthly-revenue-v2");
        assert_eq!(slug("  --weird--  "), "weird");
        assert_eq!(slug("Ünïcode Näme"), "ünïcode-näme");
    }

    #[test]
    fn sidecar_keys_match_the_shared_format() {
        let json = serde_json::to_value(record("abc", Some("My Query"))).unwrap();
        let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(|s| &**s).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["cursorLength", "cursorLocation", "id", "isDirty", "name"]
        );
        let scratch = serde_json::to_value(record("abc", None)).unwrap();
        assert!(!scratch.as_object().unwrap().contains_key("name"));
        assert!(!scratch.as_object().unwrap().contains_key("subject"));
    }

    /// macOS writes `subject` on a browse tab; the shared store must not drop it.
    #[test]
    fn a_browse_tab_keeps_its_subject_through_the_sidecar() {
        let mut browsed = record("abc", None);
        browsed.subject = Some("users".into());
        let json = serde_json::to_string(&browsed).unwrap();
        assert!(json.contains(r#""subject":"users""#), "{json}");
        let back: SavedQueryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.subject.as_deref(), Some("users"));
    }

    #[test]
    fn round_trips_records_in_session_order() {
        let store = temp_store("order");
        store.save(&record("a", Some("Alpha")), "select 'a'");
        store.save(&record("b", None), "select 'b'");
        store.save_session(&EditorSession {
            order: vec!["b".into(), "a".into()],
            active_id: Some("b".into()),
            active_connection: None,
        });
        let loaded = store.load();
        let ids: Vec<&str> = loaded.tabs.iter().map(|t| &*t.record.id).collect();
        assert_eq!(ids, ["b", "a"]);
        assert_eq!(loaded.session.active_id.as_deref(), Some("b"));
        assert_eq!(loaded.tabs[0].text, "select 'b'");
    }

    #[test]
    fn a_forgotten_scratch_tab_reopens_but_a_named_one_stays_closed() {
        let store = temp_store("forgotten");
        store.save(&record("s1", None), "unsaved work");
        store.save(&record("n1", Some("Named")), "saved work");
        store.save_session(&EditorSession::default());
        let loaded = store.load();
        let ids: Vec<&str> = loaded.tabs.iter().map(|t| &*t.record.id).collect();
        assert_eq!(ids, ["s1"]);
        assert_eq!(store.all_records().len(), 2);
    }

    #[test]
    fn stale_active_id_is_dropped_not_kept() {
        let store = temp_store("stale");
        store.save(&record("a", None), "select 1");
        store.save_session(&EditorSession {
            order: vec!["a".into()],
            active_id: Some("gone".into()),
            active_connection: None,
        });
        assert_eq!(store.load().session.active_id, None);
    }

    #[test]
    fn legacy_per_connection_session_still_restores_an_active_tab() {
        let store = temp_store("legacy");
        store.save(&record("a", None), "select 1");
        let legacy = r#"{"order":["a"],"activeConnection":"dev","activeByConnection":{"dev":"a"}}"#;
        std::fs::write(store.directory().join("session.json"), legacy).unwrap();
        assert_eq!(store.load().session.active_id.as_deref(), Some("a"));
    }

    #[test]
    fn tilde_expansion_matches_the_qt_support_dir() {
        let home = Path::new("/home/me");
        assert_eq!(expand_tilde("~", home), PathBuf::from("/home/me"));
        assert_eq!(expand_tilde("~/x", home), PathBuf::from("/home/me/x"));
        assert_eq!(expand_tilde("/abs", home), PathBuf::from("/abs"));
    }
}
