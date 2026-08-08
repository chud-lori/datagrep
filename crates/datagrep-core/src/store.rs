//! The windowed result store — the other half of the memory contract (§3.2).
//!
//! > "**Invariant: `datagrep` never holds a result set larger than
//! > `total_result_budget`, regardless of the query.** `SELECT * FROM events`
//! > on a 2 TB table streams, spills, and caps. It does not OOM and it does not
//! > freeze, because chunk 1 renders before chunk 2 is requested."
//!
//! One store per result set. A **single writer task** consumes the feeder's
//! bounded channel and is the only thing that ever mutates the store; readers
//! take `Arc` snapshots of [`StoreState`] and never take a lock on the write
//! path. That is why a 4 K scroll fling does not contend with a streaming
//! query (§3.4: "store tasks — one per result set — single writer, no lock").
//!
//! **Storage, with one important exception** (§3.2). Tabular results become
//! Arrow [`RecordBatch`]es at this boundary: columnar collapses per-cell enum
//! overhead, nulls are validity bits, and Arrow IPC is simultaneously the spill
//! format and the export format. Documents deliberately do **not** become
//! Arrow — forcing heterogeneous documents into a columnar schema means either
//! a union-of-everything or a re-encode on every schema delta — so they stay in
//! [`DocSegment`], whose shape leaves room for the arena variant the design
//! calls for.
//!
//! Admission is three gates, checked in this order:
//!
//! 1. the **global** budget shared by every store ([`GlobalBudget`]),
//! 2. this query's **hot** budget (`per_query_hot`, `hot_window_rows`),
//! 3. the **soft row cap**, which the feeder enforces at the source.
//!
//! When a gate closes, the store first tries to spill the oldest resident chunk
//! and only then parks the feeder — which stops the driver reading its socket,
//! which stops the server. Nothing is ever dropped on the floor.

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

/// Where overflow goes when a result set outgrows its hot budget (§3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpillPolicy {
    /// No disk. The store parks the feeder instead of overflowing — correct,
    /// just less pleasant. This is what a read-only or ephemeral environment
    /// gets.
    Disabled,
    /// Append-only Arrow IPC in `dir`, unlinked at creation (see
    /// [`crate::spill`]).
    Enabled { dir: PathBuf, max_bytes: u64 },
}

/// The published memory contract, as data (design §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPolicy {
    /// Across **all** result sets, enforced by [`GlobalBudget`].
    pub total_result_budget: usize,
    /// Resident bytes for one result set before it starts spilling.
    pub per_query_hot: usize,
    /// Resident rows around the viewport for one result set.
    pub hot_window_rows: usize,
    /// Rows after which the feeder stops and the UI offers
    /// "[Load more] [Export all]".
    pub soft_row_cap: u64,
    pub spill: SpillPolicy,
}

impl Default for MemoryPolicy {
    /// Exactly the numbers in design §3.2.
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
    /// The feeder policy implied by this memory policy plus the connection's
    /// advertised starting fetch size.
    pub fn feeder_policy(&self, default_fetch_rows: u32) -> crate::feeder::FeederPolicy {
        crate::feeder::FeederPolicy {
            soft_row_cap: self.soft_row_cap,
            ..crate::feeder::FeederPolicy::for_fetch_rows(default_fetch_rows)
        }
    }
}

/// **The one counter every result set in the process shares** (§3.2
/// `total_result_budget`): an `Arc<AtomicUsize>` of resident result bytes, plus
/// the limit and a `Notify` that wakes stores parked on it.
///
/// The notify is what makes the budget a *queue* rather than a deadlock: when
/// one result set shrinks or closes, every store waiting on memory is woken to
/// try again. Without it, closing a tab would free bytes nobody ever noticed.
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

    /// The budget described by a [`MemoryPolicy`].
    pub fn from_policy(policy: &MemoryPolicy) -> Self {
        Self::new(policy.total_result_budget)
    }

    /// Reserve `bytes`, or fail without reserving anything. A CAS loop, not a
    /// lock: this is on the admission path of every chunk.
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

    /// Give bytes back and wake everyone parked on the budget.
    pub fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.used
            .fetch_sub(bytes.min(self.used()), Ordering::AcqRel);
        self.freed.notify_waiters();
    }

    /// Resident result bytes across every store.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Headroom before the next reservation fails.
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

