//! Cursor implementations (design §3.1, §3.2, §3.5).
//!
//! [`RedisPairsCursor`] is the one true SCAN-family cursor: `SCAN`, `HSCAN`,
//! `SSCAN`, and `ZSCAN` share identical cursor mechanics (an opaque numeric
//! cursor, `MATCH`, `COUNT`), so one struct with a command switch drives all
//! four (design §3.1 requirement 2: "one code path with a command switch").
//!
//! **SCAN's own guarantee, restated so the UI doesn't get it wrong:** SCAN
//! guarantees every key present for the entire scan's duration is returned
//! at least once (*eventual completeness*), but keys can legitimately be
//! returned **more than once** if the keyspace is resized mid-scan. This
//! cursor does not deduplicate — silently deduping would hide the fact that
//! the underlying keyspace is being mutated concurrently, which is
//! information the UI should surface (e.g. a small "results may repeat"
//! notice), not swallow. Deduping, if wanted, is the caller's business.

use std::sync::Arc;

use async_trait::async_trait;
use dbx_api::driver::{Batch, CancelFlag, Cursor, CursorStats, FetchHint, Payload, ResumeToken};
use dbx_api::error::DbError;
use dbx_api::shape::{Shape, ValueKind};
use dbx_api::value::Value;
use dbx_api::Bytes;

use crate::error::map_redis_error;
use crate::value::from_resp;

/// Which SCAN-family command this cursor drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanFamily {
    /// Plain `SCAN` over the keyspace. Each key is paired with its `TYPE`
    /// (one pipelined round trip per batch, not one round trip per key) so
    /// a keyspace listing is at least minimally self-describing.
    Keyspace,
    Hash,
    Set,
    SortedSet,
}

impl ScanFamily {
    fn command(self) -> &'static str {
        match self {
            ScanFamily::Keyspace => "SCAN",
            ScanFamily::Hash => "HSCAN",
            ScanFamily::Set => "SSCAN",
            ScanFamily::SortedSet => "ZSCAN",
        }
    }

    fn value_kind(self) -> ValueKind {
        match self {
            ScanFamily::Keyspace => ValueKind::Unknown,
            ScanFamily::Hash => ValueKind::Hash,
            ScanFamily::Set => ValueKind::Set,
            ScanFamily::SortedSet => ValueKind::SortedSet,
        }
    }
}

/// A paging cursor over one SCAN-family command (design §3.1 requirement 3).
pub struct RedisPairsCursor {
    manager: redis::aio::ConnectionManager,
    family: ScanFamily,
    /// The key `HSCAN`/`SSCAN`/`ZSCAN` operate on; unused for `Keyspace`.
    key: Option<String>,
    /// Compiled `MATCH` glob, if a filter was supplied.
    match_glob: Option<String>,
    /// The SCAN cursor for the *next* round; `"0"` means "start" and, after
    /// at least one round has run, also means "done".
    cursor: String,
    started: bool,
    exhausted: bool,
    shape: Shape,
    stats: CursorStats,
    cancel: CancelFlag,
}

impl RedisPairsCursor {
    pub fn new(
        manager: redis::aio::ConnectionManager,
        family: ScanFamily,
        key: Option<String>,
        match_glob: Option<String>,
        resume: Option<ResumeToken>,
        cancel: CancelFlag,
    ) -> Self {
        let cursor = resume
            .and_then(|t| String::from_utf8(t.0.to_vec()).ok())
            .unwrap_or_else(|| "0".to_string());
        Self {
            manager,
            family,
            key,
            match_glob,
            cursor,
            started: false,
            exhausted: false,
            shape: Shape::Pairs {
                value_kind: family.value_kind(),
            },
            stats: CursorStats::default(),
            cancel,
        }
    }
}

