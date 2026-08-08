//! Resolved config paths (`datagrep doctor` prints these). No `dirs`/`directories`
//! crate — outside the ticket's allowed dependency list — so this is a small
//! hand-rolled XDG-ish resolver. Nothing here touches the filesystem; it only
//! computes a path. `datagrep-profiles::Store::open` is itself lazy (module docs:
//! "opened lazily off the startup path"), so calling this at startup is free.

use std::path::PathBuf;

/// Env var that overrides the config directory outright — mainly for tests,
/// so they never touch a developer's real `~/.config/datagrep`.
pub const CONFIG_DIR_ENV: &str = "DATAGREP_CONFIG_DIR";

/// Where `datagrep` keeps its one SQLite file (profiles, folders, tunnels,
/// query history). `$DATAGREP_CONFIG_DIR`, then `$XDG_CONFIG_HOME/datagrep`,
/// then `$HOME/.config/datagrep` (`%APPDATA%\datagrep` on Windows), then `./.datagrep` as a
/// last resort so the CLI still works somewhere writable.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(CONFIG_DIR_ENV) {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("datagrep");
        }
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("datagrep");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".config").join("datagrep");
        }
    }
    PathBuf::from(".datagrep")
}

/// The one SQLite file `datagrep-profiles::Store` opens: profiles, folders,
/// tunnels, and query history all live here.
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
        unsafe { std::env::set_var(CONFIG_DIR_ENV, "/tmp/datagrep-test-config") };
        assert_eq!(config_dir(), PathBuf::from("/tmp/datagrep-test-config"));
        assert_eq!(
            profiles_db_path(),
            PathBuf::from("/tmp/datagrep-test-config/profiles.db")
        );
        unsafe { std::env::remove_var(CONFIG_DIR_ENV) };
    }
}