/// One store's share of the global budget. Releasing on `Drop` — rather than
/// at the end of the writer loop — is what makes "close the tab, get the memory
/// back" true even when the store is dropped mid-stream (design §5, P7).
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

/// A run of non-tabular rows, kept **out of Arrow on purpose** (design §3.2).
///
/// Sparse documents in a columnar schema cost either a union-of-everything or a
/// re-encode per schema delta, and both lose the `Absent`/`Null` distinction
/// that makes a document grid truthful. The variants here are the decoded form;
/// the design's target — an arena of the driver's original encoded bytes plus a
/// lazy offset index, decoded only for the visible viewport — slots in as a
/// further variant without changing any of this module's interfaces:
///
/// ```ignore
/// Raw { bytes: Bytes, offsets: Vec<u32> }
/// ```
///
/// [`crate::store::RowWindow`] already hands out segments rather than values,
/// so the viewport decode has somewhere to live when it lands.
#[derive(Debug, Clone, PartialEq)]
pub enum DocSegment {
    /// Decoded documents, one per row (`Shape::Documents`).
    Values(Vec<Value>),
    /// Key/value rows (`Shape::Pairs`, e.g. Redis `SCAN`/`HGETALL`). Kept as
    /// pairs so the key's own type survives instead of being stringified.
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

    /// Heap footprint estimate, used for admission. Approximate by design: the
    /// budget only needs to be right to within a chunk.
    pub fn byte_size(&self) -> usize {
        match self {
            DocSegment::Values(v) => v.iter().map(value_bytes).sum(),
            DocSegment::Pairs(p) => p.iter().map(|(k, v)| value_bytes(k) + value_bytes(v)).sum(),
        }
    }
}

/// Rough resident size of a value, following `Arc`s once (they are usually not
/// shared across rows in a freshly decoded chunk).
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

/// Where one chunk's rows currently live.
#[derive(Debug, Clone)]
pub enum ChunkBody {
    /// Resident Arrow.
    Table(Arc<RecordBatch>),
    /// Resident documents/pairs (never Arrow — see [`DocSegment`]).
    Docs(Arc<DocSegment>),
    /// Evicted to the spill file; `index` addresses it in the spill reader.
    Spilled { index: usize },
}

/// One chunk of the result set, in arrival order.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Row index of this chunk's first row within the result set.
    pub first_row: u64,
    pub rows: usize,
    /// Resident bytes charged to the budget, or 0 once spilled.
    pub bytes: usize,
    pub body: ChunkBody,
}

impl Chunk {
    /// Rows covered by this chunk, as a half-open range.
    pub fn range(&self) -> Range<u64> {
        self.first_row..self.first_row + self.rows as u64
    }
}

/// Lifecycle of a result set, mirroring the feeder's [`FeedState`] once the
/// stream ends. The UI's status line is a rendering of exactly this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorePhase {
    /// Admitting chunks.
    Loading,
    /// Not admitting; the feeder is parked for this reason.
    Parked(ParkReason),
    /// Stopped at the soft row cap — complete up to the cap (§3.2).
    Capped,
    /// The whole result set is resident/spilled; nothing more is coming.
    Complete,
    /// The user stopped it (§3.3).
    Cancelled,
    /// The driver failed; the message is for the status line.
    Failed(Arc<str>),
}

