use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use glib::prelude::*;
use glib::subclass::prelude::*;
use serde::{Deserialize, Serialize};

use crate::model::QueryStatus;

const DEDUPE_WINDOW_MS: i64 = 120 * 1000;
const FLUSH_DELAY_MS: u32 = 600;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Outcome {
    #[default]
    Ok,
    Error,
    Cancelled,
}

impl From<String> for Outcome {
    fn from(value: String) -> Self {
        match value.as_str() {
            "error" => Outcome::Error,
            "cancelled" => Outcome::Cancelled,
            _ => Outcome::Ok,
        }
    }
}

impl From<Outcome> for String {
    fn from(value: Outcome) -> Self {
        value.key().to_owned()
    }
}

impl Outcome {
    /// What goes on disk, shared with the Qt and macOS stores.
    pub fn key(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error => "error",
            Outcome::Cancelled => "cancelled",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error => "failed",
            Outcome::Cancelled => "cancelled",
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            Outcome::Ok => "success",
            Outcome::Error => "error",
            Outcome::Cancelled => "warning",
        }
    }
}

/// One executed statement, its fields in the alphabetical order both other stores write.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_rows: Option<u64>,
    #[serde(default)]
    pub connection: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub engine: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
    #[serde(default)]
    pub run_count: u32,
    pub sql: String,
    #[serde(default)]
    pub started_at_ms: i64,
    #[serde(default)]
    pub text_hash: String,
}

impl HistoryEntry {
    fn completed(&mut self) {
        if self.id.is_empty() {
            self.id = glib::uuid_string_random().to_string();
        }
        if self.text_hash.is_empty() {
            self.text_hash = hash_text(&self.sql);
        }
        self.run_count = self.run_count.max(1);
    }

    /// Day bucket in the user's own time zone — "Today" means the day they had.
    pub fn day_key(&self) -> String {
        day_key(self.started_at_ms)
    }

    /// Whitespace collapsed for the list row; the full text stays in `sql`.
    pub fn one_line(&self) -> String {
        let mut out = String::with_capacity(self.sql.len());
        for line in self.sql.lines() {
            let trimmed = line.replace('\t', " ");
            let trimmed = trimmed.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(trimmed);
        }
        if out.is_empty() {
            self.sql.trim().to_owned()
        } else {
            out
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub max_entries: u32,
    pub max_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_days: 180,
        }
    }
}

