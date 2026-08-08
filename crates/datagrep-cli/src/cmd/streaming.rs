//! The window-by-window execution loop shared by `query` and `export`.
//!
//! **CoreApi gap, stated up front** (design §3.2/§5.1: "export streams
//! driver→Arrow→writer→disk with a fixed buffer, never touching grid state
//! \[...\] 'Export all' ≠ 'load all'" — export is supposed to never go
//! through the result store). `CoreApi` exposes exactly one way to run a
//! statement and read its rows: [`datagrep_core::CoreApi::run_query`] +
//! [`datagrep_core::CoreApi::get_rows`], and `get_rows` answers out of
//! `datagrep_core::store::ResultStore` — there is no lower-level façade method
//! that hands a frontend a raw `Cursor`/`Batch` stream bypassing the store.
//! Since the ticket is explicit that `CoreApi` is the *only* entry point
//! ("Do not reach around it into drivers"), `export` in this crate is built
//! on the same path as `query`. It still never accumulates more than one
//! window's rows in *this process* (this function's job), and the store
//! itself is bounded and spills (never the unbounded buffering the design
//! warns about) — but it is not the zero-result-store path the design
//! describes, and that seam belongs in `datagrep-core`, not here.
//!
//! Row buffering here is intentionally shaped like the design's own pipeline
//! (§3.2): ask for one bounded [`FETCH_WINDOW`]-row slice, convert and hand
//! it to the [`crate::format::RowSink`] immediately, discard it, ask for the
//! next slice starting where the last one left off. Nothing here ever holds
//! more than one window's rows, which is what the streaming-proof test in
//! `query.rs` checks with the white-box counter below.
//!
//! Waiting for more data (`WindowStatus::Pending`/`Partial` with nothing new
//! this round) is event-driven, not a sleep loop (design §3.4 "No polling"):
//! [`datagrep_core::CoreApi::subscribe_events`] already exists for exactly this,
//! so this function waits on it instead of re-polling `get_rows` in a spin.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use datagrep_core::store::{DocSegment, WindowSlice, WindowStatus};
use datagrep_core::{QueryEvent, QueryId};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::context::Context;
use crate::exit::CliError;
use crate::format::{Row, RowSink, Summary};
use crate::value_text::{arrow_cell_to_value, CellText};

/// Rows requested per `get_rows` call. Matches the order of magnitude of the
/// design's own hot-window/soft-cap thinking (§3.2) without trying to be
/// adaptive the way the driver-side fetch sizing is — this is a display
/// window, not a wire fetch.
pub(crate) const FETCH_WINDOW: u64 = 5_000;

/// White-box proof for the streaming test (ticket: "a documented white-box
/// counter is fine"): the largest `Vec<Row>` this function ever built from a
/// single store slice, in the current process. A 200k-row result through
/// this loop should never push this past [`FETCH_WINDOW`].
#[cfg(test)]
pub(crate) static MAX_ROWS_PER_BATCH: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Default)]
pub(crate) struct RunOutcome {
    pub rows_shown: u64,
    pub cancelled: bool,
}

/// Drive one already-started query (`qid`) to completion, streaming rows into
/// `sink` a bounded window at a time.
///
/// - `limit` stops early once `rows_shown` reaches it (client-side — see
///   `query.rs`'s module docs on the `ExecOpts.row_limit` gap).
/// - `deadline` stops early once passed (same gap, for `@timeout`/`--timeout`).
/// - `on_progress(rows_shown, elapsed)` is called after every window — `export`
///   uses it to print rows/sec to stderr; `query` ignores it.
pub(crate) async fn stream_result(
    ctx: &Context,
    qid: QueryId,
    sink: &mut dyn RowSink,
    limit: Option<u64>,
    deadline: Option<Instant>,
    mut on_progress: impl FnMut(u64, std::time::Duration),
) -> Result<RunOutcome, CliError> {
    ctx.set_current_query(Some(qid));
    let result = stream_result_inner(ctx, qid, sink, limit, deadline, &mut on_progress).await;
    ctx.set_current_query(None);
    result
}