impl StorePhase {
    /// True once nothing more will ever be admitted.
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

/// An immutable snapshot of a result set. Readers clone the `Arc`, never a
/// lock; the writer publishes a new one per admitted chunk.
#[derive(Debug, Clone)]
pub struct StoreState {
    pub phase: StorePhase,
    /// Rows admitted so far.
    pub rows: u64,
    /// Chunks admitted so far.
    pub batches: u64,
    /// Bytes currently resident and charged to the global budget.
    pub resident_bytes: usize,
    /// Bytes written to the spill file.
    pub spilled_bytes: u64,
    /// Micros from store start to the first admitted chunk — the number P8
    /// measures.
    pub first_batch_micros: Option<u64>,
    /// Cells that did not fit their declared column type (see
    /// [`BatchConverter::coerced`]). Non-zero means a driver bug; it is shown,
    /// not swallowed.
    pub coerced: u64,
    /// Affected-row count from an `Ack`-shaped result (INSERT/UPDATE/DDL —
    /// `Shape::Ack { affected, .. }`). An acknowledgement carries no rows, so
    /// without this field the count would die between the driver and the
    /// frontend and every INSERT would read "(0 rows)".
    pub affected: Option<u64>,
    /// The engine's own acknowledgement message, when the shape carried one
    /// (e.g. which count strategy a driver ran). Shown, never embellished.
    pub ack_message: Option<Arc<str>>,
    /// Non-fatal server messages collected from every chunk.
    pub notices: Vec<datagrep_api::driver::Notice>,
    /// Schema evolution the driver reported mid-stream (§3.1). Append-only and
    /// never reordered, so the grid can grow a column without refetching. The
    /// store records them; applying them to a `ViewProjection` is the grid's
    /// job and lands with the document view.
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

    /// Index of the chunk containing `row`, by binary search over arrival
    /// order (chunks are contiguous and non-overlapping by construction).
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

    /// Rows currently in memory (i.e. not spilled).
    pub fn resident_rows(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| !matches!(c.body, ChunkBody::Spilled { .. }))
            .map(|c| c.rows)
            .sum()
    }
}

/// One contiguous run of rows handed to the grid. Slices borrow the store's
/// `Arc`s: no result data is ever copied to answer a scroll.
#[derive(Debug, Clone)]
pub enum WindowSlice {
    Table {
        first_row: u64,
        batch: Arc<RecordBatch>,
        /// Row offset within `batch`.
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

/// How much of the requested window the store could actually answer. The UI
/// renders each of these differently and **never** silently shows fewer rows
/// than were asked for (design §5.1: "shows the boundary honestly").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowStatus {
    /// Every requested row is in the slices.
    Ready,
    /// Some rows are here; the rest have not been fetched yet. The feeder has
    /// been resumed and the caller should ask again.
    Partial,
    /// None of the requested rows exist yet; the feeder has been resumed.
    Pending,
    /// The window is past the soft row cap — there is nothing more without
    /// "[Load more]" (§3.2).
    Capped,
    /// The window is past the end of a stopped result set.
    Cancelled,
    /// The window is past the end of a failed result set.
    Failed(Arc<str>),
}

/// The answer to `get_rows(qid, 12000..12100)` (design §3.2, §3.6).
#[derive(Debug, Clone)]
pub struct RowWindow {
    pub range: Range<u64>,
    pub status: WindowStatus,
    pub slices: Vec<WindowSlice>,
}

impl RowWindow {
    /// Rows actually delivered.
    pub fn rows(&self) -> usize {
        self.slices.iter().map(WindowSlice::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }
}

/// Everything the writer task and the readers share.
struct StoreShared {
    state: watch::Sender<Arc<StoreState>>,
    feeder: FeederHandle,
    lease: BudgetLease,
    policy: MemoryPolicy,
    spill: Mutex<Option<SpillWriter>>,
    /// The Arrow schema every tabular chunk shares, published once the
    /// converter's dictionary-sampling window has closed. Nothing may be
    /// spilled before then: the spill file has exactly one schema, and the
    /// sampling window can still re-encode the chunks already emitted.
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

/// One result set: a single writer task, `Arc` snapshots for readers, and a
/// windowed accessor for the grid.
pub struct ResultStore {
    shared: Arc<StoreShared>,
    cancel: CancellationToken,
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl ResultStore {
    /// Start the writer task for one query's stream.
    ///
    /// `cancel` is the query's node of the token tree (§3.4); cancelling it
    /// stops the store and the feeder together.
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
            run_store(rx, shared.clone(), shape, cancel.clone())
                .instrument(tracing::info_span!("store")),
        );
        Arc::new(Self {
            shared,
            cancel,
            writer: Mutex::new(Some(writer)),
        })
    }

    /// The current snapshot. Cheap: one `Arc` clone.
    pub fn state(&self) -> Arc<StoreState> {
        self.shared.snapshot()
    }

    /// Follow state changes without polling (§3.4).
    pub fn subscribe(&self) -> watch::Receiver<Arc<StoreState>> {
        self.state_sender().subscribe()
    }

