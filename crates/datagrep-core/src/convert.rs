//! Row → Arrow conversion, the store's tabular boundary.
//!
//! Everything below `datagrep-api` speaks `Vec<Value>`; everything at and above the
//! [`crate::store::ResultStore`] speaks Arrow. This module is that boundary and
//! nothing else crosses it.
//!
//! Two storage decisions are implemented here:
//!
//! - **Columnar** — nulls live in Arrow validity bitmaps (one bit per
//!   value), not in an `Option<T>` per cell, and `Value::Absent` — the
//!   not-present marker that keeps a Mongo grid truthful — becomes a null slot
//!   exactly like `Value::Null` does. Arrow simply has no third state, so the
//!   distinction is preserved in the document lane
//!   ([`crate::store::DocSegment`]) instead, which is why documents are
//!   deliberately *not* Arrow.
//! - **Dictionary encoding** — a string column whose sampled cardinality
//!   over the first [`SAMPLE_BATCHES`] batches is under
//!   [`DICTIONARY_RATIO`] of the sampled rows becomes
//!   `Dictionary(Int32, Utf8)`. Its job is no longer shrinking a wire payload:
//!   it shrinks the *shaping* work in the grid, where one shaped run is reused
//!   for every row sharing a dictionary index.
//!
//! Anything Arrow has no honest column type for (intervals, arrays, nested
//! documents, geometry, vectors, `Unsupported`) degrades to a `Utf8` display
//! column rather than being dropped or coerced into a lie.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, FixedSizeBinaryBuilder, Float64Builder,
    Int64Builder, StringBuilder, StringDictionaryBuilder, Time64NanosecondBuilder,
    TimestampMicrosecondBuilder, UInt64Builder,
};
use arrow_array::types::Int32Type;
use arrow_array::{Array, ArrayRef, RecordBatch, RecordBatchOptions, StringArray};
use arrow_schema::{Field, Schema, SchemaRef};
use datagrep_api::driver::Row;
use datagrep_api::shape::{LogicalType, RowSchema};
use datagrep_api::value::{Geometry, TzSpec, Value};

/// How many leading batches feed the cardinality sample. Two is enough to see
/// past a freakishly uniform first chunk without holding results back.
pub const SAMPLE_BATCHES: usize = 2;

/// Distinct/row ratio below which a string column is dictionary-encoded: under
/// 10% distinct values, the repeated strings are worth an index.
pub const DICTIONARY_RATIO: f64 = 0.10;

/// Below this many sampled rows the ratio is noise, so the sample is ignored —
/// a 3-row first batch must not decide the encoding of a 10M-row result.
pub const MIN_SAMPLE_ROWS: usize = 16;

/// One-shot conversion of a single chunk.
///
/// The sampling window degenerates to this one batch, so a low-cardinality
/// string column is dictionary-encoded immediately. Streaming callers should
/// use [`BatchConverter`], which samples across [`SAMPLE_BATCHES`] chunks and
/// keeps one Arrow schema for the whole result set.
///
/// Total by construction: it never panics and never returns an error. Values
/// that do not fit their column's Arrow type land as nulls with a warning —
/// see [`BatchConverter::coerced`].
pub fn rows_to_record_batch(schema: &RowSchema, rows: Vec<Row>) -> RecordBatch {
    let mut conv = BatchConverter::one_shot(Arc::new(schema.clone()));
    conv.convert(rows).batch
}

/// Output of one [`BatchConverter::convert`] call.
#[derive(Debug, Clone)]
pub struct Converted {
    /// The chunk just converted, always in the converter's current schema.
    pub batch: RecordBatch,
    /// Chunks emitted earlier, re-encoded because the sampling window just
    /// closed and chose dictionary encoding. The `usize` is the chunk's
    /// zero-based position in the sequence of batches this converter emitted.
    ///
    /// At most `SAMPLE_BATCHES - 1` entries, exactly once per converter: the
    /// alternative — publishing chunk 1 only after chunk 2 arrives — would
    /// double first-row latency, and the whole point of the pipeline is that
    /// chunk 1 renders before chunk 2 is even requested.
    pub reencoded: Vec<(usize, RecordBatch)>,
}

