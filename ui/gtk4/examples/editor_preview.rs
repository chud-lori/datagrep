// Verification harness for the editor stack: welcome state, tabs across
// connections, directive precedence, and the connection dialog — to PNGs.
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use datagrep_gtk::connection_dialog::ConnectionDialog;
use datagrep_gtk::ffi::Core;
use datagrep_gtk::tabs::EditorTabs;
use datagrep_gtk::ui::{mount, Window};

fn main() {
    let dir = std::env::var("PREVIEW_DIR").expect("PREVIEW_DIR");
    let out = std::env::var("PREVIEW_OUT").expect("PREVIEW_OUT");
    let app = adw::Application::builder()
        .application_id("io.github.chud_lori.datagrep.EditorPreview")
        .build();
    app.connect_activate(move |app| {
        std::env::set_var("DATAGREP_CONFIG_DIR", &dir);
        let core = Arc::new(Core::open(&format!("{dir}/profiles.sqlite")).expect("core"));
        let _ = core.add_profile_json("demo", &format!("sqlite://{dir}/demo.sqlite"), "");
        let _ = core.add_profile_json(
            "prod",
            &format!("sqlite://{dir}/prod.sqlite"),
            r#"{"color":"red","read_only":true}"#,
        );
        let window = mount(app, core.clone());
        let tabs: EditorTabs = window.editor_slot().child().unwrap().downcast().unwrap();

        let ran_against = Rc::new(RefCell::new(String::new()));
        tabs.connect_local("run-requested", false, {
            let ran_against = ran_against.clone();
            move |values| {
                *ran_against.borrow_mut() = values[1].get::<String>().unwrap_or_default();
                None
            }
        });
        assert!(window.select_connection("demo"), "profile is listed");

        let dialog: Rc<RefCell<Option<ConnectionDialog>>> = Rc::new(RefCell::new(None));
        let state = Rc::new((
            window,
            tabs,
            core,
            ran_against,
            out.clone(),
            app.clone(),
            dialog,
        ));
        glib::timeout_add_seconds_local(2, {
            let state = state.clone();
            let mut step = 0u32;
            move || {
                let (window, tabs, core, ran_against, out, app, dialog) = (
                    &state.0, &state.1, &state.2, &state.3, &state.4, &state.5, &state.6,
                );
                step += 1;
                match step {
                    1 => {
                        shoot(window, &format!("{out}/1-welcome.png"));
                        tabs.new_scratch_tab();
                        let _ = tabs.activate_action("tabs.bind", Some(&"prod".to_variant()));
                        tabs.new_scratch_tab();
                        let editor = tabs.active_editor().expect("an active editor");
                        editor.set_text("-- @connection prod\nSELECT 42 AS answer;");
                        editor.run_statement();
                        glib::ControlFlow::Continue
                    }
                    2 => {
                        assert_eq!(
                            *ran_against.borrow(),
                            "prod",
                            "the -- @connection directive must beat the demo binding"
                        );
                        println!("precedence: directive resolved to `prod` over binding `demo`");
                        shoot(window, &format!("{out}/2-editor.png"));
                        let d = ConnectionDialog::for_new(core.clone());
                        d.present(Some(window));
                        dialog.replace(Some(d));
                        glib::ControlFlow::Continue
                    }
                    3 => {
                        shoot(window, &format!("{out}/3-dialog.png"));
                        if let Some(d) = &*dialog.borrow() {
                            scroll_to_bottom(d.upcast_ref());
                        }
                        glib::ControlFlow::Continue
                    }
                    _ => {
                        shoot(window, &format!("{out}/4-dialog-bottom.png"));
                        app.quit();
                        glib::ControlFlow::Break
                    }
                }
            }
        });
    });
    app.run();
}

fn scroll_to_bottom(widget: &gtk::Widget) -> bool {
    if let Some(scrolled) = widget.downcast_ref::<gtk::ScrolledWindow>() {
        let adj = scrolled.vadjustment();
        adj.set_value(adj.upper());
        return true;
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if scroll_to_bottom(&current) {
            return true;
        }
        child = current.next_sibling();
    }
    false
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
