use std::path::{Path, PathBuf};

use gtk::glib;

pub const APPEARANCE: &str = "appearance";
pub const UPDATE_CHECK_ON_LAUNCH: &str = "updateCheckOnLaunch";
pub const UPDATE_SKIPPED_VERSION: &str = "updateSkippedVersion";

const GROUP: &str = "General";

/// The Qt UI's QSettings file, so both Linux front-ends share one set of preferences.
fn conf_path() -> PathBuf {
    glib::user_config_dir()
        .join("datagrep")
        .join("datagrep.conf")
}

pub fn read(key: &str) -> Option<String> {
    read_from(&conf_path(), key)
}

pub fn write(key: &str, value: &str) {
    write_to(&conf_path(), key, value);
}

pub fn read_bool(key: &str, default: bool) -> bool {
    match read(key).as_deref() {
        Some("true") => true,
        Some("false") => false,
        _ => default,
    }
}

pub fn write_bool(key: &str, value: bool) {
    write(key, if value { "true" } else { "false" });
}

fn read_from(path: &Path, key: &str) -> Option<String> {
    let file = glib::KeyFile::new();
    file.load_from_file(path, glib::KeyFileFlags::NONE).ok()?;
    file.string(GROUP, key).ok().map(Into::into)
}

// Load-then-save keeps every key the Qt build wrote; comments QSettings never writes anyway.
fn write_to(path: &Path, key: &str, value: &str) {
    let file = glib::KeyFile::new();
    let _ = file.load_from_file(path, glib::KeyFileFlags::NONE);
    file.set_string(GROUP, key, value);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = file.save_to_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_conf() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dg-settings-{}", glib::random_int()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("datagrep.conf")
    }

    #[test]
    fn reads_what_the_qt_build_writes() {
        let path = temp_conf();
        std::fs::write(
            &path,
            "[General]\nappearance=dark\nupdateCheckOnLaunch=false\n",
        )
        .unwrap();
        assert_eq!(read_from(&path, APPEARANCE).as_deref(), Some("dark"));
        assert_eq!(read_from(&path, UPDATE_SKIPPED_VERSION), None);
    }

    #[test]
    fn writing_one_key_keeps_the_others() {
        let path = temp_conf();
        std::fs::write(&path, "[General]\nappearance=light\n").unwrap();
        write_to(&path, UPDATE_SKIPPED_VERSION, "0.5.0");
        assert_eq!(read_from(&path, APPEARANCE).as_deref(), Some("light"));
        assert_eq!(
            read_from(&path, UPDATE_SKIPPED_VERSION).as_deref(),
            Some("0.5.0")
        );
    }

    #[test]
    fn a_missing_file_reads_as_nothing() {
        assert_eq!(
            read_from(Path::new("/nonexistent/x.conf"), APPEARANCE),
            None
        );
    }
}
