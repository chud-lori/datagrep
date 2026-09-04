// Verification harness for the safety ladder: seeds gated profiles, drives the
// warn and typed-phrase ceremonies, and snapshots each surface to PNG.
use std::sync::Arc;

use adw::prelude::*;
use datagrep_gtk::ffi::Core;
use datagrep_gtk::ui::Window;
use datagrep_gtk::ConnectionDialog;

fn main() {
    let dir = std::env::var("PREVIEW_DIR").expect("PREVIEW_DIR");
    let app = adw::Application::builder()
        .application_id("io.github.chud_lori.datagrep.SafetyPreview")
        .build();
    app.connect_startup(|_| datagrep_gtk::ui::load_style());
    app.connect_activate(move |app| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
        let core = Arc::new(Core::open(&format!("{dir}/profiles.sqlite")).expect("core"));
        let seed = [
            (
                "demo",
                format!("sqlite://{dir}/demo.sqlite"),
                r#"{"safety":"warn_writes"}"#,
            ),
            (
                "vault",
                format!("sqlite://{dir}/vault.sqlite"),
                r#"{"safety":"auth_writes"}"#,
            ),
            (
                "analytics",
                "postgres://demo@localhost:5432/analytics".into(),
                r#"{"safety":"warn_all"}"#,
            ),
            (
                "prod-es",
                "elasticsearch://localhost:9200".into(),
                r#"{"safety":"auth_all","read_only":true}"#,
            ),
            ("scratch", "redis://localhost:6379/0".into(), r#"{}"#),
        ];
        for (name, url, options) in seed {
            let _ = core.add_profile_json(name, &url, options);
        }
        let window = Window::new(app, core.clone());
        window.present();
        assert!(window.select_connection("demo"), "demo is listed");

        let out = std::env::var("PREVIEW_PNG").expect("PREVIEW_PNG");
        let app = app.clone();
        let core2 = core.clone();
        let editor: std::rc::Rc<std::cell::RefCell<Option<ConnectionDialog>>> = Default::default();
        let (mut step, mut waited) = (0usize, 0u32);
        // Steps advance only once their precondition holds: dialog animations set their own pace.
        glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            let advanced = match step {
                // Alert 2 rung: a write on `demo` raises the warn dialog before anything is sent.
                0 => {
                    window.run("CREATE TABLE guarded (id INTEGER PRIMARY KEY, note TEXT)");
                    true
                }
                1 => alert(&window).is_some(),
                2 => {
                    waited >= 2
                        && alert(&window).is_some_and(|dialog| {
                            shoot(&window, &out.replace(".png", "-warn.png"));
                            respond(&dialog, "cancel");
                            true
                        })
                }
                3 => {
                    alert(&window).is_none() && {
                        window.run("SELECT name FROM sqlite_master WHERE name = 'guarded'");
                        true
                    }
                }
                4 => {
                    waited >= 2 && settled(&window) && {
                        window.model().with_status(|status| {
                            assert_eq!(status.state, datagrep_gtk::QueryState::Done);
                            assert_eq!(
                                status.rows_loaded, 0,
                                "the cancelled CREATE was never sent"
                            );
                        });
                        window.run("CREATE TABLE guarded (id INTEGER PRIMARY KEY, note TEXT)");
                        true
                    }
                }
                5 => alert(&window).is_some_and(|dialog| {
                    respond(&dialog, "confirm");
                    true
                }),
                6 => {
                    waited >= 4 && alert(&window).is_none() && settled(&window) && {
                        window.run("SELECT name FROM sqlite_master WHERE name = 'guarded'");
                        true
                    }
                }
                7 => {
                    waited >= 2 && settled(&window) && {
                        window.model().with_status(|status| {
                            assert_eq!(status.state, datagrep_gtk::QueryState::Done);
                            assert_eq!(
                                status.rows_loaded, 1,
                                "the acknowledged CREATE went through"
                            );
                        });
                        shoot(&window, &out.replace(".png", "-after-warn.png"));
                        // Safe 2 rung: no polkit on this host, so the typed phrase is the path.
                        assert!(window.select_connection("vault"));
                        window.run("CREATE TABLE v (id INTEGER PRIMARY KEY)");
                        true
                    }
                }
                8 => alert(&window).is_some(),
                9 => {
                    waited >= 2
                        && alert(&window).is_some_and(|dialog| {
                            shoot(&window, &out.replace(".png", "-auth.png"));
                            type_phrase(&dialog, "staging");
                            respond(&dialog, "authenticate");
                            true
                        })
                }
                // The wrong phrase re-asks: only the dialog carrying the engine's verdict counts.
                10 => alert(&window).filter(has_verdict).is_some(),
                11 => {
                    waited >= 2
                        && alert(&window).is_some_and(|dialog| {
                            shoot(&window, &out.replace(".png", "-auth-refused.png"));
                            type_phrase(&dialog, "vault");
                            respond(&dialog, "authenticate");
                            true
                        })
                }
                12 => {
                    waited >= 4 && alert(&window).is_none() && settled(&window) && {
                        window.run("SELECT name FROM sqlite_master WHERE name = 'v'");
                        true
                    }
                }
                13 => {
                    waited >= 2 && settled(&window) && {
                        window.model().with_status(|status| {
                            assert_eq!(status.state, datagrep_gtk::QueryState::Done);
                            assert_eq!(
                                status.rows_loaded, 1,
                                "the authenticated CREATE went through"
                            );
                        });
                        shoot(&window, &out.replace(".png", "-auth-cleared.png"));
                        let dialog = ConnectionDialog::for_editing(core2.clone(), "vault");
                        dialog.present(Some(&window));
                        editor.replace(Some(dialog));
                        true
                    }
                }
                // The Safety group sits at the bottom of the page: scroll it into the frame.
                14 => {
                    waited >= 3 && {
                        let dialog = editor.borrow().clone().expect("the editor dialog");
                        let scroller = descendant::<gtk::ScrolledWindow>(dialog.upcast_ref())
                            .expect("the preferences scroller");
                        let vadjustment = scroller.vadjustment();
                        vadjustment.set_value(vadjustment.upper());
                        true
                    }
                }
                15 => {
                    waited >= 2 && {
                        shoot(&window, &out.replace(".png", "-dialog.png"));
                        true
                    }
                }
                _ => {
                    app.quit();
                    return glib::ControlFlow::Break;
                }
            };
            (step, waited) = if advanced {
                (step + 1, 0)
            } else {
                (step, waited + 1)
            };
            assert!(waited < 120, "harness stalled at step {step}");
            glib::ControlFlow::Continue
        });
    });
    app.run();
}

