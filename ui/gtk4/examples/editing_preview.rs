// Verification harness for the editing chain: seeds an index, stages, commits into a conflict
// and resolves it, snapshotting the window at each step. DATAGREP_PREVIEW_ES names the cluster.
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use datagrep_gtk::ffi::Core;
use datagrep_gtk::ui::Window;

const SEARCH: &str =
    "GET /events/_search\n{\"query\":{\"match_all\":{}},\"size\":8,\"sort\":[{\"retries\":\"asc\"}]}";

fn main() {
    let dir = std::env::var("PREVIEW_DIR").expect("PREVIEW_DIR");
    let out = std::env::var("PREVIEW_PNG").expect("PREVIEW_PNG");
    let es =
        std::env::var("DATAGREP_PREVIEW_ES").unwrap_or_else(|_| "http://localhost:9200".to_owned());
    seed(&es);

    let app = adw::Application::builder()
        .application_id("io.github.chud_lori.datagrep.EditingPreview")
        .build();
    app.connect_startup(|_| datagrep_gtk::ui::load_style());
    app.connect_activate(move |app| {
        let core = Arc::new(Core::open(&format!("{dir}/profiles.sqlite")).expect("core"));
        let _ = core.profiles_add("events", &format!("{es}/events"));
        let window = Window::new(app, core);
        window.present();
        assert!(window.select_connection("events"), "profile is listed");

        let steps: Vec<Box<dyn Fn() -> bool>> = vec![
            step(&window, |window| {
                window.run(SEARCH);
                true
            }),
            // Waits for the rows rather than guessing how long the cluster takes.
            step(&window, |window| {
                use gio::prelude::ListModelExt;
                if window.model().n_items() < 3 {
                    return false;
                }
                stage(window, 0, "status", "claimed");
                stage(window, 1, "retries", "99");
                if let Err(why) = window.model().stage_delete(2) {
                    eprintln!("{why}");
                }
                true
            }),
        ];

        let (window, es, out) = (window.clone(), es.clone(), out.clone());
        let app = app.clone();
        let mut tick = 0usize;
        // A paintable snapshotted in the same turn as a layout change has nothing to draw.
        glib::timeout_add_local(Duration::from_millis(800), move || {
            if let Some(step) = steps.get(tick) {
                if step() {
                    tick += 1;
                }
                return glib::ControlFlow::Continue;
            }
            tick += 1;
            match tick - steps.len() {
                1 => {
                    shoot(&window, &out);
                    // Somebody else writes row 0's document, so the guard it staged is now stale.
                    bump(&es, "doc-1");
                    window
                        .staged_bar()
                        .emit_by_name::<()>("commit-requested", &[]);
                }
                2 => shoot(&window, &out.replace(".png", "-confirm.png")),
                3 => match find::<adw::AlertDialog>(window.upcast_ref()) {
                    // The bindings expose `response` as a signal only.
                    Some(dialog) => {
                        dialog.emit_by_name::<()>("response", &[&"confirm"]);
                        dialog.close();
                    }
                    None => eprintln!("no commit confirmation on screen"),
                },
                5 => {
                    shoot(&window, &out.replace(".png", "-report.png"));
                    click(&window, "Resolve Conflicts…");
                }
                7 => {
                    shoot(&window, &out.replace(".png", "-conflicts.png"));
                    // Re-guarded against the version just shown, and still not written.
                    click(&window, "Re-apply Onto This Version");
                }
                8 => window
                    .staged_bar()
                    .emit_by_name::<()>("commit-requested", &[]),
                9 => match find::<adw::AlertDialog>(window.upcast_ref()) {
                    Some(dialog) => {
                        dialog.emit_by_name::<()>("response", &[&"confirm"]);
                        dialog.close();
                    }
                    None => eprintln!("no second commit confirmation on screen"),
                },
                11 => {
                    shoot(&window, &out.replace(".png", "-rebased.png"));
                    app.quit();
                    return glib::ControlFlow::Break;
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        });
    });
    app.run();
}

fn step(window: &Window, body: impl Fn(&Window) -> bool + 'static) -> Box<dyn Fn() -> bool> {
    let window = window.clone();
    Box::new(move || body(&window))
}

/// Stages the cell the way the editor popover does, by field name rather than column index.
fn stage(window: &Window, row: u64, field: &str, typed: &str) {
    let model = window.model();
    let column =
        (0..model.column_count()).find(|&col| model.field_name(row, col).as_deref() == Some(field));
    let Some(column) = column else {
        eprintln!("no column named {field}");
        return;
    };
    if let Err(why) = model.stage_edit(row, column, typed) {
        eprintln!("{why}");
    }
}

fn curl(args: &[&str]) {
    match Command::new("curl").args(args).output() {
        Ok(done) if !done.status.success() => eprintln!("curl {args:?} failed"),
        Err(error) => eprintln!("{error}"),
        _ => {}
    }
}

/// A fresh index every run, so a commit from the last one cannot make this one a no-op.
fn seed(es: &str) {
    curl(&["-s", "-X", "DELETE", &format!("{es}/events")]);
    curl(&[
        "-s",
        "-X",
        "PUT",
        &format!("{es}/events"),
        "-H",
        "Content-Type: application/json",
        "-d",
        r#"{"mappings":{"properties":{"status":{"type":"keyword"},"retries":{"type":"integer"},"score":{"type":"float"},"archived":{"type":"boolean"},"note":{"type":"text"}}}}"#,
    ]);
    for n in 1..=8 {
        curl(&[
            "-s",
            "-X",
            "PUT",
            &format!("{es}/events/_doc/doc-{n}"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &format!(
                r#"{{"status":"open","retries":{n},"score":{n}.5,"archived":false,"note":"the {n} th event, waiting on somebody"}}"#
            ),
        ]);
    }
    curl(&["-s", "-X", "POST", &format!("{es}/events/_refresh")]);
}

fn bump(es: &str, id: &str) {
    curl(&[
        "-s",
        "-X",
        "POST",
        &format!("{es}/events/_update/{id}?refresh=true"),
        "-H",
        "Content-Type: application/json",
        "-d",
        r#"{"doc":{"status":"somebody else claimed it"}}"#,
    ]);
}

fn click(window: &Window, label: &str) {
    let mut found = Vec::new();
    collect::<gtk::Button>(window.upcast_ref(), &mut found);
    match found
        .iter()
        .find(|button| button.label().is_some_and(|text| text == label))
    {
        Some(button) => button.emit_clicked(),
        None => eprintln!("no button labelled {label}"),
    }
}

fn shoot(window: &Window, path: &str) {
    let (width, height) = (window.width() as f64, window.height() as f64);
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width, height);
    let Some(node) = snapshot.to_node() else {
        eprintln!("nothing to snapshot for {path}");
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
    let mut found = Vec::new();
    collect::<T>(widget, &mut found);
    found.into_iter().next()
}
