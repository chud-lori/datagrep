use gtk::glib;

use crate::settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    System,
    Light,
    Dark,
}

impl Mode {
    /// The value strings the Qt build stores, so the shared conf reads back on both.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::System => "system",
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "light" => Mode::Light,
            "dark" => Mode::Dark,
            _ => Mode::System,
        }
    }

    fn scheme(self) -> adw::ColorScheme {
        match self {
            Mode::System => adw::ColorScheme::Default,
            Mode::Light => adw::ColorScheme::ForceLight,
            Mode::Dark => adw::ColorScheme::ForceDark,
        }
    }
}

pub fn mode() -> Mode {
    Mode::parse(&settings::read(settings::APPEARANCE).unwrap_or_default())
}

pub fn set_mode(mode: Mode) {
    settings::write(settings::APPEARANCE, mode.as_str());
    adw::StyleManager::default().set_color_scheme(mode.scheme());
}

/// Call once, after the display exists and before any window shows.
pub fn apply_stored() {
    adw::StyleManager::default().set_color_scheme(mode().scheme());
}

pub fn is_dark() -> bool {
    adw::StyleManager::default().is_dark()
}

/// Fires on every effective palette change: mode switches and desktop theme flips alike.
pub fn connect_changed<F: Fn(bool) + 'static>(f: F) -> glib::SignalHandlerId {
    adw::StyleManager::default().connect_dark_notify(move |manager| f(manager.is_dark()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stored_strings_round_trip() {
        for mode in [Mode::System, Mode::Light, Mode::Dark] {
            assert_eq!(Mode::parse(mode.as_str()), mode);
        }
        assert_eq!(Mode::parse("qt-wrote-something-new"), Mode::System);
    }
}
