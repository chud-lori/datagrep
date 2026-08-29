use std::cell::Cell;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use serde::Deserialize;

use crate::settings;

const MANIFEST_URL: &str = "https://chud-lori.github.io/datagrep/latest.json";
pub const RELEASES_URL: &str = "https://github.com/chud-lori/datagrep/releases";

/// Shape of `latest.json` (docs/latest.json in the repo). Extra keys ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default, rename = "release_url")]
    pub release_url: String,
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use gtk::subclass::prelude::*;
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct UpdateCheck {
        pub did_check_this_launch: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for UpdateCheck {
        const NAME: &'static str = "DgUpdateCheck";
        type Type = super::UpdateCheck;
    }

    impl ObjectImpl for UpdateCheck {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("update-available")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    // checkNow()'s outcome, including "nothing newer" and "check failed".
                    Signal::builder("check-finished")
                        .param_types([bool::static_type(), bool::static_type()])
                        .build(),
                ]
            })
        }
    }
}

glib::wrapper! {
    pub struct UpdateCheck(ObjectSubclass<imp::UpdateCheck>);
}

impl Default for UpdateCheck {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateCheck {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn current_version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    pub fn check_on_launch_enabled() -> bool {
        settings::read_bool(settings::UPDATE_CHECK_ON_LAUNCH, true)
    }

    pub fn set_check_on_launch_enabled(enabled: bool) {
        settings::write_bool(settings::UPDATE_CHECK_ON_LAUNCH, enabled);
    }

    /// Once per launch, never on a timer; silent on any failure.
    pub fn check_on_launch_if_enabled(&self) {
        let imp = self.imp();
        if !Self::check_on_launch_enabled() || imp.did_check_this_launch.get() {
            return;
        }
        imp.did_check_this_launch.set(true);
        self.fetch(false);
    }

    pub fn check_now(&self) {
        self.fetch(true);
    }

    /// Suppresses the launch notice for exactly this version.
    pub fn skip(version: &str) {
        settings::write(settings::UPDATE_SKIPPED_VERSION, version);
    }

    fn fetch(&self, user_initiated: bool) {
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(fetch_manifest());
        });
        let check = self.downgrade();
        glib::spawn_future_local(async move {
            let Ok(manifest) = rx.recv().await else {
                return;
            };
            if let Some(check) = check.upgrade() {
                check.handle(manifest, user_initiated);
            }
        });
    }

    fn handle(&self, manifest: Option<Manifest>, user_initiated: bool) {
        let Some(manifest) = manifest else {
            // Silence on any launch-check failure; only an asked-for check reports.
            if user_initiated {
                self.emit_by_name::<()>("check-finished", &[&false, &true]);
            }
            return;
        };
        let newer = is_newer(&manifest.version, Self::current_version());
        let announce = || {
            self.emit_by_name::<()>(
                "update-available",
                &[&manifest.version, &manifest.release_url],
            );
        };
        if !user_initiated {
            let skipped = settings::read(settings::UPDATE_SKIPPED_VERSION).unwrap_or_default();
            if newer && normalize(&skipped) != normalize(&manifest.version) {
                announce();
            }
            return;
        }
        if newer {
            announce();
        }
        self.emit_by_name::<()>("check-finished", &[&newer, &false]);
    }

    pub fn connect_update_available<F: Fn(&Self, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("update-available", false, move |values| {
            let check = values[0]
                .get::<Self>()
                .expect("the signal carries the check");
            let version = values[1].get::<String>().unwrap_or_default();
            let url = values[2].get::<String>().unwrap_or_default();
            f(&check, &version, &url);
            None
        })
    }

    pub fn connect_check_finished<F: Fn(&Self, bool, bool) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("check-finished", false, move |values| {
            let check = values[0]
                .get::<Self>()
                .expect("the signal carries the check");
            let newer = values[1].get::<bool>().unwrap_or_default();
            let failed = values[2].get::<bool>().unwrap_or_default();
            f(&check, newer, failed);
            None
        })
    }
}

// One GET, short timeout, nothing cached, nothing persisted — informs and links only.
fn fetch_manifest() -> Option<Manifest> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(format!("datagrep/{}", UpdateCheck::current_version()))
            .build()
            .ok()?;
        let response = client
            .get(MANIFEST_URL)
            .header("Accept", "application/json")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let manifest: Manifest = response.json().await.ok()?;
        (!manifest.version.is_empty() && !manifest.tag.is_empty()).then_some(manifest)
    })
}

pub fn normalize(v: &str) -> &str {
    v.strip_prefix('v').unwrap_or(v)
}

pub fn is_newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| {
        let mut out = [0u64; 3];
        for (i, part) in normalize(s).splitn(3, '.').enumerate() {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            out[i] = digits.parse().unwrap_or(0);
        }
        out
    };
    parse(a) > parse(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_matches_the_other_two_frontends() {
        assert!(is_newer("0.5.0", "0.4.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.4.1", "0.4.0"));
        assert!(!is_newer("0.4.0", "0.4.0"));
        assert!(!is_newer("0.4.0", "v0.4.0"));
        assert!(!is_newer("0.3.9", "0.4.0"));
        assert!(is_newer("0.5.0-rc1", "0.4.9"));
    }

    #[test]
    fn normalize_strips_only_a_leading_v() {
        assert_eq!(normalize("v0.4.0"), "0.4.0");
        assert_eq!(normalize("0.4.0"), "0.4.0");
    }
}