/// Streaming row→Arrow converter with one locked schema per result set.
///
/// The Arrow column types are decided from the declared [`RowSchema`], refined
/// by scanning the first chunk, and then **locked**: a grid column cannot
/// change type while the user scrolls. String encoding (plain vs dictionary) is
/// decided once, at the end of the sampling window.
#[derive(Debug)]
pub struct BatchConverter {
    schema: Arc<RowSchema>,
    /// Locked after the first chunk.
    kinds: Vec<ColKind>,
    /// Per-column distinct-value sample; `None` once a column is disqualified
    /// (too many distinct values, or not a string column at all).
    samples: Vec<Option<HashSet<Arc<str>>>>,
    sampled_rows: usize,
    batches_seen: usize,
    sample_batches: usize,
    /// Per-column: is this column dictionary-encoded? Empty until the window
    /// closes.
    dict: Vec<bool>,
    arrow_schema: Option<SchemaRef>,
    /// Chunks retained while the window is open, so they can be re-encoded.
    pending: Vec<RecordBatch>,
    coerced: u64,
}

impl BatchConverter {
    /// A converter for one result set with the standard 2-batch sample window.
    pub fn new(schema: Arc<RowSchema>) -> Self {
        Self::with_window(schema, SAMPLE_BATCHES)
    }

    /// Single-chunk converter: decide and encode from this batch alone.
    pub fn one_shot(schema: Arc<RowSchema>) -> Self {
        Self::with_window(schema, 1)
    }

    fn with_window(schema: Arc<RowSchema>, sample_batches: usize) -> Self {
        let n = schema.fields.len();
        Self {
            schema,
            kinds: Vec::new(),
            samples: vec![None; n],
            sampled_rows: 0,
            batches_seen: 0,
            sample_batches: sample_batches.max(1),
            dict: Vec::new(),
            arrow_schema: None,
            pending: Vec::new(),
            coerced: 0,
        }
    }

    /// The locked Arrow schema, once the first chunk has been converted.
    pub fn arrow_schema(&self) -> Option<SchemaRef> {
        self.arrow_schema.clone()
    }

    /// Column indices that ended up dictionary-encoded. Empty while the
    /// sampling window is still open.
    pub fn dictionary_columns(&self) -> Vec<usize> {
        self.dict
            .iter()
            .enumerate()
            .filter_map(|(i, &d)| d.then_some(i))
            .collect()
    }

    /// How many cells did not fit their locked column type and were stored as
    /// null. Non-zero means a driver violated its own declared schema; it is
    /// surfaced rather than swallowed.
    pub fn coerced(&self) -> u64 {
        self.coerced
    }

    /// True once the sampling window has closed and the schema is final.
    pub fn is_settled(&self) -> bool {
        self.batches_seen >= self.sample_batches
    }

    /// Convert one chunk. Never fails: a chunk that cannot be assembled is
    /// reported as an empty batch in the locked schema, which the store shows
    /// as a gap rather than crashing the app.
    pub fn convert(&mut self, rows: Vec<Row>) -> Converted {
        let window_open = self.batches_seen < self.sample_batches;

        if self.kinds.is_empty() {
            self.kinds = decide_kinds(&self.schema, &rows);
            self.samples = self
                .kinds
                .iter()
                .map(|k| (*k == ColKind::Str).then(HashSet::new))
                .collect();
        }
        if window_open {
            self.sample(&rows);
        }

        let index = self.batches_seen;
        self.batches_seen += 1;
        let closing = window_open && self.batches_seen >= self.sample_batches;
        if closing {
            self.settle();
        }

        let dict = if self.dict.is_empty() {
            vec![false; self.kinds.len()]
        } else {
            self.dict.clone()
        };
        let (batch, coerced) = build_batch(&self.schema, &self.kinds, &dict, &rows);
        self.coerced += coerced;
        if self.arrow_schema.is_none() || closing {
            self.arrow_schema = Some(batch.schema());
        }

        let mut reencoded = Vec::new();
        if closing {
            let target = batch.schema();
            let dicts = self.dictionary_columns();
            if !dicts.is_empty() {
                for (i, old) in std::mem::take(&mut self.pending).into_iter().enumerate() {
                    reencoded.push((i, dictionary_encode(&old, &dicts, &target)));
                }
            } else {
                self.pending.clear();
            }
        } else if window_open {
            self.pending.push(batch.clone());
        }
        let _ = index;

        Converted { batch, reencoded }
    }

    /// Accumulate distinct string values per candidate column, bounded: once a
    /// column exceeds the ratio it can never qualify, so tracking stops.
    fn sample(&mut self, rows: &[Row]) {
        self.sampled_rows += rows.len();
        for (col, slot) in self.samples.iter_mut().enumerate() {
            let Some(seen) = slot.as_mut() else { continue };
            for row in rows {
                if let Some(Value::Str(s)) = row.get(col) {
                    if !seen.contains(s) {
                        seen.insert(s.clone());
                    }
                }
            }
            // Disqualify eagerly so the set can never grow past the threshold.
            let cap = ((self.sampled_rows as f64 * DICTIONARY_RATIO).ceil() as usize).max(1);
            if seen.len() > cap {
                *slot = None;
            }
        }
    }

