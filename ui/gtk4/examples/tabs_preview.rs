// Verification harness for per-tab results and browse-on-click: drives the real
// wiring in ui::mount, asserts what is on screen, and snapshots each state.
use std::sync::Arc;

use adw::prelude::*;
use datagrep_gtk::ffi::Core;
use datagrep_gtk::ui::Window;
use datagrep_gtk::{EditorPage, EditorTabs};

/// One turn of the harness: assert what is on screen, then do the next thing.
type Step = Box<dyn Fn(&Window, &EditorTabs)>;

fn main() {
    let dir = std::env::var("PREVIEW_DIR").expect("PREVIEW_DIR");
    std::env::set_var("DATAGREP_CONFIG_DIR", &dir);
    let app = adw::Application::builder()
        .application_id("io.github.chud_lori.datagrep.TabsPreview")
        .build();
    // Without this every render is unstyled and understates the real window.
    app.connect_startup(|_| datagrep_gtk::ui::load_style());
    app.connect_activate(move |app| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
        let core = Arc::new(Core::open(&format!("{dir}/profiles.sqlite")).expect("core"));
        // Marked and read-only, so a render shows both channels on the row.
        let _ = core.add_profile_json(
            "left",
            &format!("sqlite://{dir}/left.sqlite"),
            r#"{"color":"red"}"#,
        );
        let _ = core.add_profile_json(
            "right",
            &format!("sqlite://{dir}/right.sqlite"),
            r#"{"color":"blue","read_only":true}"#,
        );

        let window = datagrep_gtk::ui::mount(app, core);
        let tabs = window
            .editor_slot()
            .child()
            .and_downcast::<EditorTabs>()
            .expect("the editor tabs are mounted");
        assert!(window.select_connection("left"), "left is listed");

        let out = std::env::var("PREVIEW_PNG").expect("PREVIEW_PNG");
        let steps: Vec<Step> = vec![
            Box::new(|_, tabs| {
                tabs.new_scratch_tab();
                bind(tabs, "left");
                run(
                    tabs,
                    "CREATE TABLE IF NOT EXISTS people (id INTEGER PRIMARY KEY, name TEXT, note TEXT)",
                );
            }),
            Box::new(|_, tabs| {
                run(
                    tabs,
                    "INSERT INTO people (name, note) VALUES ('ada', 'first'), ('grace', 'second')",
                )
            }),
            Box::new(|_, tabs| run(tabs, "SELECT 1 AS a, 2 AS b")),
            Box::new(|window, tabs| {
                assert_eq!(window.model().column_count(), 2, "the left tab's own result");
                tabs.new_scratch_tab();
                bind(tabs, "right");
                run(tabs, "SELECT 1 AS c, 2 AS d, 3 AS e");
            }),
            Box::new(|window, tabs| {
                assert_eq!(window.model().column_count(), 3, "the right tab's own result");
                select(tabs, 0);
            }),
            Box::new(|window, _| {
                assert_eq!(
                    window.model().column_count(),
                    2,
                    "switching back restores the tab's own result, not the last one run"
                );
            }),
            Box::new(|_, tabs| select(tabs, 1)),
            Box::new(|window, tabs| {
                assert_eq!(window.model().column_count(), 3, "and back the other way");
                // An unbound tab follows the window, so a connection switch orphans its result.
                tabs.new_scratch_tab();
                bind(tabs, "");
                run(tabs, "SELECT 1 AS f, 2 AS g, 3 AS h, 4 AS i");
            }),
            Box::new(|window, _| {
                assert_eq!(window.model().column_count(), 4, "the unbound tab ran");
                assert!(window.select_connection("right"), "right is listed");
            }),
            Box::new(|window, _| {
                assert_eq!(
                    window.model().column_count(),
                    0,
                    "a result the current connection did not produce is off screen"
                );
                assert!(window.select_connection("left"), "back to left");
            }),
            Box::new(|window, _| expand_first_tree_row(window.upcast_ref())),
            Box::new(|window, _| activate_first_object(window.upcast_ref())),
            Box::new(|window, tabs| {
                assert_eq!(tabs.live_ids().len(), 4, "the browse opened a tab of its own");
                let editor = tabs.active_editor().expect("the browse tab is in front");
                assert_eq!(editor.subject().as_deref(), Some("people"), "titled by object");
                assert!(
                    editor.text().contains("SELECT * FROM \"main\".\"people\""),
                    "the engine's own statement: {}",
                    editor.text()
                );
                assert!(!editor.is_dirty(), "a browse buffer nobody typed into is clean");
                assert_eq!(window.model().column_count(), 3, "and its rows are loaded");
            }),
            // A second click on the same object focuses its tab rather than opening another.
            Box::new(|window, _| activate_first_object(window.upcast_ref())),
            Box::new(|_, tabs| {
                assert_eq!(tabs.live_ids().len(), 4, "the same object reuses its tab");
            }),
        ];

        let app = app.clone();
        let mut tick = 0usize;
        glib::timeout_add_seconds_local(1, move || {
            match steps.get(tick) {
                Some(step) => step(&window, &tabs),
                None => {
                    shoot(&window, &out);
                    app.quit();
                    return glib::ControlFlow::Break;
                }
            }
            // A paintable snapshotted in the same turn as a layout change draws nothing.
            if tick == 5 {
                shoot(&window, &out.replace(".png", "-restored.png"));
            }
            tick += 1;
            glib::ControlFlow::Continue
        });
    });
    app.run();
}

fn bind(tabs: &EditorTabs, connection: &str) {
    tabs.activate_action("tabs.bind", Some(&connection.to_variant()))
        .expect("the bind action");
}

fn run(tabs: &EditorTabs, sql: &str) {
    let editor = tabs.active_editor().expect("a tab is in front");
    editor.set_text(sql);
    editor.run_statement();
}

fn select(tabs: &EditorTabs, index: usize) {
    let id = tabs.live_ids()[index].clone();
    tabs.open_saved(&id);
    assert_eq!(
        tabs.active_editor().map(|e| e.id()),
        Some(id),
        "the tab came to the front"
    );
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
    if let Err(error) = renderer.render_texture(&node, None).save_to_png(path) {
        eprintln!("{error}");
    }
}

fn expand_first_tree_row(widget: &gtk::Widget) {
    let expander = find::<gtk::TreeExpander>(widget).expect("the schema tree has a root");
    expander
        .list_row()
        .expect("the root is a tree row")
        .set_expanded(true);
}

/// The click that opens an object: activation, not selection.
fn activate_first_object(widget: &gtk::Widget) {
    let list = find::<gtk::TreeExpander>(widget)
        .and_then(|expander| expander.ancestor(gtk::ListView::static_type()))
        .and_downcast::<gtk::ListView>()
        .expect("the tree's list view");
    list.emit_by_name::<()>("activate", &[&1u32]);
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

// Keeps the unused-import check honest about what this harness drives.
const _: fn(&EditorPage) -> String = EditorPage::text;