impl Retention {
    pub fn clamped(max_entries: u32, max_days: u32) -> Self {
        Self {
            max_entries: max_entries.max(100),
            max_days: max_days.max(1),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DateRange {
    Day,
    Week,
    Month,
    #[default]
    All,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryFilter {
    pub text: String,
    /// Empty = every connection. History is never scoped for the user.
    pub connection: String,
    pub range: DateRange,
    pub outcome: Option<Outcome>,
}

impl HistoryFilter {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.connection.is_empty()
            && self.range == DateRange::All
            && self.outcome.is_none()
    }

    /// Terms split and lowercased once, not once per entry.
    pub fn prepare(&self, now_ms: i64) -> PreparedFilter {
        PreparedFilter {
            terms: self
                .text
                .split_whitespace()
                .map(str::to_lowercase)
                .collect(),
            connection: self.connection.clone(),
            outcome: self.outcome,
            earliest_ms: match self.range {
                DateRange::All => None,
                DateRange::Day => Some(start_of_today_ms(now_ms)),
                DateRange::Week => Some(now_ms - 7 * 86_400_000),
                DateRange::Month => Some(now_ms - 30 * 86_400_000),
            },
        }
    }
}

pub struct PreparedFilter {
    terms: Vec<String>,
    connection: String,
    outcome: Option<Outcome>,
    earliest_ms: Option<i64>,
}

impl PreparedFilter {
    pub fn matches(&self, entry: &HistoryEntry) -> bool {
        if !self.connection.is_empty() && entry.connection != self.connection {
            return false;
        }
        if self.outcome.is_some_and(|o| o != entry.outcome) {
            return false;
        }
        if self.earliest_ms.is_some_and(|ms| entry.started_at_ms < ms) {
            return false;
        }
        self.terms
            .iter()
            .all(|term| contains_ci(&entry.sql, term) || contains_ci(&entry.error, term))
    }
}

// SQL is overwhelmingly ASCII; the general path allocates only when it is not.
fn contains_ci(haystack: &str, needle_lower: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle_lower.as_bytes());
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    if h.is_ascii() {
        return h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
    }
    haystack.to_lowercase().contains(needle_lower)
}

pub fn normalise(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut last_was_space = false;
    for ch in sql.chars() {
        if ch.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    while out.ends_with(' ') || out.ends_with(';') {
        out.pop();
    }
    out
}

/// FNV-1a over the normalised statement — the same digits the other two stores write.
pub fn hash_text(sql: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalise(sql).as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:x}")
}

fn local(ms: i64) -> Option<glib::DateTime> {
    glib::DateTime::from_unix_local(ms.div_euclid(1000)).ok()
}

pub fn day_key(ms: i64) -> String {
    local(ms)
        .and_then(|dt| dt.format("%Y-%m-%d").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn start_of_today_ms(now_ms: i64) -> i64 {
    let Some(now) = local(now_ms) else {
        return now_ms;
    };
    glib::DateTime::new(
        &now.timezone(),
        now.year(),
        now.month(),
        now.day_of_month(),
        0,
        0,
        0.0,
    )
    .map(|start| start.to_unix() * 1000)
    .unwrap_or(now_ms)
}

fn cutoff_day_key(days: u32, now_ms: i64) -> String {
    day_key(now_ms - i64::from(days.max(1) - 1) * 86_400_000)
}

pub fn now_ms() -> i64 {
    glib::real_time() / 1000
}

struct PendingRun {
    sql: String,
    connection: String,
    engine: String,
    started_at_ms: i64,
    recorded: bool,
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct HistoryStore {
        pub directory: RefCell<PathBuf>,
        /// Newest first, the order both other stores keep in memory.
        pub entries: RefCell<Vec<HistoryEntry>>,
        pub dirty_days: RefCell<HashSet<String>>,
        pub retention: Cell<Retention>,
        pub(super) pending: RefCell<Option<PendingRun>>,
        pub flush_queued: Cell<bool>,
        pub loaded: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for HistoryStore {
        const NAME: &'static str = "DgHistoryStore";
        type Type = super::HistoryStore;
    }

    impl ObjectImpl for HistoryStore {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| vec![Signal::builder("changed").build()])
        }

        // A statement run moments before quit must not be lost to the debounce.
        fn dispose(&self) {
            self.obj().flush();
        }
    }
}

glib::wrapper! {
    pub struct HistoryStore(ObjectSubclass<imp::HistoryStore>);
}

impl HistoryStore {
    pub fn new(directory: PathBuf) -> Self {
        let _ = fs::create_dir_all(&directory);
        let store: Self = glib::Object::new();
        let imp = store.imp();
        imp.retention.set(read_retention(&directory));
        *imp.directory.borrow_mut() = directory;
        store
    }

    pub fn directory(&self) -> PathBuf {
        self.imp().directory.borrow().clone()
    }

    pub fn retention(&self) -> Retention {
        self.imp().retention.get()
    }

    pub fn set_retention(&self, retention: Retention) {
        let imp = self.imp();
        let retention = Retention::clamped(retention.max_entries, retention.max_days);
        imp.retention.set(retention);
        write_retention(retention, &imp.directory.borrow());
        self.prune();
        self.schedule_flush();
        self.emit_by_name::<()>("changed", &[]);
    }

    /// Borrowed, newest first: the panel filters in place rather than cloning the log.
    pub fn with_entries<R>(&self, f: impl FnOnce(&[HistoryEntry]) -> R) -> R {
        f(&self.imp().entries.borrow())
    }

    pub fn entry(&self, id: &str) -> Option<HistoryEntry> {
        self.with_entries(|entries| entries.iter().find(|e| e.id == id).cloned())
    }

    /// Connection names that actually appear in history, for the filter list.
    pub fn connections(&self) -> Vec<String> {
        self.with_entries(|entries| {
            let mut names: Vec<String> = entries
                .iter()
                .filter(|e| !e.connection.is_empty())
                .map(|e| e.connection.clone())
                .collect();
            names.sort();
            names.dedup();
            names
        })
    }

    pub fn load(&self) {
        let imp = self.imp();
        if imp.loaded.replace(true) {
            return;
        }
        let directory = imp.directory.borrow().clone();
        let retention = imp.retention.get();
        let cutoff = cutoff_day_key(retention.max_days, now_ms());

        let mut files = day_files(&directory);
        files.sort_by(|a, b| b.0.cmp(&a.0));
        let mut entries: Vec<HistoryEntry> = Vec::new();
        for (key, path) in files {
            if key < cutoff {
                let _ = fs::remove_file(&path);
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let Ok(mut entry) = serde_json::from_str::<HistoryEntry>(line) else {
                    continue;
                };
                if entry.sql.trim().is_empty() {
                    continue;
                }
                entry.completed();
                entries.push(entry);
            }
            if entries.len() >= retention.max_entries as usize {
                break;
            }
        }
        entries.sort_by(|a, b| b.started_at_ms.cmp(&a.started_at_ms));
        *imp.entries.borrow_mut() = entries;
        self.prune();
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn record(&self, mut entry: HistoryEntry) {
        if entry.sql.trim().is_empty() {
            return;
        }
        entry.completed();
        let imp = self.imp();
        {
            let mut entries = imp.entries.borrow_mut();
            let mut dirty = imp.dirty_days.borrow_mut();
            let twin = entries.iter().position(|e| {
                e.text_hash == entry.text_hash
                    && e.connection == entry.connection
                    && e.outcome == entry.outcome
                    && e.error == entry.error
                    && (entry.started_at_ms - e.started_at_ms).abs() <= DEDUPE_WINDOW_MS
            });
            match twin {
                Some(index) => {
                    let mut merged = entries.remove(index);
                    dirty.insert(merged.day_key()); // it may be leaving this day
                    merged.started_at_ms = entry.started_at_ms;
                    merged.duration_ms = entry.duration_ms;
                    merged.row_count = entry.row_count;
                    merged.affected_rows = entry.affected_rows;
                    merged.run_count += 1;
                    dirty.insert(merged.day_key());
                    entries.insert(0, merged);
                }
                None => {
                    dirty.insert(entry.day_key());
                    entries.insert(0, entry);
                }
            }
            entries.sort_by(|a, b| b.started_at_ms.cmp(&a.started_at_ms));
        }
        self.prune();
        self.schedule_flush();
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn remove(&self, id: &str) {
        let imp = self.imp();
        {
            let mut entries = imp.entries.borrow_mut();
            let Some(index) = entries.iter().position(|e| e.id == id) else {
                return;
            };
            imp.dirty_days.borrow_mut().insert(entries[index].day_key());
            entries.remove(index);
        }
        self.schedule_flush();
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn clear(&self, connection: Option<&str>) {
        let imp = self.imp();
        {
            let mut entries = imp.entries.borrow_mut();
            let mut dirty = imp.dirty_days.borrow_mut();
            entries.retain(|entry| {
                let goes = connection.map_or(true, |name| entry.connection == name);
                if goes {
                    dirty.insert(entry.day_key());
                }
                !goes
            });
        }
        self.schedule_flush();
        self.emit_by_name::<()>("changed", &[]);
    }

    /// Recorded before the engine is asked, so a refused run has a pending entry to fail into.
    pub fn execution_started(&self, sql: &str, connection: &str, engine: &str) {
        let mut pending = self.imp().pending.borrow_mut();
        if sql.trim().is_empty() {
            *pending = None;
            return;
        }
        *pending = Some(PendingRun {
            sql: sql.to_owned(),
            connection: connection.to_owned(),
            engine: engine.to_owned(),
            started_at_ms: now_ms(),
            recorded: false,
        });
    }

    pub fn execution_progressed(&self, status: &QueryStatus) {
        if !status.state.is_terminal() {
            return;
        }
        let outcome = match status.state {
            crate::model::QueryState::Failed => Outcome::Error,
            crate::model::QueryState::Cancelled => Outcome::Cancelled,
            _ => Outcome::Ok,
        };
        let error = match outcome {
            Outcome::Error => status.error.clone().unwrap_or_default(),
            _ => String::new(),
        };
        self.commit_pending(
            outcome,
            status.elapsed_ms,
            Some(status.rows_loaded),
            status.affected_rows,
            error,
        );
    }

    /// The run never got a query handle — a connect failure, or a rejected statement.
    pub fn execution_failed_to_start(&self, message: &str) {
        let elapsed = self
            .imp()
            .pending
            .borrow()
            .as_ref()
            .map(|pending| (now_ms() - pending.started_at_ms).max(0) as u64)
            .unwrap_or_default();
        self.commit_pending(Outcome::Error, elapsed, None, None, message.to_owned());
    }

    fn commit_pending(
        &self,
        outcome: Outcome,
        duration_ms: u64,
        row_count: Option<u64>,
        affected_rows: Option<u64>,
        error: String,
    ) {
        let entry = {
            let mut pending = self.imp().pending.borrow_mut();
            let Some(run) = pending.as_mut().filter(|run| !run.recorded) else {
                return;
            };
            run.recorded = true;
            HistoryEntry {
                connection: run.connection.clone(),
                duration_ms,
                engine: run.engine.clone(),
                error,
                outcome,
                row_count,
                affected_rows,
                sql: run.sql.clone(),
                started_at_ms: run.started_at_ms,
                ..HistoryEntry::default()
            }
        };
        self.record(entry);
    }

    /// Retention, applied: entry count first, then age.
    fn prune(&self) {
        let imp = self.imp();
        let retention = imp.retention.get();
        let cutoff = cutoff_day_key(retention.max_days, now_ms());
        let mut entries = imp.entries.borrow_mut();
        let mut dirty = imp.dirty_days.borrow_mut();
        let over = entries.len().min(retention.max_entries as usize);
        let stale = entries[..over]
            .iter()
            .position(|e| e.day_key() < cutoff)
            .unwrap_or(over);
        for entry in &entries[stale..] {
            dirty.insert(entry.day_key());
        }
        entries.truncate(stale);
    }

    fn schedule_flush(&self) {
        let imp = self.imp();
        if imp.flush_queued.replace(true) {
            return;
        }
        let store = self.downgrade();
        let delay = std::time::Duration::from_millis(FLUSH_DELAY_MS.into());
        glib::timeout_add_local_once(delay, move || {
            if let Some(store) = store.upgrade() {
                store.flush();
            }
        });
    }

    /// Rewrites only the days that changed; a day left with nothing loses its file.
    pub fn flush(&self) {
        let imp = self.imp();
        imp.flush_queued.set(false);
        let dirty = std::mem::take(&mut *imp.dirty_days.borrow_mut());
        let directory = imp.directory.borrow().clone();
        let entries = imp.entries.borrow();

        let mut by_day: HashMap<String, Vec<&HistoryEntry>> = HashMap::new();
        for entry in entries.iter() {
            by_day.entry(entry.day_key()).or_default().push(entry);
        }
        for day in &dirty {
            let path = directory.join(format!("{day}.jsonl"));
            match by_day.get(day) {
                Some(day_entries) if !day_entries.is_empty() => {
                    let mut text = String::new();
                    // Oldest first inside a file, so it reads naturally with `tail`.
                    for entry in day_entries.iter().rev() {
                        let Ok(line) = serde_json::to_string(entry) else {
                            continue;
                        };
                        text.push_str(&line);
                        text.push('\n');
                    }
                    write_atomically(&path, text.as_bytes());
                }
                _ => {
                    let _ = fs::remove_file(&path);
                }
            }
        }

        let cutoff = cutoff_day_key(imp.retention.get().max_days, now_ms());
        for (key, path) in day_files(&directory) {
            if key < cutoff && !by_day.contains_key(&key) {
                let _ = fs::remove_file(&path);
            }
        }
    }

    pub fn connect_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("changed", false, move |values| {
            let store = values[0]
                .get::<Self>()
                .expect("the signal carries the store");
            f(&store);
            None
        })
    }
}

fn day_files(directory: &Path) -> Vec<(String, PathBuf)> {
    let Ok(dir) = fs::read_dir(directory) else {
        return Vec::new();
    };
    dir.filter_map(|entry| {
        let path = entry.ok()?.path();
        let key = path
            .file_name()?
            .to_str()?
            .strip_suffix(".jsonl")?
            .to_owned();
        Some((key, path))
    })
    .collect()
}

fn write_atomically(path: &Path, bytes: &[u8]) {
    let temp = path.with_extension("jsonl.tmp");
    if fs::write(&temp, bytes).is_ok() {
        let _ = fs::rename(&temp, path);
    }
}

fn retention_path(directory: &Path) -> PathBuf {
    directory.join("retention.json")
}

fn read_retention(directory: &Path) -> Retention {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Stored {
        #[serde(default)]
        max_entries: Option<u32>,
        #[serde(default)]
        max_days: Option<u32>,
    }
    let Ok(text) = fs::read_to_string(retention_path(directory)) else {
        return Retention::default();
    };
    let stored: Stored = match serde_json::from_str(&text) {
        Ok(stored) => stored,
        Err(_) => return Retention::default(),
    };
    let defaults = Retention::default();
    Retention::clamped(
        stored.max_entries.unwrap_or(defaults.max_entries),
        stored.max_days.unwrap_or(defaults.max_days),
    )
}

// Hand-formatted to match the indented, key-sorted JSON Qt and Swift write.
fn write_retention(retention: Retention, directory: &Path) {
    let text = format!(
        "{{\n    \"maxDays\": {},\n    \"maxEntries\": {}\n}}\n",
        retention.max_days, retention.max_entries
    );
    let _ = fs::write(retention_path(directory), text);
}