    /// Close the sampling window and pick the final string encodings.
    fn settle(&mut self) {
        let enough = self.sampled_rows >= MIN_SAMPLE_ROWS;
        self.dict = self
            .samples
            .iter()
            .map(|s| match s {
                Some(seen) if enough => {
                    (seen.len() as f64) < self.sampled_rows as f64 * DICTIONARY_RATIO
                }
                _ => false,
            })
            .collect();
        self.samples = vec![None; self.kinds.len()];
    }
}

/// Re-encode the named `Utf8` columns of `batch` as `Dictionary(Int32, Utf8)`
/// so every chunk of a result set shares one schema (spill and export both
/// require it).
fn dictionary_encode(batch: &RecordBatch, dict_cols: &[usize], target: &SchemaRef) -> RecordBatch {
    let rows = batch.num_rows();
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for (i, col) in batch.columns().iter().enumerate() {
        if !dict_cols.contains(&i) {
            columns.push(col.clone());
            continue;
        }
        let Some(strings) = col.as_any().downcast_ref::<StringArray>() else {
            columns.push(col.clone());
            continue;
        };
        let mut b = StringDictionaryBuilder::<Int32Type>::new();
        for i in 0..strings.len() {
            if strings.is_null(i) {
                b.append_null();
            } else {
                b.append_value(strings.value(i));
            }
        }
        columns.push(Arc::new(b.finish()));
    }
    finish_batch(target.clone(), columns, rows)
}

/// Arrow column type for one field, before any dictionary decision.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ColKind {
    /// Nothing but `Null`/`Absent` was observed; Arrow's `Null` type.
    Null,
    Bool,
    I64,
    U64,
    F64,
    Date,
    Time,
    /// Microseconds with one column-wide timezone (`None` = naive).
    Timestamp(Option<Arc<str>>),
    Uuid,
    Bytes,
    /// A genuine string column — the only dictionary-encoding candidate.
    Str,
    /// `Utf8` display fallback: decimals, JSON text, and everything Arrow has
    /// no honest column type for.
    Text,
}

impl ColKind {
    /// Merge an observed value's kind into the column's running kind. Any
    /// disagreement collapses to the lossless `Text` fallback.
    fn merge(self, other: ColKind) -> ColKind {
        match (self, other) {
            (a, ColKind::Null) => a,
            (ColKind::Null, b) => b,
            (a, b) if a == b => a,
            _ => ColKind::Text,
        }
    }
}

/// What a single value would need as a column type.
fn value_kind(v: &Value) -> Option<ColKind> {
    Some(match v {
        Value::Null | Value::Absent => ColKind::Null,
        Value::Bool(_) => ColKind::Bool,
        Value::I64(_) => ColKind::I64,
        Value::U64(_) => ColKind::U64,
        Value::F64(_) => ColKind::F64,
        Value::Date(_) => ColKind::Date,
        Value::Time { .. } => ColKind::Time,
        Value::Timestamp { tz, .. } => ColKind::Timestamp(tz_name(tz)),
        Value::Uuid(_) => ColKind::Uuid,
        Value::Bytes(_) => ColKind::Bytes,
        Value::Str(_) => ColKind::Str,
        // Decimal stays a string so NUMERIC never round-trips through f64 —
        // a silently wrong number is worse than a crash. JSON stays raw text
        // so key order and numeric precision survive re-display unchanged.
        Value::Decimal(_) | Value::Json(_) => ColKind::Text,
        Value::Interval { .. }
        | Value::Array(_)
        | Value::Document(_)
        | Value::Ref { .. }
        | Value::Geo(_)
        | Value::Vector(_)
        | Value::Unsupported { .. } => ColKind::Text,
    })
}

/// The Arrow timezone string for a [`TzSpec`]; `None` means naive.
fn tz_name(tz: &TzSpec) -> Option<Arc<str>> {
    match tz {
        TzSpec::Naive => None,
        TzSpec::Utc => Some(Arc::from("UTC")),
        TzSpec::Named(n) => Some(n.clone()),
        TzSpec::Offset(mins) => {
            let sign = if *mins < 0 { '-' } else { '+' };
            let m = mins.unsigned_abs();
            Some(Arc::from(format!("{sign}{:02}:{:02}", m / 60, m % 60)))
        }
    }
}