fn alert(window: &Window) -> Option<adw::AlertDialog> {
    descendant::<adw::AlertDialog>(window.upcast_ref())
}

// A real click closes and then emits; emitting by hand must close by hand too.
fn respond(dialog: &adw::AlertDialog, response: &str) {
    dialog.emit_by_name::<()>("response", &[&response]);
    dialog.force_close();
}

fn settled(window: &Window) -> bool {
    window.model().with_status(|status| !status.is_streaming())
}

fn has_verdict(dialog: &adw::AlertDialog) -> bool {
    fn any_error_label(root: &gtk::Widget) -> bool {
        let mut child = root.first_child();
        while let Some(widget) = child {
            if widget.is::<gtk::Label>() && widget.has_css_class("error") {
                return true;
            }
            if any_error_label(&widget) {
                return true;
            }
            child = widget.next_sibling();
        }
        false
    }
    any_error_label(dialog.upcast_ref())
}

fn type_phrase(dialog: &adw::AlertDialog, phrase: &str) {
    let entry = descendant::<gtk::Entry>(dialog.upcast_ref()).expect("the phrase entry");
    entry.set_text(phrase);
}

fn descendant<T: IsA<gtk::Widget>>(root: &gtk::Widget) -> Option<T> {
    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Ok(found) = widget.clone().downcast::<T>() {
            return Some(found);
        }
        if let Some(found) = descendant::<T>(&widget) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}

fn shoot(window: &Window, path: &str) {
    let (width, height) = (window.width() as f64, window.height() as f64);
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width, height);
    let Some(node) = snapshot.to_node() else {
        eprintln!("nothing to snapshot");
        return;
    };
    let Some(renderer) = window.native().and_then(|native| native.renderer()) else {
        eprintln!("no renderer");
        return;
    };
    let texture = renderer.render_texture(&node, None);
    if let Err(error) = texture.save_to_png(path) {
        eprintln!("{error}");
    }
}
