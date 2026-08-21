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

pub(crate) const FETCH_WINDOW: u64 = 5_000;

#[cfg(test)]
pub(crate) static MAX_ROWS_PER_BATCH: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Default)]
pub(crate) struct RunOutcome {
    pub rows_shown: u64,
    pub cancelled: bool,
    pub affected: Option<u64>,
    pub capped: bool,
}

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
    let (shape_cols, json_cols) = ctx
        .core
        .queries()
        .store(qid)
        .map(|s| shape_columns(s.shape()))
        .unwrap_or_default();
    let mut next = 0u64;
    let mut rows_shown = 0u64;
    let mut started = false;
    let mut columns: Vec<String> = shape_cols;
    let mut note: Option<String> = None;
    let mut cancelled = false;
    let mut capped = false;

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
        let delivered = window.rows() as u64;
        let mut hit_limit = false;

        for slice in &window.slices {
            if !started {
                if columns.is_empty() {
                    columns = derive_columns(slice);
                }
                sink.start(&columns)?;
                started = true;
            }
            let mut rows = slice_to_rows(slice, &json_cols);
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
                capped = true;
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

    let state = ctx.core.queries().state(qid);
    let affected = state.as_ref().and_then(|s| s.affected);
    if !started {
        sink.start(&columns)?;
        if note.is_none() {
            if let Some(message) = state.as_ref().and_then(|s| s.ack_message.clone()) {
                note = Some(message.to_string());
            } else if affected.is_none() {
                note = Some("no rows returned by this statement".to_string());
            }
        }
    }
    sink.finish(&Summary {
        rows_shown,
        note: note.clone(),
        affected,
    })?;

    Ok(RunOutcome {
        rows_shown,
        cancelled,
        affected,
        capped,
    })
}

fn shape_columns(shape: &datagrep_api::shape::Shape) -> (Vec<String>, Vec<bool>) {
    use datagrep_api::shape::{LogicalType, Shape};
    match shape {
        Shape::Table(schema) => (
            schema.fields.iter().map(|f| f.name.to_string()).collect(),
            schema
                .fields
                .iter()
                .map(|f| f.logical == LogicalType::Json)
                .collect(),
        ),
        Shape::Documents { .. } => (vec!["doc".to_string()], vec![false]),
        Shape::Pairs { .. } => (
            vec!["key".to_string(), "value".to_string()],
            vec![false, false],
        ),
        _ => (Vec::new(), Vec::new()),
    }
}

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
        WindowSlice::Docs { docs, .. } => match docs.as_ref() {
            DocSegment::Values(_) => vec!["doc".to_string()],
            DocSegment::Pairs(_) => vec!["key".to_string(), "value".to_string()],
        },
    }
}

fn slice_to_rows(slice: &WindowSlice, json_cols: &[bool]) -> Vec<Row> {
    match slice {
        WindowSlice::Table {
            batch, offset, len, ..
        } => (*offset..*offset + *len)
            .map(|r| {
                (0..batch.num_columns())
                    .map(|c| {
                        let value = arrow_cell_to_value(batch.column(c).as_ref(), r);
                        match value {
                            datagrep_api::Value::Str(s)
                                if json_cols.get(c).copied().unwrap_or(false) =>
                            {
                                CellText::Json(s.to_string())
                            }
                            other => CellText::from_value(&other),
                        }
                    })
                    .collect()
            })
            .collect(),
        WindowSlice::Docs {
            docs, offset, len, ..
        } => match docs.as_ref() {
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

fn doc_cell(v: &datagrep_api::Value) -> CellText {
    match v {
        datagrep_api::Value::Null => CellText::Null,
        datagrep_api::Value::Absent => CellText::Absent,
        other => match serde_json::to_string(&crate::value_text::value_to_json(other)) {
            Ok(json) => CellText::Json(json),
            Err(_) => CellText::Text(String::from("<unserializable document>")),
        },
    }
}
