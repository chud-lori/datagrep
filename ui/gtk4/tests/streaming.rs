use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use datagrep_gtk::{CellKind, Core, Query, QueryStatus, ResultModel};
use gio::prelude::*;

const DEADLINE: Duration = Duration::from_secs(60);

/// The URL carries no password, so no test ever reaches the OS keychain.
struct Fixture {
    core: Core,
    db: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let db =
            std::env::temp_dir().join(format!("datagrep-gtk-{}-{}.db", std::process::id(), tag));
        let _ = fs::remove_file(&db);
        let core = Core::open(":memory:").expect("an in-memory profile store opens");
        core.profiles_add("t", &format!("sqlite://{}", db.display()))
            .expect("the sqlite profile is accepted");
        Self { core, db }
    }

    fn generate(&self, n: u64) -> Query {
        self.core
            .query(
                "t",
                &format!(
                    "WITH RECURSIVE g(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM g WHERE i < {n}) \
                     SELECT i AS id, 'row ' || i AS label FROM g"
                ),
            )
            .expect("the query starts")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db);
    }
}

/// A context per test: `cargo test` threads share one process-wide default.
fn on_main<R>(body: impl FnOnce(&glib::MainContext) -> R) -> R {
    let ctx = glib::MainContext::new();
    ctx.with_thread_default(|| body(&ctx))
        .expect("the fresh context can be acquired")
}

fn pump(ctx: &glib::MainContext, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        while ctx.iteration(false) {}
        if done() {
            return true;
        }
        if start.elapsed() > DEADLINE {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn await_terminal(query: &Query) -> QueryStatus {
    let start = Instant::now();
    loop {
        let status = QueryStatus::parse(&query.status_json().expect("a status snapshot"));
        if status.state.is_terminal() || start.elapsed() > DEADLINE {
            return status;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn text(model: &ResultModel, row: u64, col: u32) -> String {
    model.with_cell(row, col, |_, s, _| s.to_owned())
}

#[test]
fn a_finished_result_is_exposed_in_full_and_read_without_materialising_it() {
    on_main(|_| {
        let fixture = Fixture::new("finished");
        let query = fixture.generate(5_000);
        let status = await_terminal(&query);
        assert_eq!(status.rows_loaded, 5_000, "{:?}", status.error);

        let model = ResultModel::new();
        model.set_query(query);

        assert_eq!(model.n_items(), 5_000);
        assert_eq!(model.column_count(), 2);
        assert_eq!(model.column(0).expect("first column").name, "id");

        assert_eq!(text(&model, 0, 0), "1");
        assert_eq!(text(&model, 4_999, 1), "row 5000");
        assert_eq!(model.resident_pages(), 2);
        assert!(
            model.resident_rows() <= 2_048,
            "{} rows resident",
            model.resident_rows()
        );

        assert_eq!(text(&model, 511, 0), "512");
        assert_eq!(text(&model, 512, 0), "513");

        let row = model
            .item(512)
            .and_then(|o| o.downcast::<datagrep_gtk::ResultRow>().ok())
            .expect("an item for an exposed row");
        assert_eq!(row.index(), 512);
        assert!(model.item(5_000).is_none());

        model.with_cell(9_999, 0, |kind, s, _| {
            assert_eq!(kind, CellKind::Pending);
            assert!(s.is_empty());
        });
        model.with_cell(0, 99, |kind, _, _| assert_eq!(kind, CellKind::Pending));
    });
}

#[test]
fn rows_appear_as_the_engine_streams_them() {
    on_main(|ctx| {
        let fixture = Fixture::new("streaming");
        let model = ResultModel::new();
        model.set_query(fixture.generate(200_000));

        let at_adoption = model.n_items();
        assert!(
            at_adoption < 200_000,
            "the result was already complete; this test proves nothing"
        );

        let ticks = std::rc::Rc::new(std::cell::Cell::new(0u32));
        model.connect_status_changed({
            let ticks = ticks.clone();
            move |_| ticks.set(ticks.get() + 1)
        });

        let reached = pump(ctx, || model.with_status(|s| s.state.is_terminal()));
        assert!(reached, "the query did not finish inside the deadline");

        pump(ctx, || model.n_items() == 200_000);

        assert_eq!(model.n_items(), 200_000);
        assert_eq!(model.with_status(|s| s.rows_loaded), 200_000);
        assert!(ticks.get() > 0, "no progress tick ever reached the model");
        assert!(model.n_items() > at_adoption);
        assert_eq!(text(&model, 199_999, 1), "row 200000");
        assert!(model.resident_rows() <= 2_048);
    });
}

#[test]
fn cancel_returns_instantly_and_leaves_the_loaded_rows_readable() {
    on_main(|ctx| {
        let fixture = Fixture::new("cancel");
        let model = ResultModel::new();
        model.set_query(fixture.generate(50_000_000));

        pump(ctx, || model.n_items() > 0);
        let loaded = model.n_items();
        assert!(loaded > 0, "the query produced no rows to cancel");

        let start = Instant::now();
        model.cancel();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "cancel blocked for {:?}",
            start.elapsed()
        );

        pump(ctx, || model.with_status(|s| s.state.is_terminal()));
        assert!(model.with_status(|s| s.state.is_terminal()));
        assert!(model.n_items() >= loaded);
        assert_eq!(text(&model, 0, 0), "1");
    });
}

#[test]
fn a_new_result_replaces_the_old_one_and_reset_empties_the_model() {
    on_main(|_| {
        let fixture = Fixture::new("replace");

        let model = ResultModel::new();
        let columns = std::rc::Rc::new(std::cell::Cell::new(0u32));
        model.connect_columns_changed({
            let columns = columns.clone();
            move |_| columns.set(columns.get() + 1)
        });

        let first = fixture.generate(1_000);
        await_terminal(&first);
        model.set_query(first);
        assert_eq!(model.n_items(), 1_000);
        assert_eq!(text(&model, 0, 0), "1");
        assert_eq!(columns.get(), 1, "the first schema was announced");

        let second = fixture.generate(10);
        await_terminal(&second);
        model.set_query(second);
        assert_eq!(model.n_items(), 10);
        assert_eq!(model.resident_pages(), 0, "the old windows were freed");
        assert_eq!(text(&model, 9, 1), "row 10");
        assert_eq!(
            columns.get(),
            3,
            "the old schema was dropped, the new one announced"
        );

        model.reset();
        assert_eq!(model.n_items(), 0);
        assert_eq!(model.column_count(), 0);
        assert_eq!(model.resident_rows(), 0);
        assert!(model.cancel().is_none());
        assert_eq!(columns.get(), 4);
    });
}
