// Verification harness: seeds a profile, runs a statement, snapshots the window to PNG.
use std::rc::Rc;

use adw::prelude::*;
use datagrep_gtk::ffi::Core;
use datagrep_gtk::ui::Window;

fn main() {
    let dir = std::env::var("PREVIEW_DIR").expect("PREVIEW_DIR");
    let app = adw::Application::builder()
        .application_id("io.github.chud_lori.datagrep.Preview")
        .build();
    app.connect_activate(move |app| {
        let core = Rc::new(Core::open(&format!("{dir}/profiles.sqlite")).expect("core"));
        let _ = core.profiles_add("demo", &format!("sqlite://{dir}/demo.sqlite"));
        let window = Window::new(app, core);
        window.present();
        assert!(window.select_connection("demo"), "profile is listed");
        window.run(
            "SELECT n AS id, 'name-' || n AS name, n * 1.5 AS score, \
             'a longer text column value here ' || n AS note, NULL AS empty \
             FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n < 5000) \
             SELECT n FROM c)",
        );

        expand_first_tree_row(window.upcast_ref());

        let out = std::env::var("PREVIEW_PNG").expect("PREVIEW_PNG");
        let app = app.clone();
        glib::timeout_add_seconds_local(3, move || {
            shoot(&window, &out);
            app.quit();
            glib::ControlFlow::Break
        });
    });
    app.run();
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

/// Drives the first expander the way a click would, so the lazy fetch shows in the snapshot.
fn expand_first_tree_row(widget: &gtk::Widget) -> bool {
    if let Some(expander) = widget.downcast_ref::<gtk::TreeExpander>() {
        if let Some(row) = expander.list_row() {
            row.set_expanded(true);
            return true;
        }
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if expand_first_tree_row(&current) {
            return true;
        }
        child = current.next_sibling();
    }
    false
}
