//! Resolved config paths (`dbx doctor` prints these). No `dirs`/`directories`
//! crate — outside the ticket's allowed dependency list — so this is a small
//! hand-rolled XDG-ish resolver. Nothing here touches the filesystem; it only
//! computes a path. `dbx-profiles::Store::open` is itself lazy (module docs:
//! "opened lazily off the startup path"), so calling this at startup is free.

use std::path::PathBuf;

/// Env var that overrides the config directory outright — mainly for tests,
/// so they never touch a developer's real `~/.config/dbx`.
pub const CONFIG_DIR_ENV: &str = "DBX_CONFIG_DIR";

/// Where `dbx` keeps its one SQLite file (profiles, folders, tunnels, query
/// history — design §3.7). `$DBX_CONFIG_DIR`, then `$XDG_CONFIG_HOME/dbx`,
/// then `$HOME/.config/dbx` (`%APPDATA%\dbx` on Windows), then `./.dbx` as a
/// last resort so the CLI still works somewhere writable.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(CONFIG_DIR_ENV) {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("dbx");
        }
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("dbx");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".config").join("dbx");
        }
    }
    PathBuf::from(".dbx")
}

/// The one SQLite file `dbx-profiles::Store` opens: profiles, folders,
/// tunnels, and query history all live here (design §3.7).
pub fn profiles_db_path() -> PathBuf {
    config_dir().join("profiles.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        // SAFETY: test-only env mutation, single-threaded within this test
        // (no other test in this module touches these vars); restored before
        // returning.
        unsafe { std::env::set_var(CONFIG_DIR_ENV, "/tmp/dbx-test-config") };
        assert_eq!(config_dir(), PathBuf::from("/tmp/dbx-test-config"));
        assert_eq!(
            profiles_db_path(),
            PathBuf::from("/tmp/dbx-test-config/profiles.db")
        );
        unsafe { std::env::remove_var(CONFIG_DIR_ENV) };
    }
}
