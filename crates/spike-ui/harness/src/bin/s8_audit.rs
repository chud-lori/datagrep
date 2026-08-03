//! S8 — gpui-component gap audit (design doc §8, S8: not a kill gate, a
//! *costing* spike).
//!
//! (a) `Table` fed by a synthetic delegate: 1,000,000 rows x 24 columns,
//!     generated on demand from (row_ix, col_ix) -- NO materialized
//!     `Vec` of rows. Delegate calls are counted with `AtomicU64`s; if
//!     `render_td` is ever called anywhere near 1,000,000 x 24 times at
//!     startup, virtualization is fake (design doc's stated kill signal).
//! (b) `Input` in code-editor mode holding ~1 MB of generated SQL.
//!
//! A scripted sequence of `scroll_to_row` jumps exercises the table without
//! needing real mouse input (no automation harness exists yet -- that's
//! §6/CoreApi, out of scope for a throwaway spike). Delegate call counts are
//! logged before/after each jump so the report can show they stay bounded to
//! the viewport rather than scaling with the jump target.
//!
//! Verified directly against the gpui-component 0.5.1 source (crates.io
//! tarball == git tag v0.5.1, cross-checked) rather than `main`-branch docs,
//! which describe some not-yet-released APIs (cell selection, `.line_number`
//! existed already; `.text_center()` and `cell_selectable` did not -- see
//! SPIKE-REPORT.md for the exact methods checked and where).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui::*;
use gpui_component::{
    input::{Input, InputState},
    table::{Column, Table, TableDelegate, TableState},
    v_flex, ActiveTheme, Root,
};

static ROWS_COUNT_CALLS: AtomicU64 = AtomicU64::new(0);
static COLUMNS_COUNT_CALLS: AtomicU64 = AtomicU64::new(0);
static COLUMN_CALLS: AtomicU64 = AtomicU64::new(0);
static RENDER_TD_CALLS: AtomicU64 = AtomicU64::new(0);

const TOTAL_ROWS: usize = 1_000_000;
const TOTAL_COLS: usize = 24;

fn snapshot() -> (u64, u64, u64, u64) {
    (
        ROWS_COUNT_CALLS.load(Ordering::Relaxed),
        COLUMNS_COUNT_CALLS.load(Ordering::Relaxed),
        COLUMN_CALLS.load(Ordering::Relaxed),
        RENDER_TD_CALLS.load(Ordering::Relaxed),
    )
}

fn log_snapshot(tag: &str, start: Instant) {
    let (rc, cc, colc, td) = snapshot();
    eprintln!(
        "[s8] {tag} elapsed={:?} rows_count_calls={rc} columns_count_calls={cc} column_calls={colc} render_td_calls={td}",
        start.elapsed()
    );
}

/// Deterministic cell content from (row, col) alone. No row is ever stored.
fn cell_text(row_ix: usize, col_ix: usize) -> String {
    match col_ix % 6 {
        0 => format!("row-{row_ix}"),
        1 => format!("{}", (row_ix as u64).wrapping_mul(7_919) % 1_000_003),
        2 => format!("user{row_ix}@example.com"),
        3 => match row_ix % 3 {
            0 => "active".to_string(),
            1 => "pending".to_string(),
            _ => "inactive".to_string(),
        },
        4 => format!("{:.2}", ((row_ix as f64) * 0.0001).sin() * 1000.0),
        _ => format!("col{col_ix}-val-{row_ix}"),
    }
}

/// Generates deterministic SQL text, at least `target_bytes` long, without
/// ever holding the whole thing as a Vec of statements -- append straight
/// into one growing String.
fn generate_sql(target_bytes: usize) -> String {
    let mut s = String::with_capacity(target_bytes + 512);
    let mut i: usize = 0;
    while s.len() < target_bytes {
        s.push_str(&format!(
            "-- statement {i}\nSELECT id, name, email, created_at, status\nFROM users_{m}\nWHERE status = 'active' AND created_at > '2024-01-01'\nORDER BY created_at DESC\nLIMIT 100;\n\n",
            i = i,
            m = i % 37
        ));
        i += 1;
    }
    s
}

