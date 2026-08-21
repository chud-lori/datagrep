use std::fmt;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use datagrep_api::driver::{Batch, Payload};
use datagrep_api::shape::{FieldDef, FieldFlags, LogicalType, RowSchema, SchemaDelta, Shape};
use datagrep_api::value::Value;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::convert::BatchConverter;
use crate::feeder::{FeedState, FeederHandle, ParkReason};
use crate::lock;
use crate::spill::{SpillReader, SpillWriter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpillPolicy {
    Disabled,
    Enabled { dir: PathBuf, max_bytes: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPolicy {
    pub total_result_budget: usize,
    pub per_query_hot: usize,
    pub hot_window_rows: usize,
    pub soft_row_cap: u64,
    pub spill: SpillPolicy,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            total_result_budget: 256 * 1024 * 1024,
            per_query_hot: 64 * 1024 * 1024,
            hot_window_rows: 50_000,
            soft_row_cap: 500_000,
            spill: SpillPolicy::Enabled {
                dir: std::env::temp_dir(),
                max_bytes: 4 * 1024 * 1024 * 1024,
            },
        }
    }
}

impl MemoryPolicy {
    pub fn feeder_policy(&self, default_fetch_rows: u32) -> crate::feeder::FeederPolicy {
        crate::feeder::FeederPolicy {
            soft_row_cap: self.soft_row_cap,
            ..crate::feeder::FeederPolicy::for_fetch_rows(default_fetch_rows)
        }
    }
}

#[derive(Clone)]
pub struct GlobalBudget {
    used: Arc<AtomicUsize>,
    freed: Arc<Notify>,
    limit: usize,
}

impl GlobalBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            used: Arc::new(AtomicUsize::new(0)),
            freed: Arc::new(Notify::new()),
            limit,
        }
    }

    pub fn from_policy(policy: &MemoryPolicy) -> Self {
        Self::new(policy.total_result_budget)
    }

    pub fn try_reserve(&self, bytes: usize) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.saturating_add(bytes);
            if next > self.limit {
                return false;
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => used = actual,
            }
        }
    }

    pub fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.used
            .fetch_sub(bytes.min(self.used()), Ordering::AcqRel);
        self.freed.notify_waiters();
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn available(&self) -> usize {
        self.limit.saturating_sub(self.used())
    }
}

impl fmt::Debug for GlobalBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlobalBudget")
            .field("used", &self.used())
            .field("limit", &self.limit)
            .finish()
    }
}

#[derive(Debug)]
struct BudgetLease {
    budget: GlobalBudget,
    held: AtomicUsize,
}

impl BudgetLease {
    fn new(budget: GlobalBudget) -> Self {
        Self {
            budget,
            held: AtomicUsize::new(0),
        }
    }

    fn try_reserve(&self, bytes: usize) -> bool {
        if self.budget.try_reserve(bytes) {
            self.held.fetch_add(bytes, Ordering::AcqRel);
            true
        } else {
            false
        }
    }

    fn release(&self, bytes: usize) {
        let bytes = bytes.min(self.held.load(Ordering::Acquire));
        if bytes > 0 {
            self.held.fetch_sub(bytes, Ordering::AcqRel);
            self.budget.release(bytes);
        }
    }
}

