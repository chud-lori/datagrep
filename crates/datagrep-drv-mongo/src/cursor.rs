//! [`MongoCursor`] (ticket item 3) plus the one-shot [`DocsCursor`]/[`AckCursor`]
//! used for command replies, counts, and mutation acknowledgements.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use bson::{doc, Bson, Document as BsonDocument};
use tokio::sync::Mutex;

use datagrep_api::driver::{Batch, Cursor, CursorStats, FetchHint, Payload, ResumeToken};
use datagrep_api::error::DbError;
use datagrep_api::shape::{FieldDef, FieldFlags, LogicalType, Shape};
use datagrep_api::value::Value;

use crate::error::map_mongo_error;
use crate::value::bson_to_value;

/// How (if at all) this cursor can hand back a [`ResumeToken`] (ticket item
/// 3: keyset on `_id` for `find`, `None` with a documented reason for
/// `aggregate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeStrategy {
    /// Plain `find`: encode `{_id: {$gt: last}}` from the last document's
    /// `_id` seen so far.
    IdKeyset,
    /// `aggregate` (or anything else without a stable, index-backed cursor
    /// key the core can safely re-issue a query against).
    None,
}

/// The two live-cursor shapes this driver produces: a plain `find`/
/// `aggregate`/raw-cursor-command cursor, or one bound to an explicit
/// transaction's [`mongodb::ClientSession`] (design's actor-pattern note —
/// see `transaction.rs`'s module doc for why a full actor task turned out to
/// be unnecessary here).
enum Inner {
    Plain(mongodb::Cursor<BsonDocument>),
    Session {
        cursor: mongodb::SessionCursor<BsonDocument>,
        session: Arc<Mutex<mongodb::ClientSession>>,
    },
}

/// Streams `find`/`aggregate`/raw-cursor-command results (ticket item 3).
/// Each BSON document becomes a `Value::Document` (key order preserved,
/// design §3.1) and new top-level field paths emit `SchemaDelta::AddColumn`
/// as they're first observed (design risk #7: "the grid grows a column
/// without refetching").
pub struct MongoCursor {
    inner: Option<Inner>,
    shape: Shape,
    resume: ResumeStrategy,
    seen_fields: HashSet<Arc<str>>,
    last_id: Option<Bson>,
    stats: CursorStats,
}

impl MongoCursor {
    pub fn plain(cursor: mongodb::Cursor<BsonDocument>, resume: ResumeStrategy) -> Self {
        Self {
            inner: Some(Inner::Plain(cursor)),
            shape: Shape::Documents { root_hint: None },
            resume,
            seen_fields: HashSet::new(),
            last_id: None,
            stats: CursorStats::default(),
        }
    }

    pub fn session(
        cursor: mongodb::SessionCursor<BsonDocument>,
        session: Arc<Mutex<mongodb::ClientSession>>,
        resume: ResumeStrategy,
    ) -> Self {
        Self {
            inner: Some(Inner::Session { cursor, session }),
            shape: Shape::Documents { root_hint: None },
            resume,
            seen_fields: HashSet::new(),
            last_id: None,
            stats: CursorStats::default(),
        }
    }

    async fn advance(&mut self) -> Result<bool, DbError> {
        match self.inner.as_mut().ok_or(DbError::Closed)? {
            Inner::Plain(c) => c.advance().await.map_err(map_mongo_error),
            Inner::Session { cursor, session } => {
                let mut guard = session.lock().await;
                cursor.advance(&mut guard).await.map_err(map_mongo_error)
            }
        }
    }

    fn current_len(&self) -> usize {
        match self.inner.as_ref() {
            Some(Inner::Plain(c)) => c.current().as_bytes().len(),
            Some(Inner::Session { cursor, .. }) => cursor.current().as_bytes().len(),
            None => 0,
        }
    }

    fn current_doc(&self) -> Result<BsonDocument, DbError> {
        match self.inner.as_ref().ok_or(DbError::Closed)? {
            Inner::Plain(c) => c.deserialize_current().map_err(map_mongo_error),
            Inner::Session { cursor, .. } => cursor.deserialize_current().map_err(map_mongo_error),
        }
    }

    /// Record any top-level field paths not yet seen on this cursor as
    /// `SchemaDelta::AddColumn` (ticket item 3). Deliberately shallow (only
    /// the document root): the design's `ViewProjection`/`FieldTrie`
    /// machinery that ranks and promotes *nested* paths into columns is a
    /// datagrep-core concern (§3.1) — this cursor only reports what's true about
    /// the wire-level documents it streamed.
    fn track_schema(&mut self, doc: &BsonDocument) -> Vec<datagrep_api::shape::SchemaDelta> {
        let mut deltas = Vec::new();
        for (k, v) in doc.iter() {
            if self.seen_fields.contains(k.as_str()) {
                continue;
            }
            let name: Arc<str> = Arc::from(k.as_str());
            self.seen_fields.insert(name.clone());
            let logical = bson_to_value(v)
                .logical_type()
                .unwrap_or(LogicalType::Unknown);
            deltas.push(datagrep_api::shape::SchemaDelta::AddColumn {
                field: FieldDef {
                    name,
                    logical,
                    flags: FieldFlags::empty(),
                    native_type: None,
                },
            });
        }
        deltas
    }
}