struct MillionRowDelegate {
    columns: Vec<Column>,
}

impl MillionRowDelegate {
    fn new() -> Self {
        let columns = (0..TOTAL_COLS)
            .map(|i| Column::new(format!("c{i}"), format!("Col {i}")).width(px(130.)))
            .collect();
        Self { columns }
    }
}

impl TableDelegate for MillionRowDelegate {
    fn columns_count(&self, _: &App) -> usize {
        COLUMNS_COUNT_CALLS.fetch_add(1, Ordering::Relaxed);
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        ROWS_COUNT_CALLS.fetch_add(1, Ordering::Relaxed);
        TOTAL_ROWS
    }

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        COLUMN_CALLS.fetch_add(1, Ordering::Relaxed);
        &self.columns[col_ix]
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        RENDER_TD_CALLS.fetch_add(1, Ordering::Relaxed);
        cell_text(row_ix, col_ix)
    }
}

struct Example {
    table: Entity<TableState<MillionRowDelegate>>,
    editor: Entity<InputState>,
    _script_task: Task<()>,
}

impl Example {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = MillionRowDelegate::new();
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .col_movable(true)
                .col_resizable(true)
                .row_selectable(true)
                .col_selectable(true)
        });

        let sql = generate_sql(1_100_000);
        eprintln!("[s8] generated SQL: {} bytes", sql.len());
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("sql")
                .line_number(true)
                .searchable(true)
                .default_value(sql)
        });

        let start = Instant::now();
        log_snapshot("t=0s after construction (before first paint)", start);

        let table_for_task = table.clone();
        let script_task = cx.spawn(async move |_this, cx| {
            Timer::after(Duration::from_secs(2)).await;
            log_snapshot("t=2s pre-scroll", start);
            let _ = table_for_task.update(cx, |t, cx| t.scroll_to_row(500_000, cx));

            Timer::after(Duration::from_millis(800)).await;
            log_snapshot("t=2.8s after scroll_to_row(500_000)", start);

            Timer::after(Duration::from_secs(2)).await;
            let _ = table_for_task.update(cx, |t, cx| t.scroll_to_row(TOTAL_ROWS - 1, cx));
            Timer::after(Duration::from_millis(800)).await;
            log_snapshot("t=5.6s after scroll_to_row(999_999)", start);

            Timer::after(Duration::from_secs(2)).await;
            let _ = table_for_task.update(cx, |t, cx| t.scroll_to_row(0, cx));
            Timer::after(Duration::from_millis(800)).await;
            log_snapshot("t=8.4s after scroll_to_row(0)", start);

            Timer::after(Duration::from_secs(4)).await;
            log_snapshot("t=12.4s FINAL", start);
            eprintln!(
                "[s8] FINAL total possible render_td calls if NOT virtualized would be {}",
                TOTAL_ROWS * TOTAL_COLS
            );
        });

        Self {
            table,
            editor,
            _script_task: script_task,
        }
    }
}

impl Render for Example {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .gap_2()
            .p_2()
            .bg(cx.theme().background)
            .child(
                div()
                    .h(px(480.))
                    .w_full()
                    .child(Table::new(&self.table).stripe(true).bordered(true)),
            )
            .child(div().flex_1().w_full().child(Input::new(&self.editor).h_full()))
    }
}

fn main() {
    eprintln!("[s8] pid={}", std::process::id());
    eprintln!(
        "[s8] toolkit=gpui 0.2.2 + gpui-component 0.5.1 (crates.io), feature=tree-sitter-languages"
    );
    eprintln!(
        "[s8] table: {TOTAL_ROWS} rows x {TOTAL_COLS} cols synthetic, no materialized Vec of rows"
    );

    let app = Application::new();

    app.run(move |cx: &mut App| {
        gpui_component::init(cx);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(880.)), cx)),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| Example::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();

        // Hard auto-quit so this is scriptable from the driving shell.
        cx.spawn(async move |cx| {
            Timer::after(Duration::from_secs(20)).await;
            eprintln!("[s8] auto-quit after 20s");
            cx.update(|cx| cx.quit()).ok();
        })
        .detach();
    });
}
