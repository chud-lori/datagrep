// Verification harness for schema-aware completion and SQL formatting, rendered to PNGs.
use std::sync::Arc;

use adw::prelude::*;
use datagrep_gtk::ffi::Core;
use datagrep_gtk::tabs::EditorTabs;
use datagrep_gtk::ui::{mount, Window};

fn main() {
    let dir = std::env::var("PREVIEW_DIR").expect("PREVIEW_DIR");
    let out = std::env::var("PREVIEW_OUT").expect("PREVIEW_OUT");
    let app = adw::Application::builder()
        .application_id("io.github.chud_lori.datagrep.CompletionPreview")
        .build();
    app.connect_activate(move |app| {
        std::env::set_var("DATAGREP_CONFIG_DIR", &dir);
        let core = Arc::new(Core::open(&format!("{dir}/profiles.sqlite")).expect("core"));
        let _ = core.add_profile_json("demo", &format!("sqlite://{dir}/demo.sqlite"), "");
        let ddl = core
            .query(
                "demo",
                "CREATE TABLE IF NOT EXISTS people (id INTEGER, full_name TEXT, email TEXT); \
                 CREATE TABLE IF NOT EXISTS pets (id INTEGER, owner_id INTEGER)",
            )
            .expect("the DDL starts");
        std::thread::sleep(std::time::Duration::from_millis(500));
        drop(ddl);
        let window = mount(app, core.clone());
        let tabs: EditorTabs = window.editor_slot().child().unwrap().downcast().unwrap();
        assert!(window.select_connection("demo"), "profile is listed");

        let (out, app) = (out.clone(), app.clone());
        let mut step = 0u32;
        glib::timeout_add_seconds_local(2, move || {
            step += 1;
            let editor = tabs.active_editor();
            match (step, editor) {
                (1, _) => {
                    tabs.new_scratch_tab();
                    let editor = tabs.active_editor().expect("an active editor");
                    editor.set_text("select p.  from people p join pets on pets.owner_id = p.id");
                    let view = source_view(editor.upcast_ref()).expect("a source view");
                    let buffer = view.buffer();
                    buffer.place_cursor(&buffer.iter_at_offset(9));
                    view.grab_focus();
                    view.emit_by_name::<()>("show-completion", &[]);
                    glib::ControlFlow::Continue
                }
                (2, Some(editor)) => {
                    let view = source_view(editor.upcast_ref()).expect("a source view");
                    shoot(&window, &format!("{out}/1-editor.png"));
                    match popover(view.upcast_ref()) {
                        Some(p) => shoot_widget(&p, &format!("{out}/2-completion.png")),
                        None => println!("no completion popover was shown"),
                    }
                    view.emit_by_name::<()>("show-completion", &[]);
                    glib::ControlFlow::Continue
                }
                (3, Some(editor)) => {
                    editor.set_text(
                        "select id, full_name from people where id > 1 and email like '%@x' order by full_name",
                    );
                    let _ = tabs.activate_action("tabs.format", None);
                    println!("formatted:\n{}", editor.text());
                    shoot(&window, &format!("{out}/3-formatted.png"));
                    app.quit();
                    glib::ControlFlow::Break
                }
                _ => {
                    app.quit();
                    glib::ControlFlow::Break
                }
            }
        });
    });
    app.run();
}

fn source_view(widget: &gtk::Widget) -> Option<sourceview5::View> {
    if let Some(view) = widget.downcast_ref::<sourceview5::View>() {
        return Some(view.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = source_view(&current) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

fn popover(widget: &gtk::Widget) -> Option<gtk::Widget> {
    let mut child = widget.first_child();
    while let Some(current) = child {
        if current.is::<gtk::Popover>() && current.is_visible() {
            return Some(current);
        }
        if let Some(found) = popover(&current) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

fn shoot(window: &Window, path: &str) {
    shoot_widget(window.upcast_ref(), path);
}

fn shoot_widget(widget: &gtk::Widget, path: &str) {
    let (width, height) = (widget.width() as f64, widget.height() as f64);
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width, height);
    let Some(node) = snapshot.to_node() else {
        eprintln!("nothing to snapshot");
        return;
    };
    let Some(renderer) = widget.native().and_then(|native| native.renderer()) else {
        eprintln!("no renderer");
        return;
    };
    let texture = renderer.render_texture(&node, None);
    if let Err(error) = texture.save_to_png(path) {
        eprintln!("{error}");
    }
}