    fn state_sender(&self) -> &watch::Sender<Arc<StoreState>> {
        &self.shared.state
    }

    /// The feeder behind this store, for state and for park/resume.
    pub fn feeder(&self) -> &FeederHandle {
        &self.shared.feeder
    }

    /// Rows admitted so far.
    pub fn rows(&self) -> u64 {
        self.state().rows
    }

    /// Resolve a row window (design §3.2 window resolver, §3.6).
    ///
    /// - **resident** → an `Arc` slice of the chunk, no copy;
    /// - **spilled** → read back from the Arrow IPC file on the blocking pool;
    /// - **not yet fetched** → [`WindowStatus::Pending`] (or `Partial`) *and*
    ///   the feeder is resumed, so asking for rows is what makes them arrive;
    /// - **past the cap / stopped / failed** → the status says which, because
    ///   an empty grid with no explanation is the incumbents' failure mode.
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
                // Complete but short: the caller asked past the end of a
                // finished result. That is `Ready` — there is nothing missing.
                StorePhase::Complete => WindowStatus::Ready,
                StorePhase::Loading | StorePhase::Parked(_) => {
                    // Asking for rows we do not have is the signal that the
                    // viewport moved: start fetching again (§3.6).
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

    /// The local half of a stop (§3.3): stop the feeder, stop the writer, and
    /// let the budget go. Returns immediately — it does not wait for the tasks.
    pub fn stop(&self) {
        self.cancel.cancel();
        self.shared.feeder.stop();
    }

    /// Stop and wait for the writer to exit, so the budget is provably back.
    /// Used by orderly shutdown and by tests; the stop button uses
    /// [`ResultStore::stop`].
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
    /// P7 ("reclaim 10 s after closing all result tabs") is a `Drop` impl: the
    /// writer is cancelled, and the budget lease is released as soon as it and
    /// the writer are gone.
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

/// Read one spilled chunk on the blocking pool (§3.4: spill I/O never runs on
/// a worker thread).
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

/// The single writer task. Nothing else ever mutates a store.
async fn run_store(
    mut rx: mpsc::Receiver<Batch>,
    shared: Arc<StoreShared>,
    shape: Shape,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let mut converter = table_converter(&shape);

    // An acknowledgement's whole payload lives in its *shape* (§3.1): publish
    // it before consuming the (empty) stream so the count reaches every
    // snapshot, not just the terminal one.
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
            // Nothing storable in this chunk (an Ack, or a shape we do not
            // materialise yet). Still surface anything the server said — a
            // notice or a schema delta is never dropped just because the chunk
            // carried no rows.
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
            // Chunks re-encoded because the dictionary window just closed
            // replace their originals in place; row counts never change.
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

    // The channel closed: the feeder is done, and its terminal state is the
    // truth about why.
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

/// A converted chunk, ready for admission.
struct Admitted {
    rows: usize,
    bytes: usize,
    body: ChunkBody,
    reencoded: Vec<(usize, RecordBatch)>,
}

/// The tabular converter for this cursor's shape, or `None` for a stream that
/// will never produce rows.
fn table_converter(shape: &Shape) -> Option<BatchConverter> {
    match shape {
        Shape::Table(schema) => Some(BatchConverter::new(schema.clone())),
        _ => None,
    }
}

/// A placeholder schema for a driver that emits rows under a shape that never
/// declared one (`Shape::Unknown` narrowed by the first batch, §3.1). Columns
/// are untyped, so every value takes the lossless display path.
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

/// Turn one driver chunk into something storable. `None` means the chunk holds
/// no rows (an `Ack`, an empty payload, or a graph result, which has no store
/// representation until a graph engine lands).
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

/// Gate 1 and 2: charge `bytes` to the global budget, spilling or parking as
/// needed. Returns `false` only when the query was cancelled while waiting.
async fn admit(shared: &Arc<StoreShared>, bytes: usize, cancel: &CancellationToken) -> bool {
    let mut parked = false;
    loop {
        // Register on the budget's notify *before* testing it, so a release
        // landing in between cannot be lost — a store that misses its wakeup
        // is a result set that never finishes loading.
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

/// Gate 2, applied after admission: keep this query's resident set inside
/// `per_query_hot` / `hot_window_rows` by spilling the oldest chunks.
async fn enforce_hot_window(shared: &Arc<StoreShared>) {
    loop {
        let snapshot = shared.snapshot();
        let over_bytes = snapshot.resident_bytes > shared.policy.per_query_hot;
        let over_rows = snapshot.resident_rows() > shared.policy.hot_window_rows;
        if !over_bytes && !over_rows {
            return;
        }
        if !spill_oldest(shared).await {
            // Nothing could be shed. While the dictionary-sampling window is
            // still open its chunks are deliberately unspillable, so a small
            // overshoot — bounded by `SAMPLE_BATCHES` chunks, each already
            // capped at the feeder's 4 MB byte ceiling — is accepted rather
            // than parking a stream that has not yet settled its schema.
            // Once settled, an unsheddable overshoot means spill is off or
            // full, and parking is the honest answer.
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

/// Move the oldest resident Arrow chunk to the spill file and release its
/// bytes. `false` means nothing could be spilled (spill disabled, full, no
/// resident tabular chunk, or a schema mismatch), and the caller must park.
async fn spill_oldest(shared: &Arc<StoreShared>) -> bool {
    let SpillPolicy::Enabled { dir, max_bytes } = shared.policy.spill.clone() else {
        return false;
    };

    // One spill file, one schema. Until the converter settles, the chunks
    // already emitted may still be re-encoded, so none of them may be written.
    let Some(schema) = lock(&shared.settled_schema).clone() else {
        return false;
    };

    let snapshot = shared.snapshot();
    let Some((index, batch, bytes)) = snapshot.chunks.iter().enumerate().find_map(|(i, c)| {
        match &c.body {
            ChunkBody::Table(b) if b.schema() == schema => Some((i, b.clone(), c.bytes)),
            // Documents are not Arrow, so they cannot use the Arrow IPC spill
            // file. They stay resident and the budget is held by parking.
            _ => None,
        }
    }) else {
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

    /// Wire cursor → feeder → store the way `QueryMgr` does.
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

    /// The happy path: a finite result lands complete, addressable, and inside
    /// the budget.
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
        assert!(state.first_batch_micros.is_some(), "P8 is measured here");
        assert!(budget.used() > 0, "resident bytes are charged");

        // A window inside one chunk.
        let w = store.get_rows(10..20).await;
        assert_eq!(w.status, WindowStatus::Ready);
        assert_eq!(w.rows(), 10);
        assert_eq!(w.slices.len(), 1);

        // A window straddling four chunks comes back as four borrowed
        // slices — nothing is copied to answer a scroll.
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

        // Closing stops the tasks but keeps the data readable; the budget is
        // returned when the result set itself is dropped (design §5, P7).
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

    /// **Test 4 — the global budget is shared across result sets (§3.2).**
    ///
    /// The second store parks when the budget is exhausted, and resumes the
    /// moment the first one is closed. This is the invariant that makes
    /// "256 MB across ALL result sets" true rather than per-tab wishful
    /// thinking.
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

        // Second store: endless. It can admit at most what is left, then must
        // park — never overrun the shared budget.
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

        // Closing the first result set is what frees the memory — and the
        // second must notice without anyone polling.
        first.close().await;
        drop(first);
        assert!(
            until(2_000, || second.state().rows > rows_while_parked).await,
            "second store did not resume after the first was closed"
        );

        second.close().await;
    }

    /// The soft row cap reaches the store as a phase the status line can show,
    /// and windows past the cap say `Capped` rather than looking like a
    /// still-loading result.
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

    /// Documents keep their own lane: they are never converted to Arrow, and
    /// the `Absent`/`Null` distinction survives the store (§3.2).
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

    /// A window past what has been fetched is `Pending` **and** resumes the
    /// feeder: asking for rows is what makes them arrive (§3.6).
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

    /// Spilled chunks are still addressable: the window resolver reads them
    /// back from the Arrow IPC file and the caller cannot tell the difference.
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

    /// Cancelling mid-stream leaves a truthful store: phase `Cancelled`, the
    /// rows that did arrive still readable, and the budget returned.
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

    /// The budget arithmetic itself: reservations never exceed the limit, and
    /// releases are exact.
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

    /// A lease returns exactly what it took when it is dropped — the mechanism
    /// behind "close the tab, get the memory back".
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