impl Drop for BudgetLease {
    fn drop(&mut self) {
        let held = *self.held.get_mut();
        if held > 0 {
            self.budget.release(held);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DocSegment {
    Values(Vec<Value>),
    Pairs(Vec<(Value, Value)>),
}

impl DocSegment {
    pub fn len(&self) -> usize {
        match self {
            DocSegment::Values(v) => v.len(),
            DocSegment::Pairs(p) => p.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn byte_size(&self) -> usize {
        match self {
            DocSegment::Values(v) => v.iter().map(value_bytes).sum(),
            DocSegment::Pairs(p) => p.iter().map(|(k, v)| value_bytes(k) + value_bytes(v)).sum(),
        }
    }
}

fn value_bytes(v: &Value) -> usize {
    let base = std::mem::size_of::<Value>();
    base + match v {
        Value::Decimal(s) | Value::Str(s) | Value::Json(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::Array(items) => items.iter().map(value_bytes).sum(),
        Value::Document(doc) => doc
            .iter()
            .map(|(k, val)| k.len() + value_bytes(val))
            .sum::<usize>(),
        Value::Ref { key, .. } => key.iter().map(value_bytes).sum(),
        Value::Vector(v) => v.len() * std::mem::size_of::<f32>(),
        Value::Unsupported { raw, display, .. } => raw.len() + display.len(),
        _ => 0,
    }
}

#[derive(Debug, Clone)]
pub enum ChunkBody {
    Table(Arc<RecordBatch>),
    Docs(Arc<DocSegment>),
    Spilled { index: usize },
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub first_row: u64,
    pub rows: usize,
    pub bytes: usize,
    pub body: ChunkBody,
}

impl Chunk {
    pub fn range(&self) -> Range<u64> {
        self.first_row..self.first_row + self.rows as u64
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorePhase {
    Loading,
    Parked(ParkReason),
    Capped,
    Complete,
    Cancelled,
    Failed(Arc<str>),
}

impl StorePhase {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StorePhase::Capped
                | StorePhase::Complete
                | StorePhase::Cancelled
                | StorePhase::Failed(_)
        )
    }
}

#[derive(Debug, Clone)]
pub struct StoreState {
    pub phase: StorePhase,
    pub rows: u64,
    pub batches: u64,
    pub resident_bytes: usize,
    pub spilled_bytes: u64,
    pub first_batch_micros: Option<u64>,
    pub coerced: u64,
    pub affected: Option<u64>,
    pub ack_message: Option<Arc<str>>,
    pub notices: Vec<datagrep_api::driver::Notice>,
    pub schema_deltas: Vec<SchemaDelta>,
    pub chunks: Vec<Chunk>,
}

impl StoreState {
    fn empty() -> Self {
        Self {
            phase: StorePhase::Loading,
            rows: 0,
            batches: 0,
            resident_bytes: 0,
            spilled_bytes: 0,
            first_batch_micros: None,
            coerced: 0,
            affected: None,
            ack_message: None,
            notices: Vec::new(),
            schema_deltas: Vec::new(),
            chunks: Vec::new(),
        }
    }

    pub fn chunk_of(&self, row: u64) -> Option<usize> {
        self.chunks
            .binary_search_by(|c| {
                if row < c.first_row {
                    std::cmp::Ordering::Greater
                } else if row >= c.first_row + c.rows as u64 {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()
    }

    pub fn resident_rows(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| !matches!(c.body, ChunkBody::Spilled { .. }))
            .map(|c| c.rows)
            .sum()
    }
}

#[derive(Debug, Clone)]
pub enum WindowSlice {
    Table {
        first_row: u64,
        batch: Arc<RecordBatch>,
        offset: usize,
        len: usize,
    },
    Docs {
        first_row: u64,
        docs: Arc<DocSegment>,
        offset: usize,
        len: usize,
    },
}

impl WindowSlice {
    pub fn len(&self) -> usize {
        match self {
            WindowSlice::Table { len, .. } | WindowSlice::Docs { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn first_row(&self) -> u64 {
        match self {
            WindowSlice::Table { first_row, .. } | WindowSlice::Docs { first_row, .. } => {
                *first_row
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowStatus {
    Ready,
    Partial,
    Pending,
    Capped,
    Cancelled,
    Failed(Arc<str>),
}

#[derive(Debug, Clone)]
pub struct RowWindow {
    pub range: Range<u64>,
    pub status: WindowStatus,
    pub slices: Vec<WindowSlice>,
}

impl RowWindow {
    pub fn rows(&self) -> usize {
        self.slices.iter().map(WindowSlice::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }
}

struct StoreShared {
    state: watch::Sender<Arc<StoreState>>,
    feeder: FeederHandle,
    lease: BudgetLease,
    policy: MemoryPolicy,
    spill: Mutex<Option<SpillWriter>>,
    settled_schema: Mutex<Option<SchemaRef>>,
}

impl StoreShared {
    fn publish(&self, f: impl FnOnce(&mut StoreState)) {
        self.state.send_modify(|slot| {
            let mut next = (**slot).clone();
            f(&mut next);
            *slot = Arc::new(next);
        });
    }

    fn snapshot(&self) -> Arc<StoreState> {
        self.state.borrow().clone()
    }

    fn spill_reader(&self) -> Option<SpillReader> {
        lock(&self.spill).as_ref().map(SpillWriter::reader)
    }
}

pub struct ResultStore {
    shared: Arc<StoreShared>,
    shape: Shape,
    cancel: CancellationToken,
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl ResultStore {
    pub fn spawn(
        shape: Shape,
        rx: mpsc::Receiver<Batch>,
        feeder: FeederHandle,
        policy: MemoryPolicy,
        budget: GlobalBudget,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        let (state, _) = watch::channel(Arc::new(StoreState::empty()));
        let shared = Arc::new(StoreShared {
            state,
            feeder,
            lease: BudgetLease::new(budget),
            policy,
            spill: Mutex::new(None),
            settled_schema: Mutex::new(None),
        });
        let writer = tokio::spawn(
            run_store(rx, shared.clone(), shape.clone(), cancel.clone())
                .instrument(tracing::info_span!("store")),
        );
        Arc::new(Self {
            shared,
            shape,
            cancel,
            writer: Mutex::new(Some(writer)),
        })
    }

    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    pub fn state(&self) -> Arc<StoreState> {
        self.shared.snapshot()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<StoreState>> {
        self.state_sender().subscribe()
    }

    fn state_sender(&self) -> &watch::Sender<Arc<StoreState>> {
        &self.shared.state
    }

    pub fn feeder(&self) -> &FeederHandle {
        &self.shared.feeder
    }

    pub fn rows(&self) -> u64 {
        self.state().rows
    }

    pub async fn get_rows(&self, range: Range<u64>) -> RowWindow {
        let snapshot = self.shared.snapshot();
        let mut slices = Vec::new();

        if range.start < snapshot.rows {
            let end = range.end.min(snapshot.rows);
            let mut row = range.start;
            let reader = self.shared.spill_reader();
            while row < end {
                let Some(idx) = snapshot.chunk_of(row) else {
                    break;
                };
                let chunk = &snapshot.chunks[idx];
                let offset = (row - chunk.first_row) as usize;
                let len = ((end.min(chunk.first_row + chunk.rows as u64)) - row) as usize;
                match &chunk.body {
                    ChunkBody::Table(batch) => slices.push(WindowSlice::Table {
                        first_row: row,
                        batch: batch.clone(),
                        offset,
                        len,
                    }),
                    ChunkBody::Docs(docs) => slices.push(WindowSlice::Docs {
                        first_row: row,
                        docs: docs.clone(),
                        offset,
                        len,
                    }),
                    ChunkBody::Spilled { index } => {
                        if let Some(batch) = read_spilled(reader.clone(), *index).await {
                            slices.push(WindowSlice::Table {
                                first_row: row,
                                batch: Arc::new(batch),
                                offset,
                                len,
                            });
                        }
                    }
                }
                row = chunk.first_row + chunk.rows as u64;
            }
        }

        let delivered = slices.iter().map(WindowSlice::len).sum::<usize>() as u64;
        let wanted = range.end.saturating_sub(range.start);
        let status = if delivered >= wanted {
            WindowStatus::Ready
        } else {
            match &snapshot.phase {
                StorePhase::Capped => WindowStatus::Capped,
                StorePhase::Cancelled => WindowStatus::Cancelled,
                StorePhase::Failed(msg) => WindowStatus::Failed(msg.clone()),
                StorePhase::Complete => WindowStatus::Ready,
                StorePhase::Loading | StorePhase::Parked(_) => {
                    self.shared.feeder.resume();
                    if delivered > 0 {
                        WindowStatus::Partial
                    } else {
                        WindowStatus::Pending
                    }
                }
            }
        };

        RowWindow {
            range,
            status,
            slices,
        }
    }

    pub fn stop(&self) {
        self.cancel.cancel();
        self.shared.feeder.stop();
    }

    pub async fn close(&self) {
        self.stop();
        let handle = lock(&self.writer).take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
        self.shared.feeder.join().await;
    }
}

impl Drop for ResultStore {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.shared.feeder.stop();
    }
}

impl fmt::Debug for ResultStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.state();
        f.debug_struct("ResultStore")
            .field("phase", &s.phase)
            .field("rows", &s.rows)
            .field("resident_bytes", &s.resident_bytes)
            .field("spilled_bytes", &s.spilled_bytes)
            .finish()
    }
}

async fn read_spilled(reader: Option<SpillReader>, index: usize) -> Option<RecordBatch> {
    let reader = reader?;
    match tokio::task::spawn_blocking(move || reader.read(index)).await {
        Ok(Ok(batch)) => Some(batch),
        Ok(Err(err)) => {
            tracing::error!(%err, index, "could not read a spilled chunk back");
            None
        }
        Err(err) => {
            tracing::error!(%err, index, "spill read task failed");
            None
        }
    }
}

async fn run_store(
    mut rx: mpsc::Receiver<Batch>,
    shared: Arc<StoreShared>,
    shape: Shape,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let mut converter = table_converter(&shape);

    if let Shape::Ack { affected, message } = &shape {
        let (affected, message) = (*affected, message.clone());
        shared.publish(|s| {
            s.affected = affected;
            s.ack_message = message;
        });
    }

    loop {
        let batch = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                finish(&shared, StorePhase::Cancelled);
                return;
            }
            received = rx.recv() => match received {
                Some(batch) => batch,
                None => break,
            },
        };

        let notices = batch.notices.clone();
        let deltas = batch.schema_delta.clone();
        let Some(admitted) = convert(&mut converter, &shape, batch) else {
            if !notices.is_empty() || !deltas.is_empty() {
                shared.publish(|s| {
                    s.notices.extend(notices);
                    s.schema_deltas.extend(deltas);
                });
            }
            continue;
        };

        if !admit(&shared, admitted.bytes, &cancel).await {
            finish(&shared, StorePhase::Cancelled);
            return;
        }

        if let Some(conv) = converter.as_ref() {
            if conv.is_settled() {
                if let Some(schema) = conv.arrow_schema() {
                    *lock(&shared.settled_schema) = Some(schema);
                }
            }
        }

        let elapsed = started.elapsed().as_micros() as u64;
        let coerced = converter.as_ref().map(BatchConverter::coerced).unwrap_or(0);
        shared.publish(|s| {
            for (index, batch) in &admitted.reencoded {
                if let Some(chunk) = s.chunks.get_mut(*index) {
                    if let ChunkBody::Table(_) = chunk.body {
                        chunk.body = ChunkBody::Table(Arc::new(batch.clone()));
                    }
                }
            }
            s.chunks.push(Chunk {
                first_row: s.rows,
                rows: admitted.rows,
                bytes: admitted.bytes,
                body: admitted.body.clone(),
            });
            s.rows += admitted.rows as u64;
            s.batches += 1;
            s.resident_bytes += admitted.bytes;
            s.coerced = coerced;
            s.notices.extend(notices);
            s.schema_deltas.extend(deltas);
            if s.first_batch_micros.is_none() {
                s.first_batch_micros = Some(elapsed);
            }
            if !s.phase.is_terminal() {
                s.phase = StorePhase::Loading;
            }
        });

        enforce_hot_window(&shared).await;
    }

    let phase = match shared.feeder.state() {
        FeedState::Capped => StorePhase::Capped,
        FeedState::Cancelled => StorePhase::Cancelled,
        FeedState::Failed(message) => StorePhase::Failed(message),
        // Done, or a feeder that vanished — either way nothing more arrives.
        _ => StorePhase::Complete,
    };
    finish(&shared, phase);
}

fn finish(shared: &StoreShared, phase: StorePhase) {
    tracing::debug!(?phase, rows = shared.snapshot().rows, "store finished");
    shared.publish(|s| s.phase = phase);
}

struct Admitted {
    rows: usize,
    bytes: usize,
    body: ChunkBody,
    reencoded: Vec<(usize, RecordBatch)>,
}

fn table_converter(shape: &Shape) -> Option<BatchConverter> {
    match shape {
        Shape::Table(schema) => Some(BatchConverter::new(schema.clone())),
        _ => None,
    }
}

fn synthesized_schema(width: usize) -> RowSchema {
    RowSchema {
        fields: (0..width)
            .map(|i| FieldDef {
                name: Arc::from(format!("col{i}")),
                logical: LogicalType::Unknown,
                flags: FieldFlags::NULLABLE,
                native_type: None,
            })
            .collect(),
        identity: None,
    }
}

fn convert(
    converter: &mut Option<BatchConverter>,
    shape: &Shape,
    batch: Batch,
) -> Option<Admitted> {
    match batch.payload {
        Payload::Rows(rows) => {
            if rows.is_empty() {
                return None;
            }
            let conv = converter.get_or_insert_with(|| {
                if let Shape::Table(schema) = shape {
                    BatchConverter::new(schema.clone())
                } else {
                    BatchConverter::new(Arc::new(synthesized_schema(rows[0].len())))
                }
            });
            let converted = conv.convert(rows);
            let record = converted.batch;
            Some(Admitted {
                rows: record.num_rows(),
                bytes: record.get_array_memory_size(),
                body: ChunkBody::Table(Arc::new(record)),
                reencoded: converted.reencoded,
            })
        }
        Payload::Docs(values) => {
            if values.is_empty() {
                return None;
            }
            let segment = DocSegment::Values(values);
            Some(Admitted {
                rows: segment.len(),
                bytes: segment.byte_size(),
                body: ChunkBody::Docs(Arc::new(segment)),
                reencoded: Vec::new(),
            })
        }
        Payload::Pairs(pairs) => {
            if pairs.is_empty() {
                return None;
            }
            let segment = DocSegment::Pairs(pairs);
            Some(Admitted {
                rows: segment.len(),
                bytes: segment.byte_size(),
                body: ChunkBody::Docs(Arc::new(segment)),
                reencoded: Vec::new(),
            })
        }
        Payload::Graph(chunk) => {
            tracing::warn!(
                nodes = chunk.nodes.len(),
                "graph results have no store representation yet; chunk skipped"
            );
            None
        }
        Payload::Empty => None,
    }
}

async fn admit(shared: &Arc<StoreShared>, bytes: usize, cancel: &CancellationToken) -> bool {
    let mut parked = false;
    loop {
        let freed = shared.lease.budget.freed.notified();
        tokio::pin!(freed);
        freed.as_mut().enable();

        if shared.lease.try_reserve(bytes) {
            if parked {
                shared.feeder.resume();
                shared.publish(|s| {
                    if matches!(s.phase, StorePhase::Parked(_)) {
                        s.phase = StorePhase::Loading;
                    }
                });
            }
            return true;
        }

        // Prefer giving memory back over stopping the stream.
        if spill_oldest(shared).await {
            continue;
        }

        if !parked {
            parked = true;
            tracing::debug!(
                bytes,
                used = shared.lease.budget.used(),
                limit = shared.lease.budget.limit(),
                "result budget exhausted; parking the feeder"
            );
            shared.feeder.park(ParkReason::MemoryBudget);
            shared.publish(|s| s.phase = StorePhase::Parked(ParkReason::MemoryBudget));
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return false,
            _ = &mut freed => {}
        }
    }
}

async fn enforce_hot_window(shared: &Arc<StoreShared>) {
    loop {
        let snapshot = shared.snapshot();
        let over_bytes = snapshot.resident_bytes > shared.policy.per_query_hot;
        let over_rows = snapshot.resident_rows() > shared.policy.hot_window_rows;
        if !over_bytes && !over_rows {
            return;
        }
        if !spill_oldest(shared).await {
            if lock(&shared.settled_schema).is_some() {
                shared.feeder.park(ParkReason::HotWindow);
                shared.publish(|s| {
                    if !s.phase.is_terminal() {
                        s.phase = StorePhase::Parked(ParkReason::HotWindow);
                    }
                });
            }
            return;
        }
    }
}

async fn spill_oldest(shared: &Arc<StoreShared>) -> bool {
    let SpillPolicy::Enabled { dir, max_bytes } = shared.policy.spill.clone() else {
        return false;
    };

    let Some(schema) = lock(&shared.settled_schema).clone() else {
        return false;
    };

    let snapshot = shared.snapshot();
    let Some((index, batch, bytes)) =
        snapshot
            .chunks
            .iter()
            .enumerate()
            .find_map(|(i, c)| match &c.body {
                ChunkBody::Table(b) if b.schema() == schema => Some((i, b.clone(), c.bytes)),
                _ => None,
            })
    else {
        return false;
    };

    let writer = {
        let mut slot = lock(&shared.spill);
        if slot.is_none() {
            match SpillWriter::create(&dir, schema, max_bytes) {
                Ok(writer) => *slot = Some(writer),
                Err(err) => {
                    tracing::warn!(%err, "could not open a spill file; keeping the chunk resident");
                    return false;
                }
            }
        }
        slot.clone()
    };
    let Some(writer) = writer else { return false };

    let spill_writer = writer.clone();
    let to_write = batch.clone();
    let written = tokio::task::spawn_blocking(move || spill_writer.append(&to_write)).await;
    let slot_index = match written {
        Ok(Ok(index)) => index,
        Ok(Err(err)) => {
            tracing::warn!(%err, "spill append failed; keeping the chunk resident");
            return false;
        }
        Err(err) => {
            tracing::error!(%err, "spill task failed; keeping the chunk resident");
            return false;
        }
    };

    let spilled_bytes = writer.bytes();
    shared.publish(|s| {
        if let Some(chunk) = s.chunks.get_mut(index) {
            chunk.body = ChunkBody::Spilled { index: slot_index };
            chunk.bytes = 0;
        }
        s.resident_bytes = s.resident_bytes.saturating_sub(bytes);
        s.spilled_bytes = spilled_bytes;
    });
    shared.lease.release(bytes);
    tracing::debug!(chunk = index, bytes, "chunk spilled");
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::rows_to_record_batch;
    use crate::feeder::{spawn_feeder, DATA_CHANNEL_BOUND};
    use crate::testing::{mock_row_schema, MockCursor, MockPayload, MockPlan};
    use datagrep_api::driver::Cursor;
    use std::time::Duration;

    async fn until(deadline_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(deadline_ms) {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        cond()
    }

    fn no_spill(total: usize) -> MemoryPolicy {
        MemoryPolicy {
            total_result_budget: total,
            per_query_hot: total,
            hot_window_rows: usize::MAX,
            soft_row_cap: 500_000,
            spill: SpillPolicy::Disabled,
        }
    }

    fn start(plan: MockPlan, policy: MemoryPolicy, budget: GlobalBudget) -> Arc<ResultStore> {
        let (cursor, _counters) = MockCursor::standalone(plan);
        start_with(Box::new(cursor), policy, budget)
    }

    fn start_with(
        cursor: Box<dyn Cursor>,
        policy: MemoryPolicy,
        budget: GlobalBudget,
    ) -> Arc<ResultStore> {
        let shape = cursor.shape().clone();
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::channel(DATA_CHANNEL_BOUND);
        let feeder = spawn_feeder(cursor, tx, policy.feeder_policy(500), cancel.clone());
        ResultStore::spawn(shape, rx, feeder, policy, budget, cancel)
    }

    #[tokio::test]
    async fn finite_result_completes_and_windows_resolve() {
        let plan = MockPlan {
            batches: Some(4),
            rows_per_batch: 25,
            ..MockPlan::default()
        };
        let budget = GlobalBudget::new(16 * 1024 * 1024);
        let store = start(plan, no_spill(16 * 1024 * 1024), budget.clone());

        assert!(until(2_000, || store.state().phase.is_terminal()).await);
        let state = store.state();
        assert_eq!(state.phase, StorePhase::Complete);
        assert_eq!(state.rows, 100);
        assert_eq!(state.batches, 4);
        assert!(
            state.first_batch_micros.is_some(),
            "time-to-first-batch is measured here"
        );
        assert!(budget.used() > 0, "resident bytes are charged");

        // A window inside one chunk.
        let w = store.get_rows(10..20).await;
        assert_eq!(w.status, WindowStatus::Ready);
        assert_eq!(w.rows(), 10);
        assert_eq!(w.slices.len(), 1);

        let w = store.get_rows(20..80).await;
        assert_eq!(w.status, WindowStatus::Ready);
        assert_eq!(w.rows(), 60);
        assert_eq!(
            w.slices.len(),
            4,
            "one borrowed slice per 25-row chunk touched"
        );
        assert_eq!(w.slices[0].first_row(), 20);

        // Past the end of a finished result: nothing is missing.
        let w = store.get_rows(100..120).await;
        assert_eq!(w.status, WindowStatus::Ready);
        assert!(w.is_empty());

        store.close().await;
        assert!(
            budget.used() > 0,
            "a closed-but-open result still holds its bytes"
        );
        drop(store);
        assert!(
            until(2_000, || budget.used() == 0).await,
            "dropping the result set did not return its bytes"
        );
    }

    #[tokio::test]
    async fn global_budget_parks_the_second_store_until_the_first_drops() {
        // Size the budget in units of one real converted chunk.
        let probe = rows_to_record_batch(
            &mock_row_schema(),
            (0..200)
                .map(|i| {
                    vec![
                        Value::I64(i),
                        Value::Str(Arc::from(format!("name-{i}"))),
                        Value::Str(Arc::from("active")),
                    ]
                })
                .collect(),
        );
        let chunk = probe.get_array_memory_size();
        let budget = GlobalBudget::new(chunk * 3);
        let policy = no_spill(chunk * 3);

        // First store: two chunks, then done. It keeps holding its bytes.
        let first = start(
            MockPlan {
                batches: Some(2),
                rows_per_batch: 200,
                ..MockPlan::default()
            },
            policy.clone(),
            budget.clone(),
        );
        assert!(until(2_000, || first.state().phase == StorePhase::Complete).await);
        let held = budget.used();
        assert!(held > 0, "the first result set holds budget");

        let second = start(
            MockPlan {
                rows_per_batch: 200,
                ..MockPlan::infinite(200)
            },
            policy,
            budget.clone(),
        );
        assert!(
            until(2_000, || matches!(
                second.state().phase,
                StorePhase::Parked(ParkReason::MemoryBudget)
            ))
            .await,
            "second store did not park; phase = {:?}, used = {}/{}",
            second.state().phase,
            budget.used(),
            budget.limit()
        );
        assert_eq!(
            second.feeder().state(),
            FeedState::Parked(ParkReason::MemoryBudget),
            "parking must reach the driver, not just the store"
        );
        assert!(budget.used() <= budget.limit(), "budget was overrun");
        let rows_while_parked = second.state().rows;

        first.close().await;
        drop(first);
        assert!(
            until(2_000, || second.state().rows > rows_while_parked).await,
            "second store did not resume after the first was closed"
        );

        second.close().await;
    }

    #[tokio::test]
    async fn soft_row_cap_surfaces_as_a_capped_store() {
        let policy = MemoryPolicy {
            soft_row_cap: 300,
            ..no_spill(16 * 1024 * 1024)
        };
        let store = start(
            MockPlan {
                rows_per_batch: 100,
                ..MockPlan::infinite(100)
            },
            policy,
            GlobalBudget::new(16 * 1024 * 1024),
        );
        assert!(until(2_000, || store.state().phase == StorePhase::Capped).await);
        assert_eq!(store.state().rows, 300);

        let w = store.get_rows(299..400).await;
        assert_eq!(w.status, WindowStatus::Capped);
        assert_eq!(w.rows(), 1, "the rows that do exist are still delivered");
        store.close().await;
    }

    #[tokio::test]
    async fn documents_are_not_converted_to_arrow() {
        let store = start(
            MockPlan {
                batches: Some(2),
                rows_per_batch: 10,
                payload: MockPayload::Docs,
                ..MockPlan::default()
            },
            no_spill(16 * 1024 * 1024),
            GlobalBudget::new(16 * 1024 * 1024),
        );
        assert!(until(2_000, || store.state().phase.is_terminal()).await);
        let state = store.state();
        assert_eq!(state.rows, 20);
        assert!(
            state
                .chunks
                .iter()
                .all(|c| matches!(c.body, ChunkBody::Docs(_))),
            "documents must not be stored as RecordBatches"
        );

        let w = store.get_rows(5..15).await;
        assert_eq!(w.status, WindowStatus::Ready);
        assert!(matches!(w.slices[0], WindowSlice::Docs { .. }));
        assert_eq!(w.rows(), 10);
        store.close().await;
    }

    #[tokio::test]
    async fn a_window_past_the_frontier_is_pending_and_resumes_the_feeder() {
        let policy = MemoryPolicy {
            hot_window_rows: 100,
            ..no_spill(16 * 1024 * 1024)
        };
        let store = start(
            MockPlan {
                rows_per_batch: 50,
                ..MockPlan::infinite(50)
            },
            policy,
            GlobalBudget::new(16 * 1024 * 1024),
        );
        // With spill disabled and a 100-row hot window, the store parks itself.
        assert!(
            until(2_000, || matches!(
                store.state().phase,
                StorePhase::Parked(ParkReason::HotWindow)
            ))
            .await,
            "store did not park on the hot window; phase = {:?}",
            store.state().phase
        );
        let frontier = store.state().rows;

        let w = store.get_rows(frontier + 500..frontier + 600).await;
        assert_eq!(w.status, WindowStatus::Pending);
        assert!(w.is_empty());
        assert!(
            until(2_000, || store.state().rows > frontier).await,
            "get_rows past the frontier did not resume the feeder"
        );
        store.close().await;
    }

    #[tokio::test]
    async fn spilled_chunks_are_read_back_transparently() {
        let policy = MemoryPolicy {
            total_result_budget: 16 * 1024 * 1024,
            per_query_hot: 1, // force every chunk but the newest to spill
            hot_window_rows: usize::MAX,
            soft_row_cap: 500_000,
            spill: SpillPolicy::Enabled {
                dir: std::env::temp_dir(),
                max_bytes: 64 * 1024 * 1024,
            },
        };
        let store = start(
            MockPlan {
                batches: Some(5),
                rows_per_batch: 40,
                ..MockPlan::default()
            },
            policy,
            GlobalBudget::new(16 * 1024 * 1024),
        );
        assert!(until(3_000, || store.state().phase.is_terminal()).await);
        let state = store.state();
        assert_eq!(state.rows, 200);
        assert!(state.spilled_bytes > 0, "nothing was actually spilled");
        assert!(
            state
                .chunks
                .iter()
                .any(|c| matches!(c.body, ChunkBody::Spilled { .. })),
            "expected at least one evicted chunk"
        );

        let w = store.get_rows(0..80).await;
        assert_eq!(w.status, WindowStatus::Ready);
        assert_eq!(w.rows(), 80, "spilled rows must come back");
        store.close().await;
    }

    #[tokio::test]
    async fn cancel_leaves_a_truthful_store_and_frees_the_budget() {
        let budget = GlobalBudget::new(16 * 1024 * 1024);
        let store = start(
            MockPlan {
                batch_delay: Some(Duration::from_millis(5)),
                rows_per_batch: 20,
                ..MockPlan::infinite(20)
            },
            no_spill(16 * 1024 * 1024),
            budget.clone(),
        );
        assert!(until(2_000, || store.state().rows >= 40).await);

        store.stop();
        assert!(until(2_000, || store.state().phase == StorePhase::Cancelled).await);
        let rows = store.state().rows;
        assert!(rows >= 40);

        let w = store.get_rows(0..20).await;
        assert_eq!(w.status, WindowStatus::Ready, "what arrived is still there");
        let w = store.get_rows(rows + 100..rows + 200).await;
        assert_eq!(w.status, WindowStatus::Cancelled, "and the rest says why");

        store.close().await;
        drop(store);
        assert!(until(2_000, || budget.used() == 0).await, "budget leaked");
    }

    #[test]
    fn global_budget_never_overruns() {
        let b = GlobalBudget::new(100);
        assert!(b.try_reserve(60));
        assert!(!b.try_reserve(50), "would exceed the limit");
        assert!(b.try_reserve(40), "exact fit is allowed");
        assert_eq!(b.used(), 100);
        assert_eq!(b.available(), 0);
        assert!(!b.try_reserve(1));
        b.release(40);
        assert_eq!(b.used(), 60);
        assert!(b.try_reserve(40));
        b.release(1_000_000);
        assert_eq!(b.used(), 0, "over-release must not underflow");
    }

    #[test]
    fn dropping_a_lease_returns_its_bytes() {
        let budget = GlobalBudget::new(1_000);
        {
            let lease = BudgetLease::new(budget.clone());
            assert!(lease.try_reserve(400));
            assert!(lease.try_reserve(300));
            assert_eq!(budget.used(), 700);
            lease.release(300);
            assert_eq!(budget.used(), 400);
        }
        assert_eq!(budget.used(), 0);
    }
}