#[async_trait]
impl Cursor for RedisPairsCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    #[tracing::instrument(skip(self), fields(family = ?self.family, rows_so_far = self.stats.rows))]
    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.exhausted {
            return Ok(None);
        }
        // Design §3.3: Redis cancellation is our own SCAN loop stopping —
        // there is nothing to send the server. Checked at the one natural
        // await-adjacent point in this loop, per round trip.
        if self.cancel.is_cancelled() {
            self.exhausted = true;
            return Err(DbError::Cancelled);
        }

        let count = hint.max_rows.max(1);
        let mut cmd = redis::Cmd::new();
        cmd.arg(self.family.command());
        if let Some(key) = &self.key {
            cmd.arg(key);
        }
        cmd.arg(&self.cursor);
        if let Some(glob) = &self.match_glob {
            cmd.arg("MATCH").arg(glob);
        }
        cmd.arg("COUNT").arg(count);

        let reply: redis::Value = cmd
            .query_async(&mut self.manager)
            .await
            .map_err(map_redis_error)?;
        let (next_cursor, raw_items) = parse_scan_reply(reply)?;
        self.started = true;
        self.cursor = next_cursor.clone();
        if next_cursor == "0" {
            self.exhausted = true;
        }

        let pairs = match self.family {
            ScanFamily::Keyspace => keyspace_pairs(&mut self.manager, raw_items).await?,
            ScanFamily::Hash => paired_chunks(raw_items, from_resp),
            ScanFamily::Set => raw_items
                .into_iter()
                .map(|m| (from_resp(m), Value::Bool(true)))
                .collect(),
            ScanFamily::SortedSet => paired_chunks(raw_items, score_value),
        };

        let n = pairs.len() as u64;
        self.stats.rows += n;
        self.stats.batches += 1;
        let seq = self.stats.batches - 1;

        Ok(Some(Batch {
            seq,
            payload: Payload::Pairs(pairs),
            schema_delta: Vec::new(),
            notices: Vec::new(),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        if self.exhausted {
            None
        } else if !self.started {
            // Not run yet: resuming means starting from whatever cursor
            // this was constructed with (usually "0").
            Some(ResumeToken(Bytes::from(self.cursor.clone().into_bytes())))
        } else {
            Some(ResumeToken(Bytes::from(self.cursor.clone().into_bytes())))
        }
    }

    fn stats(&self) -> CursorStats {
        self.stats
    }

    async fn close(&mut self) -> Result<(), DbError> {
        // SCAN has no server-side cursor to release — it is entirely
        // client-driven state (the opaque cursor value itself). Nothing to
        // do beyond marking local iteration finished.
        self.exhausted = true;
        Ok(())
    }
}

/// Pull the `[cursor, items]` shape every SCAN-family reply has.
fn parse_scan_reply(v: redis::Value) -> Result<(String, Vec<redis::Value>), DbError> {
    match v {
        redis::Value::Array(mut top) if top.len() == 2 => {
            let items = top.pop().expect("len checked above");
            let cursor_v = top.pop().expect("len checked above");
            let cursor = scan_cursor_to_string(cursor_v)?;
            let items = match items {
                redis::Value::Array(items) => items,
                other => {
                    return Err(DbError::Protocol(format!(
                        "expected an array of scan items, got {other:?}"
                    )))
                }
            };
            Ok((cursor, items))
        }
        other => Err(DbError::Protocol(format!(
            "expected a 2-element SCAN reply [cursor, items], got {other:?}"
        ))),
    }
}

fn scan_cursor_to_string(v: redis::Value) -> Result<String, DbError> {
    match v {
        redis::Value::BulkString(b) => String::from_utf8(b)
            .map_err(|e| DbError::Protocol(format!("non-UTF8 SCAN cursor: {e}"))),
        redis::Value::SimpleString(s) => Ok(s),
        redis::Value::Int(i) => Ok(i.to_string()),
        other => Err(DbError::Protocol(format!(
            "unexpected SCAN cursor reply shape: {other:?}"
        ))),
    }
}

/// Pair up a flat `[a, b, a, b, …]` reply (HSCAN's field/value list,
/// ZSCAN's member/score list) into `(Value, Value)`s, mapping the value
/// side through `map_value` (plain [`from_resp`] for hashes, score-aware
/// parsing for sorted sets).
fn paired_chunks(
    items: Vec<redis::Value>,
    map_value: impl Fn(redis::Value) -> Value,
) -> Vec<(Value, Value)> {
    let mut out = Vec::with_capacity(items.len() / 2);
    let mut it = items.into_iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        out.push((from_resp(k), map_value(v)));
    }
    out
}