/// Seed each column from the declared logical type, then refine against the
/// first chunk's actual values. Declared-schema engines agree on the first try;
/// a disagreement degrades that column to the lossless `Text` fallback.
fn decide_kinds(schema: &RowSchema, rows: &[Row]) -> Vec<ColKind> {
    schema
        .fields
        .iter()
        .enumerate()
        .map(|(col, field)| {
            let mut kind = match field.logical {
                LogicalType::Null => ColKind::Null,
                LogicalType::Bool => ColKind::Bool,
                LogicalType::I64 => ColKind::I64,
                LogicalType::U64 => ColKind::U64,
                LogicalType::F64 => ColKind::F64,
                LogicalType::Date => ColKind::Date,
                LogicalType::Time => ColKind::Time,
                // The declared type says nothing about the timezone; the data
                // does, so start naive and let the first value refine it.
                LogicalType::Timestamp => ColKind::Null,
                LogicalType::Uuid => ColKind::Uuid,
                LogicalType::Bytes => ColKind::Bytes,
                LogicalType::Str => ColKind::Str,
                _ => ColKind::Text,
            };
            for row in rows {
                let Some(v) = row.get(col) else { continue };
                let Some(observed) = value_kind(v) else {
                    continue;
                };
                kind = kind.merge(observed);
                if kind == ColKind::Text {
                    break;
                }
            }
            kind
        })
        .collect()
}

/// Build one `RecordBatch`, returning it plus the number of cells that did not
/// fit their column type (stored as nulls).
fn build_batch(
    schema: &RowSchema,
    kinds: &[ColKind],
    dict: &[bool],
    rows: &[Row],
) -> (RecordBatch, u64) {
    let n = rows.len();
    let mut fields = Vec::with_capacity(kinds.len());
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(kinds.len());
    let mut coerced = 0u64;

    for (col, kind) in kinds.iter().enumerate() {
        let use_dict = *kind == ColKind::Str && dict.get(col).copied().unwrap_or(false);
        let (array, lost) = build_column(kind, use_dict, rows, col);
        coerced += lost;
        let name = schema
            .fields
            .get(col)
            .map(|f| f.name.to_string())
            .unwrap_or_else(|| format!("col{col}"));
        fields.push(Field::new(name, array.data_type().clone(), true));
        columns.push(array);
    }

    let arrow_schema: SchemaRef = Arc::new(Schema::new(fields));
    (finish_batch(arrow_schema, columns, n), coerced)
}

/// Assemble a batch, falling back to an empty batch of the same schema rather
/// than panicking — a malformed chunk from one driver must never take the
/// whole app down.
fn finish_batch(schema: SchemaRef, columns: Vec<ArrayRef>, rows: usize) -> RecordBatch {
    let opts = RecordBatchOptions::new().with_row_count(Some(rows));
    match RecordBatch::try_new_with_options(schema.clone(), columns, &opts) {
        Ok(batch) => batch,
        Err(err) => {
            tracing::error!(%err, rows, "could not assemble RecordBatch; emitting an empty chunk");
            RecordBatch::new_empty(schema)
        }
    }
}

/// A row's cell, treating a short row as absence rather than an error — a
/// driver that sends fewer cells than its schema declares must not panic us.
fn cell_of(row: &Row, col: usize) -> &Value {
    const ABSENT: &Value = &Value::Absent;
    row.get(col).unwrap_or(ABSENT)
}