#[async_trait]
impl Cursor for MongoCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.inner.is_none() {
            return Err(DbError::Closed);
        }
        let max_rows = hint.max_rows.max(1) as usize;
        let max_bytes = hint.max_bytes.max(1) as usize;

        let mut docs = Vec::new();
        let mut deltas = Vec::new();
        let mut bytes = 0usize;
        while docs.len() < max_rows && bytes < max_bytes {
            if !self.advance().await? {
                break;
            }
            bytes += self.current_len();
            let doc = self.current_doc()?;
            if let Some(id) = doc.get("_id") {
                self.last_id = Some(id.clone());
            }
            deltas.extend(self.track_schema(&doc));
            docs.push(bson_to_value(&Bson::Document(doc)));
        }

        if docs.is_empty() {
            return Ok(None);
        }

        let n = docs.len() as u64;
        self.stats.rows += n;
        self.stats.bytes += bytes as u64;
        self.stats.batches += 1;

        Ok(Some(Batch {
            seq: self.stats.batches - 1,
            payload: Payload::Docs(docs),
            schema_delta: deltas,
            notices: Vec::new(),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        match self.resume {
            ResumeStrategy::None => None,
            ResumeStrategy::IdKeyset => {
                let id = self.last_id.clone()?;
                let wrapper = doc! { "_id": id };
                let bytes = bson::to_vec(&wrapper).ok()?;
                Some(ResumeToken(bytes.into()))
            }
        }
    }

    fn stats(&self) -> CursorStats {
        self.stats
    }

    async fn close(&mut self) -> Result<(), DbError> {
        // Dropping the inner `mongodb::Cursor`/`SessionCursor` is what makes
        // "always drop the cursor" (ticket item 6) true: the driver's own
        // `Drop` impl issues `killCursors` for any not-yet-exhausted server
        // cursor. Taking the `Option` guarantees that happens exactly once,
        // deterministically, rather than waiting on whenever this struct
        // itself is eventually dropped.
        self.inner.take();
        Ok(())
    }
}

/// Decode a Mongo `_id`-keyset [`ResumeToken`] back into the `Bson` value to
/// filter on (ticket item 3's round-trip; see `connection.rs`'s `Op::Scan`
/// compilation).
pub fn decode_id_keyset(token: &ResumeToken) -> Result<Bson, DbError> {
    let wrapper: BsonDocument = bson::from_slice(&token.0).map_err(|e| {
        DbError::Protocol(format!("resume token is not a valid keyset wrapper: {e}"))
    })?;
    wrapper
        .get("_id")
        .cloned()
        .ok_or_else(|| DbError::Protocol("resume token wrapper missing `_id`".to_string()))
}

/// A cursor that yields a fixed, already-materialized set of documents
/// exactly once — used for raw-command replies that aren't cursor-shaped,
/// `explain` output, and count/mutation acknowledgements framed as one
/// visible document (ticket items 2 and 5).
pub struct DocsCursor {
    shape: Shape,
    docs: Vec<Value>,
    done: bool,
}

impl DocsCursor {
    pub fn new(docs: Vec<Value>) -> Self {
        Self {
            shape: Shape::Documents { root_hint: None },
            docs,
            done: false,
        }
    }
}

#[async_trait]
impl Cursor for DocsCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, _hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        if self.docs.is_empty() {
            return Ok(None);
        }
        Ok(Some(Batch {
            seq: 0,
            payload: Payload::Docs(std::mem::take(&mut self.docs)),
            schema_delta: Vec::new(),
            notices: Vec::new(),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        None
    }

    fn stats(&self) -> CursorStats {
        CursorStats::default()
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.done = true;
        Ok(())
    }
}

/// A one-shot `Ack`-shaped cursor for mutations and DDL (design:
/// `Shape::Ack { affected, message }`), the `message` used to honestly state
/// which count strategy ran (ticket item 2: "surfacing which ran").
pub struct AckCursor {
    shape: Shape,
    done: bool,
}

impl AckCursor {
    pub fn new(affected: Option<u64>, message: Option<Arc<str>>) -> Self {
        Self {
            shape: Shape::Ack { affected, message },
            done: false,
        }
    }
}

#[async_trait]
impl Cursor for AckCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, _hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Batch::default()))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        None
    }

    fn stats(&self) -> CursorStats {
        CursorStats::default()
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.done = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyset_resume_token_round_trips() {
        let id = Bson::Int64(42);
        let wrapper = doc! { "_id": id.clone() };
        let bytes = bson::to_vec(&wrapper).unwrap();
        let token = ResumeToken(bytes.into());
        let decoded = decode_id_keyset(&token).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn keyset_resume_token_rejects_garbage() {
        let token = ResumeToken(bytes::Bytes::from_static(b"not bson"));
        assert!(decode_id_keyset(&token).is_err());
    }

    #[tokio::test]
    async fn docs_cursor_yields_once_then_ends() {
        let mut c = DocsCursor::new(vec![Value::I64(1), Value::I64(2)]);
        let hint = FetchHint::default();
        let first = c.next_batch(hint).await.unwrap().unwrap();
        assert_eq!(
            first.payload,
            Payload::Docs(vec![Value::I64(1), Value::I64(2)])
        );
        assert!(c.next_batch(hint).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ack_cursor_yields_one_empty_batch_then_ends() {
        let mut c = AckCursor::new(Some(3), Some(Arc::from("count_documents (exact)")));
        assert!(matches!(
            c.shape(),
            Shape::Ack {
                affected: Some(3),
                ..
            }
        ));
        let hint = FetchHint::default();
        assert!(c.next_batch(hint).await.unwrap().is_some());
        assert!(c.next_batch(hint).await.unwrap().is_none());
    }
}