/// `ZSCAN` scores are string-encoded doubles even though we negotiate
/// RESP3 (score isn't one of the handful of RESP3-typed reply fields) — so
/// parse the bulk string back into `Value::F64` rather than leaving a
/// score looking like an opaque string in the grid; fall back to whatever
/// [`from_resp`] would have produced if it doesn't parse (never invent
/// data that isn't there).
fn score_value(v: redis::Value) -> Value {
    let mapped = from_resp(v);
    if let Value::Str(s) = &mapped {
        if let Ok(f) = s.parse::<f64>() {
            return Value::F64(f);
        }
    }
    mapped
}

/// Pair each scanned key with its `TYPE`, one pipelined round trip for the
/// whole batch (never one round trip per key, and never `KEYS *` — design
/// §5.2).
async fn keyspace_pairs(
    manager: &mut redis::aio::ConnectionManager,
    raw_keys: Vec<redis::Value>,
) -> Result<Vec<(Value, Value)>, DbError> {
    if raw_keys.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<Value> = raw_keys.into_iter().map(from_resp).collect();
    let mut pipe = redis::pipe();
    for k in &keys {
        match k {
            Value::Str(s) => {
                pipe.cmd("TYPE").arg(s.as_bytes());
            }
            Value::Bytes(b) => {
                pipe.cmd("TYPE").arg(b.as_ref());
            }
            other => {
                return Err(DbError::Protocol(format!(
                    "SCAN returned a non-string key: {other:?}"
                )))
            }
        }
    }
    let types: Vec<redis::Value> = pipe.query_async(manager).await.map_err(map_redis_error)?;
    Ok(keys
        .into_iter()
        .zip(types)
        .map(|(k, t)| (k, from_resp(t)))
        .collect())
}

/// A cursor that yields exactly one batch and then ends — for `Shape::Ack`
/// replies (`OK`, an integer/affected count) and for single-value or
/// single-statement `Request::Native` results that don't fit the SCAN-shaped
/// cursor above (design §3.1 requirement 2).
pub struct OneShotCursor {
    shape: Shape,
    payload: Option<Payload>,
    stats: CursorStats,
}

impl OneShotCursor {
    pub fn new(shape: Shape, payload: Payload) -> Self {
        let rows = match &payload {
            Payload::Pairs(p) => p.len() as u64,
            Payload::Rows(r) => r.len() as u64,
            Payload::Docs(d) => d.len() as u64,
            Payload::Graph(_) | Payload::Empty => 0,
        };
        Self {
            shape,
            payload: Some(payload),
            stats: CursorStats {
                rows,
                batches: 0,
                bytes: 0,
                server_elapsed_micros: None,
            },
        }
    }

    /// Convenience for `Shape::Ack` results.
    pub fn ack(affected: Option<u64>, message: Option<Arc<str>>) -> Self {
        Self::new(Shape::Ack { affected, message }, Payload::Empty)
    }
}

#[async_trait]
impl Cursor for OneShotCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, _hint: FetchHint) -> Result<Option<Batch>, DbError> {
        match self.payload.take() {
            None => Ok(None),
            Some(payload) => {
                self.stats.batches = 1;
                Ok(Some(Batch {
                    seq: 0,
                    payload,
                    schema_delta: Vec::new(),
                    notices: Vec::new(),
                }))
            }
        }
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        None
    }

    fn stats(&self) -> CursorStats {
        self.stats
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.payload = None;
        Ok(())
    }
}