/// Build one column. `Absent` and `Null` both become null slots in the Arrow
/// validity bitmap, since Arrow has no third state; the `Absent`/`Null`
/// distinction lives in the document lane, not here.
fn build_column(kind: &ColKind, dict: bool, rows: &[Row], col: usize) -> (ArrayRef, u64) {
    let n = rows.len();
    let mut coerced = 0u64;

    macro_rules! primitive {
        ($builder:expr, $pat:pat => $val:expr) => {{
            let mut b = $builder;
            for row in rows {
                match cell_of(row, col) {
                    Value::Null | Value::Absent => b.append_null(),
                    $pat => b.append_value($val),
                    _ => {
                        coerced += 1;
                        b.append_null();
                    }
                }
            }
            Arc::new(b.finish()) as ArrayRef
        }};
    }

    let array: ArrayRef = match kind {
        ColKind::Null => Arc::new(arrow_array::NullArray::new(n)),
        ColKind::Bool => primitive!(BooleanBuilder::with_capacity(n), Value::Bool(v) => *v),
        ColKind::I64 => primitive!(Int64Builder::with_capacity(n), Value::I64(v) => *v),
        ColKind::U64 => primitive!(UInt64Builder::with_capacity(n), Value::U64(v) => *v),
        ColKind::F64 => primitive!(Float64Builder::with_capacity(n), Value::F64(v) => *v),
        ColKind::Date => primitive!(Date32Builder::with_capacity(n), Value::Date(v) => *v),
        ColKind::Time => {
            primitive!(Time64NanosecondBuilder::with_capacity(n), Value::Time { nanos } => *nanos)
        }
        ColKind::Timestamp(tz) => {
            let mut b = TimestampMicrosecondBuilder::with_capacity(n).with_timezone_opt(tz.clone());
            for row in rows {
                match cell_of(row, col) {
                    Value::Null | Value::Absent => b.append_null(),
                    Value::Timestamp { micros, .. } => b.append_value(*micros),
                    _ => {
                        coerced += 1;
                        b.append_null();
                    }
                }
            }
            Arc::new(b.finish())
        }
        ColKind::Uuid => {
            let mut b = FixedSizeBinaryBuilder::with_capacity(n, 16);
            for row in rows {
                match cell_of(row, col) {
                    Value::Uuid(bytes) => {
                        if b.append_value(bytes).is_err() {
                            coerced += 1;
                            b.append_null();
                        }
                    }
                    _ => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        ColKind::Bytes => {
            let mut b = BinaryBuilder::with_capacity(n, n * 16);
            for row in rows {
                match cell_of(row, col) {
                    Value::Null | Value::Absent => b.append_null(),
                    Value::Bytes(v) => b.append_value(v),
                    _ => {
                        coerced += 1;
                        b.append_null();
                    }
                }
            }
            Arc::new(b.finish())
        }
        ColKind::Str | ColKind::Text if dict => {
            let mut b = StringDictionaryBuilder::<Int32Type>::new();
            for row in rows {
                match display_value(cell_of(row, col)) {
                    Some(s) => b.append_value(s),
                    None => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        ColKind::Str | ColKind::Text => {
            let mut b = StringBuilder::with_capacity(n, n * 16);
            for row in rows {
                match display_value(cell_of(row, col)) {
                    Some(s) => b.append_value(s),
                    None => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
    };
    (array, coerced)
}

/// Truthful text for any value, for the `Utf8` fallback column and for the
/// grid's own cell rendering. `None` for `Null` and `Absent` — the two things
/// that must never render as the string `"null"`.
pub fn display_value(v: &Value) -> Option<String> {
    let mut out = String::new();
    write_value(&mut out, v)?;
    Some(out)
}

fn write_value(out: &mut String, v: &Value) -> Option<()> {
    match v {
        Value::Null | Value::Absent => return None,
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::I64(n) => {
            let _ = write!(out, "{n}");
        }
        Value::U64(n) => {
            let _ = write!(out, "{n}");
        }
        Value::F64(n) => {
            let _ = write!(out, "{n}");
        }
        // Decimal and Json are already exact text — never reformat them.
        Value::Decimal(s) | Value::Str(s) | Value::Json(s) => out.push_str(s),
        Value::Bytes(b) => {
            out.push_str("0x");
            for byte in b.iter() {
                let _ = write!(out, "{byte:02x}");
            }
        }
        Value::Date(days) => write_date(out, *days),
        Value::Time { nanos } => write_time(out, *nanos),
        Value::Timestamp { micros, tz } => {
            let days = micros.div_euclid(86_400_000_000);
            let rem = micros.rem_euclid(86_400_000_000);
            write_date(out, days as i32);
            out.push('T');
            write_time(out, rem * 1_000);
            match tz {
                TzSpec::Naive => {}
                TzSpec::Utc => out.push('Z'),
                TzSpec::Named(n) => {
                    let _ = write!(out, "[{n}]");
                }
                TzSpec::Offset(_) => {
                    if let Some(name) = tz_name(tz) {
                        out.push_str(&name);
                    }
                }
            }
        }
        Value::Interval {
            months,
            days,
            nanos,
        } => {
            let _ = write!(out, "{months} mons {days} days {nanos} ns");
        }
        Value::Uuid(b) => {
            for (i, byte) in b.iter().enumerate() {
                if matches!(i, 4 | 6 | 8 | 10) {
                    out.push('-');
                }
                let _ = write!(out, "{byte:02x}");
            }
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if write_value(out, item).is_none() {
                    out.push_str("null");
                }
            }
            out.push(']');
        }
        Value::Document(doc) => {
            out.push('{');
            for (i, (k, val)) in doc.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{k}: ");
                if write_value(out, val).is_none() {
                    out.push_str("null");
                }
            }
            out.push('}');
        }
        Value::Ref { target, key } => {
            let _ = write!(out, "{target}#");
            for (i, k) in key.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if write_value(out, k).is_none() {
                    out.push_str("null");
                }
            }
        }
        Value::Geo(g) => write_geometry(out, g),
        Value::Vector(items) => {
            out.push('[');
            for (i, f) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{f}");
            }
            out.push(']');
        }
        // The escape hatch already carries the driver's own display text.
        Value::Unsupported { display, .. } => out.push_str(display),
    }
    Some(())
}

fn write_geometry(out: &mut String, g: &Geometry) {
    let pts = |out: &mut String, ps: &[(f64, f64)]| {
        out.push('(');
        for (i, (x, y)) in ps.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{x} {y}");
        }
        out.push(')');
    };
    match g {
        Geometry::Point { x, y } => {
            let _ = write!(out, "POINT({x} {y})");
        }
        Geometry::LineString(ps) => {
            out.push_str("LINESTRING");
            pts(out, ps);
        }
        Geometry::Polygon(rings) => {
            out.push_str("POLYGON(");
            for (i, ring) in rings.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                pts(out, ring);
            }
            out.push(')');
        }
        Geometry::Raw { wkb } => {
            let _ = write!(out, "WKB[{} bytes]", wkb.len());
        }
    }
}

/// ISO-8601 date from a day count, via Howard Hinnant's civil-from-days —
/// cheaper and smaller than pulling in a calendar crate for one format.
fn write_date(out: &mut String, days: i32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let _ = write!(out, "{y:04}-{m:02}-{d:02}");
}

fn write_time(out: &mut String, nanos: i64) {
    let n = nanos.rem_euclid(86_400_000_000_000);
    let (h, rem) = (n / 3_600_000_000_000, n % 3_600_000_000_000);
    let (m, rem) = (rem / 60_000_000_000, rem % 60_000_000_000);
    let (s, frac) = (rem / 1_000_000_000, rem % 1_000_000_000);
    let _ = write!(out, "{h:02}:{m:02}:{s:02}");
    if frac != 0 {
        let _ = write!(out, ".{frac:09}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{
        BinaryArray, BooleanArray, Date32Array, DictionaryArray, FixedSizeBinaryArray,
        Float64Array, Int64Array, Time64NanosecondArray, TimestampMicrosecondArray, UInt64Array,
    };
    use arrow_schema::{DataType, TimeUnit};
    use bytes::Bytes;
    use datagrep_api::shape::{FieldDef, FieldFlags};
    use datagrep_api::value::Document;

    fn field(name: &str, logical: LogicalType) -> FieldDef {
        FieldDef {
            name: Arc::from(name),
            logical,
            flags: FieldFlags::NULLABLE,
            native_type: None,
        }
    }

    fn schema(fields: Vec<FieldDef>) -> RowSchema {
        RowSchema {
            fields,
            identity: None,
        }
    }

    fn col<T: 'static>(b: &RecordBatch, i: usize) -> &T {
        b.column(i)
            .as_any()
            .downcast_ref::<T>()
            .expect("column type")
    }

    /// Every `Value` variant that has an Arrow column type lands in that type,
    /// and every variant that does not lands in the `Utf8` display fallback
    /// with its bytes intact.
    #[test]
    fn every_value_variant_maps_to_a_column() {
        let s = schema(vec![
            field("b", LogicalType::Bool),
            field("i", LogicalType::I64),
            field("u", LogicalType::U64),
            field("f", LogicalType::F64),
            field("dec", LogicalType::Decimal),
            field("s", LogicalType::Str),
            field("by", LogicalType::Bytes),
            field("d", LogicalType::Date),
            field("t", LogicalType::Time),
            field("ts", LogicalType::Timestamp),
            field("uu", LogicalType::Uuid),
            field("j", LogicalType::Json),
            field("iv", LogicalType::Interval),
            field("arr", LogicalType::Array),
            field("doc", LogicalType::Document),
            field("geo", LogicalType::Geo),
            field("vec", LogicalType::Vector),
            field("uns", LogicalType::Unknown),
        ]);
        let row: Row = vec![
            Value::Bool(true),
            Value::I64(-7),
            Value::U64(u64::MAX),
            Value::F64(1.5),
            Value::Decimal(Arc::from("1.10")),
            Value::Str(Arc::from("hello")),
            Value::Bytes(Bytes::from_static(&[0xde, 0xad])),
            Value::Date(19_723), // 2024-01-01
            Value::Time {
                nanos: 3_661_000_000_000,
            },
            Value::Timestamp {
                micros: 1_704_067_200_000_000,
                tz: TzSpec::Utc,
            },
            Value::Uuid([0x11; 16]),
            Value::Json(Arc::from(r#"{"b":1,"a":2}"#)),
            Value::Interval {
                months: 1,
                days: 2,
                nanos: 3,
            },
            Value::Array(Arc::from(vec![Value::I64(1), Value::Null])),
            Value::Document(Arc::new(Document::from_fields(vec![(
                Arc::from("k"),
                Value::Str(Arc::from("v")),
            )]))),
            Value::Geo(Arc::new(Geometry::Point { x: 1.0, y: 2.0 })),
            Value::Vector(Arc::from(vec![0.5f32, 1.5])),
            Value::Unsupported {
                type_name: Arc::from("pg_lsn"),
                raw: Bytes::from_static(b"\x00\x01"),
                display: Arc::from("0/1"),
            },
        ];
        let b = rows_to_record_batch(&s, vec![row]);
        assert_eq!(b.num_rows(), 1);
        assert_eq!(b.num_columns(), 18);

        assert!(col::<BooleanArray>(&b, 0).value(0));
        assert_eq!(col::<Int64Array>(&b, 1).value(0), -7);
        assert_eq!(col::<UInt64Array>(&b, 2).value(0), u64::MAX);
        assert_eq!(col::<Float64Array>(&b, 3).value(0), 1.5);
        // Decimal is text, exactly as the server sent it — trailing zero kept.
        assert_eq!(col::<StringArray>(&b, 4).value(0), "1.10");
        assert_eq!(col::<StringArray>(&b, 5).value(0), "hello");
        assert_eq!(col::<BinaryArray>(&b, 6).value(0), &[0xde, 0xad]);
        assert_eq!(col::<Date32Array>(&b, 7).value(0), 19_723);
        assert_eq!(
            col::<Time64NanosecondArray>(&b, 8).value(0),
            3_661_000_000_000
        );
        assert_eq!(
            col::<TimestampMicrosecondArray>(&b, 9).value(0),
            1_704_067_200_000_000
        );
        assert_eq!(
            b.schema().field(9).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some(Arc::from("UTC"))),
            "timezone is never silently dropped"
        );
        assert_eq!(col::<FixedSizeBinaryArray>(&b, 10).value(0), &[0x11; 16]);
        // JSON stays the raw text; key order and precision survive.
        assert_eq!(col::<StringArray>(&b, 11).value(0), r#"{"b":1,"a":2}"#);
        assert_eq!(col::<StringArray>(&b, 12).value(0), "1 mons 2 days 3 ns");
        assert_eq!(col::<StringArray>(&b, 13).value(0), "[1, null]");
        assert_eq!(col::<StringArray>(&b, 14).value(0), "{k: v}");
        assert_eq!(col::<StringArray>(&b, 15).value(0), "POINT(1 2)");
        assert_eq!(col::<StringArray>(&b, 16).value(0), "[0.5, 1.5]");
        assert_eq!(col::<StringArray>(&b, 17).value(0), "0/1");
    }

    /// `Absent` — the not-present marker that makes a document grid
    /// truthful — becomes a null slot, indistinguishable in Arrow from `Null`.
    /// The distinction survives in the document lane, never here.
    #[test]
    fn absent_and_null_both_become_nulls() {
        let s = schema(vec![
            field("i", LogicalType::I64),
            field("s", LogicalType::Str),
        ]);
        let b = rows_to_record_batch(
            &s,
            vec![
                vec![Value::I64(1), Value::Str(Arc::from("a"))],
                vec![Value::Null, Value::Absent],
                // A short row (driver sent fewer cells) is absence too.
                vec![],
            ],
        );
        assert_eq!(b.num_rows(), 3);
        let ints = col::<Int64Array>(&b, 0);
        assert!(!ints.is_null(0));
        assert!(ints.is_null(1), "Null -> null slot");
        assert!(ints.is_null(2), "missing cell -> null slot");
        let strs = col::<StringArray>(&b, 1);
        assert!(strs.is_null(1), "Absent -> null slot");
        assert_eq!(ints.null_count(), 2);
    }

    /// A low-cardinality string column is dictionary-encoded, a
    /// high-cardinality one is not.
    #[test]
    fn dictionary_encoding_kicks_in_on_low_cardinality() {
        let s = schema(vec![
            field("status", LogicalType::Str),
            field("name", LogicalType::Str),
        ]);
        let rows: Vec<Row> = (0..100)
            .map(|i| {
                vec![
                    Value::Str(Arc::from(["active", "closed"][i % 2])),
                    Value::Str(Arc::from(format!("name-{i}"))),
                ]
            })
            .collect();
        let b = rows_to_record_batch(&s, rows);
        assert_eq!(
            b.schema().field(0).data_type(),
            &DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            "2 distinct / 100 rows is well under the 10% threshold"
        );
        assert_eq!(
            b.schema().field(1).data_type(),
            &DataType::Utf8,
            "100 distinct / 100 rows must stay plain Utf8"
        );
        let dict = col::<DictionaryArray<Int32Type>>(&b, 0);
        let values = dict
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("string values");
        assert_eq!(values.len(), 2, "one shaped run per distinct value");
    }

    /// A tiny sample must not decide the encoding of a whole result set.
    #[test]
    fn tiny_samples_do_not_trigger_dictionary_encoding() {
        let s = schema(vec![field("status", LogicalType::Str)]);
        let rows: Vec<Row> = (0..4).map(|_| vec![Value::Str(Arc::from("x"))]).collect();
        let b = rows_to_record_batch(&s, rows);
        assert_eq!(b.schema().field(0).data_type(), &DataType::Utf8);
    }

    /// The encoding decision is taken over the first two batches, and the
    /// chunks already emitted are re-encoded so the whole result set shares
    /// one schema — which is what spill and export require.
    #[test]
    fn streaming_converter_settles_after_two_batches_and_reencodes() {
        let s = Arc::new(schema(vec![field("status", LogicalType::Str)]));
        let mut conv = BatchConverter::new(s);
        let batch = |n: usize| -> Vec<Row> {
            (0..n)
                .map(|i| vec![Value::Str(Arc::from(["a", "b", "c"][i % 3]))])
                .collect()
        };

        let first = conv.convert(batch(50));
        assert!(!conv.is_settled(), "window still open after one batch");
        assert_eq!(first.batch.schema().field(0).data_type(), &DataType::Utf8);
        assert!(first.reencoded.is_empty());

        let second = conv.convert(batch(50));
        assert!(conv.is_settled());
        assert_eq!(conv.dictionary_columns(), vec![0]);
        assert_eq!(
            second.batch.schema().field(0).data_type(),
            &DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8))
        );
        assert_eq!(second.reencoded.len(), 1, "chunk 0 re-encoded");
        let (idx, re) = &second.reencoded[0];
        assert_eq!(*idx, 0);
        assert_eq!(re.schema(), second.batch.schema(), "one schema per result");
        assert_eq!(re.num_rows(), 50);

        let third = conv.convert(batch(10));
        assert!(
            third.reencoded.is_empty(),
            "re-encoding happens exactly once"
        );
        assert_eq!(third.batch.schema(), second.batch.schema());
    }

    /// A column whose values disagree with each other degrades to the lossless
    /// `Utf8` display fallback rather than dropping the odd value.
    #[test]
    fn heterogeneous_column_degrades_to_text() {
        let s = schema(vec![field("mixed", LogicalType::Unknown)]);
        let b = rows_to_record_batch(
            &s,
            vec![
                vec![Value::I64(1)],
                vec![Value::Str(Arc::from("two"))],
                vec![Value::Bool(false)],
            ],
        );
        assert_eq!(b.schema().field(0).data_type(), &DataType::Utf8);
        let c = col::<StringArray>(&b, 0);
        assert_eq!((c.value(0), c.value(1), c.value(2)), ("1", "two", "false"));
    }

    /// Mixed timezones in one column cannot share an Arrow timestamp type, so
    /// the column degrades to text rather than silently reinterpreting an
    /// instant — a wrong instant is worse than an ugly cell.
    #[test]
    fn mixed_timezones_degrade_rather_than_reinterpret() {
        let s = schema(vec![field("ts", LogicalType::Timestamp)]);
        let b = rows_to_record_batch(
            &s,
            vec![
                vec![Value::Timestamp {
                    micros: 0,
                    tz: TzSpec::Utc,
                }],
                vec![Value::Timestamp {
                    micros: 0,
                    tz: TzSpec::Named(Arc::from("Asia/Singapore")),
                }],
            ],
        );
        assert_eq!(b.schema().field(0).data_type(), &DataType::Utf8);
        assert_eq!(col::<StringArray>(&b, 0).value(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn display_null_and_absent_have_no_text() {
        assert_eq!(display_value(&Value::Null), None);
        assert_eq!(display_value(&Value::Absent), None);
    }

    #[test]
    fn date_and_uuid_formatting() {
        assert_eq!(
            display_value(&Value::Date(0)).as_deref(),
            Some("1970-01-01")
        );
        assert_eq!(
            display_value(&Value::Date(19_723)).as_deref(),
            Some("2024-01-01")
        );
        assert_eq!(
            display_value(&Value::Date(-1)).as_deref(),
            Some("1969-12-31")
        );
        let mut uuid = [0u8; 16];
        for (i, b) in uuid.iter_mut().enumerate() {
            *b = i as u8;
        }
        assert_eq!(
            display_value(&Value::Uuid(uuid)).as_deref(),
            Some("00010203-0405-0607-0809-0a0b0c0d0e0f")
        );
    }

    #[test]
    fn zero_column_schema_still_carries_a_row_count() {
        let b = rows_to_record_batch(&schema(vec![]), vec![vec![], vec![]]);
        assert_eq!(b.num_rows(), 2, "row count survives a zero-column schema");
        assert_eq!(b.num_columns(), 0);
    }
}