async fn stream_result_inner(
    ctx: &Context,
    qid: QueryId,
    sink: &mut dyn RowSink,
    limit: Option<u64>,
    deadline: Option<Instant>,
    on_progress: &mut impl FnMut(u64, std::time::Duration),
) -> Result<RunOutcome, CliError> {
    let mut events = ctx.core.subscribe_events();
    let started_at = std::time::Instant::now();
    let mut next = 0u64;
    let mut rows_shown = 0u64;
    let mut started = false;
    let mut columns: Vec<String> = Vec::new();
    let mut note: Option<String> = None;
    let mut cancelled = false;

    loop {
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                let _ = ctx.core.cancel(qid).await;
                cancelled = true;
                note = Some(
                    "stopped: timed out — the server may still be executing this query".to_string(),
                );
                break;
            }
        }

        let window = ctx.core.get_rows(qid, next..next + FETCH_WINDOW).await?;
        // What the *store* delivered this round — used below to decide
        // whether to keep asking (never truncated by `--limit`).
        let delivered = window.rows() as u64;
        let mut hit_limit = false;

        for slice in &window.slices {
            if !started {
                columns = derive_columns(slice);
                sink.start(&columns)?;
                started = true;
            }
            let mut rows = slice_to_rows(slice);
            // `--limit`/`@limit` must cap what actually reaches the sink, not
            // just when we stop asking for more: a single window (up to
            // `FETCH_WINDOW` rows) can already exceed a small `--limit`, so
            // checking the total only *after* writing the whole window would
            // let e.g. `--limit 3` print all 5000. Truncate per slice instead.
            if let Some(limit) = limit {
                let remaining = limit.saturating_sub(rows_shown);
                if (rows.len() as u64) > remaining {
                    rows.truncate(remaining as usize);
                }
            }
            #[cfg(test)]
            MAX_ROWS_PER_BATCH.fetch_max(rows.len(), Ordering::Relaxed);
            rows_shown += rows.len() as u64;
            sink.write_rows(&rows)?;
            if limit.is_some_and(|limit| rows_shown >= limit) {
                hit_limit = true;
                break;
            }
        }
        next += delivered;
        on_progress(rows_shown, started_at.elapsed());

        if hit_limit {
            let _ = ctx.core.cancel(qid).await;
            cancelled = true;
            note = Some(format!(
                "stopped after {} rows (--limit/@limit)",
                limit.unwrap_or(rows_shown)
            ));
            break;
        }

        match window.status {
            WindowStatus::Capped => {
                note = Some("stopped at the soft row cap".to_string());
                break;
            }
            WindowStatus::Cancelled => {
                cancelled = true;
                break;
            }
            WindowStatus::Failed(msg) => return Err(CliError::query(msg.to_string())),
            WindowStatus::Ready => {
                if delivered == 0 {
                    // Past the end of a finished result: nothing more ever.
                    break;
                }
            }
            WindowStatus::Partial | WindowStatus::Pending => {
                if delivered == 0 {
                    wait_for_progress(&mut events, qid, deadline).await;
                }
            }
        }
    }

    if !started {
        sink.start(&columns)?;
        if note.is_none() {
            note = Some(
                "no column data available for this statement (0 rows, or an Ack-shaped \
                 result — see README.md \"CoreApi gaps\": affected-row counts don't reach \
                 CoreApi today)"
                    .to_string(),
            );
        }
    }
    sink.finish(&Summary {
        rows_shown,
        note: note.clone(),
    })?;

    Ok(RunOutcome {
        rows_shown,
        cancelled,
    })
}

/// Wait for the next event about `qid` (or the deadline, or the broadcast
/// channel closing) instead of busy-polling `get_rows` (design §3.4).
async fn wait_for_progress(
    events: &mut broadcast::Receiver<QueryEvent>,
    qid: QueryId,
    deadline: Option<Instant>,
) {
    let recv = async {
        loop {
            match events.recv().await {
                Ok(ev) if ev.qid() == qid => return,
                Ok(_) => continue,
                Err(_) => return,
            }
        }
    };
    match deadline {
        Some(dl) => {
            let _ = tokio::time::timeout_at(dl, recv).await;
        }
        None => recv.await,
    }
}

fn derive_columns(slice: &WindowSlice) -> Vec<String> {
    match slice {
        WindowSlice::Table { batch, .. } => batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect(),
        // No driver in this build produces `Shape::Documents`/`Shape::Pairs`
        // (only sqlite/postgres are registered — both `Shape::Table`); these
        // arms exist so the match is total against the real `WindowSlice`,
        // per `value_text.rs`'s module docs on the same requirement. The
        // column names are a deliberately minimal placeholder, not a real
        // `ViewProjection` (design §3.1) — that's out of scope while nothing
        // exercises this path.
        WindowSlice::Docs { docs, .. } => match docs.as_ref() {
            DocSegment::Values(_) => vec!["doc".to_string()],
            DocSegment::Pairs(_) => vec!["key".to_string(), "value".to_string()],
        },
    }
}

fn slice_to_rows(slice: &WindowSlice) -> Vec<Row> {
    match slice {
        WindowSlice::Table {
            batch, offset, len, ..
        } => (*offset..*offset + *len)
            .map(|r| {
                (0..batch.num_columns())
                    .map(|c| {
                        CellText::from_value(&arrow_cell_to_value(batch.column(c).as_ref(), r))
                    })
                    .collect()
            })
            .collect(),
        WindowSlice::Docs {
            docs, offset, len, ..
        } => match docs.as_ref() {
            // A whole document renders as its JSON text (via `value_to_json`,
            // preserving nesting/key order) rather than the flattened display
            // text `CellText::from_value` gives a scalar cell — that's the
            // difference between "the doc, structurally" and "the doc,
            // stringified" for a value that can be arbitrarily nested.
            DocSegment::Values(values) => values[*offset..*offset + *len]
                .iter()
                .map(|v| vec![doc_cell(v)])
                .collect(),
            DocSegment::Pairs(pairs) => pairs[*offset..*offset + *len]
                .iter()
                .map(|(k, v)| vec![CellText::from_value(k), CellText::from_value(v)])
                .collect(),
        },
    }
}

/// A document cell as its true JSON text, keeping `Null`/`Absent` as the
/// sentinel `CellText` variants (so `--format table`'s NULL/`(absent)`
/// distinction still holds for a whole-document row) rather than collapsing
/// everything through the display-text path.
fn doc_cell(v: &datagrep_api::Value) -> CellText {
    match v {
        datagrep_api::Value::Null => CellText::Null,
        datagrep_api::Value::Absent => CellText::Absent,
        other => match serde_json::to_string(&crate::value_text::value_to_json(other)) {
            Ok(json) => CellText::Text(json),
            Err(_) => CellText::Text(String::from("<unserializable document>")),
        },
    }
}