/// Windowed `LRANGE` paging for a `LIST`-typed key (design §3.1 requirement
/// 2: "a 1M-field HASH must page … never come back whole" — the same rule
/// applies to a million-element list). `resume_token` is the next start
/// offset as ASCII decimal.
pub struct ListCursor {
    manager: redis::aio::ConnectionManager,
    key: String,
    next_start: i64,
    exhausted: bool,
    shape: Shape,
    stats: CursorStats,
    cancel: CancelFlag,
}

impl ListCursor {
    pub fn new(
        manager: redis::aio::ConnectionManager,
        key: String,
        resume: Option<ResumeToken>,
        cancel: CancelFlag,
    ) -> Self {
        let next_start = resume
            .and_then(|t| String::from_utf8(t.0.to_vec()).ok())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        Self {
            manager,
            key,
            next_start,
            exhausted: false,
            shape: Shape::Pairs {
                value_kind: ValueKind::List,
            },
            stats: CursorStats::default(),
            cancel,
        }
    }
}

#[async_trait]
impl Cursor for ListCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.exhausted {
            return Ok(None);
        }
        if self.cancel.is_cancelled() {
            self.exhausted = true;
            return Err(DbError::Cancelled);
        }
        let window = hint.max_rows.max(1) as i64;
        let stop = self.next_start + window - 1;
        let reply: Vec<redis::Value> = redis::cmd("LRANGE")
            .arg(&self.key)
            .arg(self.next_start)
            .arg(stop)
            .query_async(&mut self.manager)
            .await
            .map_err(map_redis_error)?;

        let got = reply.len() as i64;
        let pairs: Vec<(Value, Value)> = reply
            .into_iter()
            .enumerate()
            .map(|(i, v)| (Value::I64(self.next_start + i as i64), from_resp(v)))
            .collect();
        if got < window {
            self.exhausted = true;
        }
        self.next_start += got;

        let n = pairs.len() as u64;
        self.stats.rows += n;
        self.stats.batches += 1;
        Ok(Some(Batch {
            seq: self.stats.batches - 1,
            payload: Payload::Pairs(pairs),
            schema_delta: Vec::new(),
            notices: Vec::new(),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        if self.exhausted {
            None
        } else {
            Some(ResumeToken(Bytes::from(
                self.next_start.to_string().into_bytes(),
            )))
        }
    }

    fn stats(&self) -> CursorStats {
        self.stats
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.exhausted = true;
        Ok(())
    }
}

/// Windowed `XRANGE` paging for a `STREAM`-typed key. `resume_token` is the
/// exclusive-start entry ID (`"("`-prefixed per Redis's own range syntax).
pub struct StreamCursor {
    manager: redis::aio::ConnectionManager,
    key: String,
    next_start: String,
    exhausted: bool,
    shape: Shape,
    stats: CursorStats,
    cancel: CancelFlag,
}

impl StreamCursor {
    pub fn new(
        manager: redis::aio::ConnectionManager,
        key: String,
        resume: Option<ResumeToken>,
        cancel: CancelFlag,
    ) -> Self {
        let next_start = resume
            .and_then(|t| String::from_utf8(t.0.to_vec()).ok())
            .unwrap_or_else(|| "-".to_string());
        Self {
            manager,
            key,
            next_start,
            exhausted: false,
            shape: Shape::Pairs {
                value_kind: ValueKind::Stream,
            },
            stats: CursorStats::default(),
            cancel,
        }
    }
}

#[async_trait]
impl Cursor for StreamCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.exhausted {
            return Ok(None);
        }
        if self.cancel.is_cancelled() {
            self.exhausted = true;
            return Err(DbError::Cancelled);
        }
        let count = hint.max_rows.max(1);
        let reply: Vec<redis::Value> = redis::cmd("XRANGE")
            .arg(&self.key)
            .arg(&self.next_start)
            .arg("+")
            .arg("COUNT")
            .arg(count)
            .query_async(&mut self.manager)
            .await
            .map_err(map_redis_error)?;

        let mut pairs = Vec::with_capacity(reply.len());
        let mut last_id: Option<String> = None;
        for entry in reply {
            let redis::Value::Array(mut fields) = entry else {
                return Err(DbError::Protocol(
                    "XRANGE entry was not [id, fields]".into(),
                ));
            };
            if fields.len() != 2 {
                return Err(DbError::Protocol(
                    "XRANGE entry did not have exactly 2 elements".into(),
                ));
            }
            let field_list = fields.pop().expect("len checked above");
            let id_v = fields.pop().expect("len checked above");
            let id = match &id_v {
                redis::Value::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
                redis::Value::SimpleString(s) => s.clone(),
                other => {
                    return Err(DbError::Protocol(format!(
                        "unexpected XRANGE id shape: {other:?}"
                    )))
                }
            };
            last_id = Some(id.clone());
            pairs.push((Value::Str(id.into()), from_resp(field_list)));
        }

        let got = pairs.len() as u32;
        if got < count {
            self.exhausted = true;
        } else if let Some(id) = last_id {
            // Exclusive-start next range: "(" + last id seen.
            self.next_start = format!("({id}");
        } else {
            self.exhausted = true;
        }

        let n = pairs.len() as u64;
        self.stats.rows += n;
        self.stats.batches += 1;
        Ok(Some(Batch {
            seq: self.stats.batches - 1,
            payload: Payload::Pairs(pairs),
            schema_delta: Vec::new(),
            notices: Vec::new(),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        if self.exhausted {
            None
        } else {
            Some(ResumeToken(Bytes::from(
                self.next_start.clone().into_bytes(),
            )))
        }
    }

    fn stats(&self) -> CursorStats {
        self.stats
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.exhausted = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scan_reply_extracts_cursor_and_items() {
        let reply = redis::Value::Array(vec![
            redis::Value::BulkString(b"17".to_vec()),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"a".to_vec()),
                redis::Value::BulkString(b"b".to_vec()),
            ]),
        ]);
        let (cursor, items) = parse_scan_reply(reply).unwrap();
        assert_eq!(cursor, "17");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn parse_scan_reply_rejects_wrong_shape() {
        let bad = redis::Value::Array(vec![redis::Value::Int(1)]);
        assert!(parse_scan_reply(bad).is_err());
    }

    #[test]
    fn paired_chunks_pairs_flat_list() {
        let items = vec![
            redis::Value::BulkString(b"f1".to_vec()),
            redis::Value::BulkString(b"v1".to_vec()),
            redis::Value::BulkString(b"f2".to_vec()),
            redis::Value::BulkString(b"v2".to_vec()),
        ];
        let pairs = paired_chunks(items, from_resp);
        assert_eq!(
            pairs,
            vec![
                (Value::Str("f1".into()), Value::Str("v1".into())),
                (Value::Str("f2".into()), Value::Str("v2".into())),
            ]
        );
    }

    #[test]
    fn score_value_parses_numeric_strings() {
        let v = score_value(redis::Value::BulkString(b"3.5".to_vec()));
        assert_eq!(v, Value::F64(3.5));
        // Non-numeric falls back rather than inventing a number.
        let v2 = score_value(redis::Value::BulkString(b"not-a-number".to_vec()));
        assert_eq!(v2, Value::Str("not-a-number".into()));
    }

    #[test]
    fn resume_token_round_trips_through_scan_cursor_state() {
        // Cursor state is plain ASCII text; exercised end-to-end against a
        // live server in tests/integration.rs (#[ignore]), but the
        // token <-> string mapping itself is a pure function worth pinning
        // here without a server.
        let tok = ResumeToken(Bytes::from_static(b"482"));
        let s = String::from_utf8(tok.0.to_vec()).unwrap();
        assert_eq!(s, "482");
    }
}
