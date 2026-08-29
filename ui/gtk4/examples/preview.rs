// Verification harness: seeds a profile, runs statements, snapshots the window to PNG.
use std::path::PathBuf;
use std::sync::Arc;

use adw::prelude::*;
use datagrep_gtk::ffi::Core;
use datagrep_gtk::ui::{UtilityPane, Window};

fn main() {
    let dir = std::env::var("PREVIEW_DIR").expect("PREVIEW_DIR");
    let app = adw::Application::builder()
        .application_id("io.github.chud_lori.datagrep.Preview")
        .build();
    app.connect_startup(|_| datagrep_gtk::ui::load_style());
    app.connect_activate(move |app| {
        let core = Arc::new(Core::open(&format!("{dir}/profiles.sqlite")).expect("core"));
        let _ = core.profiles_add("demo", &format!("sqlite://{dir}/demo.sqlite"));
        let window = Window::new(app, core);
        let pane = UtilityPane::mount(&window, PathBuf::from(&dir).join("history"));
        window.present();
        assert!(window.select_connection("demo"), "profile is listed");
        window.reveal_utility();

        // One statement per turn: history records on the terminal status tick.
        let steps: Vec<Box<dyn Fn()>> = vec![
            step(&window, |window| {
                window.run("CREATE TABLE IF NOT EXISTS people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT)")
            }),
            step(&window, |window| window.run("SELECT * FROM nope")),
            step(&window, |window| {
                window.run(
                    "SELECT n AS id, 'name-' || n AS name, n * 1.5 AS score, \
                     'a longer text column value here ' || n AS note, NULL AS empty \
                     FROM (WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n < 5000) \
                     SELECT n FROM c)",
                )
            }),
            step(&window, |window| {
                window
                    .grid()
                    .emit_by_name::<()>("cell-selected", &[&3u64, &3u32]);
                expand_first_tree_row(window.upcast_ref());
            }),
            step(&window, |window| describe_first_object(window.upcast_ref())),
        ];

        let out = std::env::var("PREVIEW_PNG").expect("PREVIEW_PNG");
        let app = app.clone();
        let mut tick = 0usize;
        glib::timeout_add_seconds_local(1, move || {
            if let Some(step) = steps.get(tick) {
                step();
                tick += 1;
                return glib::ControlFlow::Continue;
            }
            // Each switch needs a frame: a paintable snapshotted in the same turn draws nothing.
            tick += 1;
            match tick - steps.len() {
                1 => {
                    shoot(&window, &out);
                    pane.show_page("history");
                    select_first_history_entry(&pane);
                }
                2 => {
                    shoot(&window, &out.replace(".png", "-history.png"));
                    // Replay through the window's one run path: the entry should say ×2.
                    let _ = pane.history().activate_action("history.rerun", None);
                }
                _ => {
                    shoot(&window, &out.replace(".png", "-rerun.png"));
                    app.quit();
                    return glib::ControlFlow::Break;
                }
            }
            glib::ControlFlow::Continue
        });
    });
    app.run();
}

fn step(window: &Window, body: impl Fn(&Window) + 'static) -> Box<dyn Fn()> {
    let window = window.clone();
    Box::new(move || body(&window))
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
    if let Some(expander) = first_expander(widget) {
        if let Some(row) = expander.list_row() {
            row.set_expanded(true);
            return true;
        }
    }
    false
}

/// Selects the first child of the expanded root — the click the inspector describes.
fn describe_first_object(widget: &gtk::Widget) {
    let Some(list) = first_expander(widget)
        .and_then(|expander| expander.ancestor(gtk::ListView::static_type()))
        .and_downcast::<gtk::ListView>()
    else {
        return;
    };
    if let Some(model) = list.model() {
        model.select_item(1, true);
    }
}

/// The entry under the day heading, found by css class: a GtkDropDown carries a list too.
fn select_first_history_entry(pane: &UtilityPane) {
    let mut found = Vec::new();
    collect::<gtk::ListView>(pane.history().upcast_ref(), &mut found);
    let list = found.iter().find(|list| list.has_css_class("dg-history"));
    if let Some(model) = list.and_then(|list| list.model()) {
        model.select_item(1, true);
    }
}

fn first_expander(widget: &gtk::Widget) -> Option<gtk::TreeExpander> {
    find::<gtk::TreeExpander>(widget)
}

fn collect<T: IsA<gtk::Widget>>(widget: &gtk::Widget, found: &mut Vec<T>) {
    if let Some(matched) = widget.downcast_ref::<T>() {
        found.push(matched.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        collect::<T>(&current, found);
        child = current.next_sibling();
    }
}

fn find<T: IsA<gtk::Widget>>(widget: &gtk::Widget) -> Option<T> {
    if let Some(found) = widget.downcast_ref::<T>() {
        return Some(found.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = find::<T>(&current) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}
