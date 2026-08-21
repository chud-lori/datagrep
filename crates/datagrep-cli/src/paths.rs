use std::path::PathBuf;

pub const CONFIG_DIR_ENV: &str = "DATAGREP_CONFIG_DIR";

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

pub fn profiles_db_path() -> PathBuf {
    config_dir().join("profiles.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        // SAFETY: test-only env mutation, restored before returning.
        unsafe { std::env::set_var(CONFIG_DIR_ENV, "/tmp/datagrep-test-config") };
        assert_eq!(config_dir(), PathBuf::from("/tmp/datagrep-test-config"));
        assert_eq!(
            profiles_db_path(),
            PathBuf::from("/tmp/datagrep-test-config/profiles.db")
        );
        unsafe { std::env::remove_var(CONFIG_DIR_ENV) };
    }
}
