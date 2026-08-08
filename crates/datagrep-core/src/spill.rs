//! Result spill to disk (design §3.2, §3.3).
//!
//! > "Spill: append-only Arrow IPC per result set in `$TMPDIR`, `unlink`ed
//! > immediately after creation on Unix so a crash can't leak files."
//!
//! Two properties are load-bearing and everything else here is subordinate to
//! them:
//!
//! 1. **The file is unlinked the instant it exists** on unix. From then on the
//!    only reference is our own descriptor: `kill -9`, a panic, or a power cut
//!    reclaims the bytes — no orphaned gigabytes in `/tmp`, ever. A user should
//!    never have to clean up after this app.
//! 2. **Append-only, with an in-memory chunk index.** Each chunk is written as
//!    its own self-contained Arrow IPC stream at a recorded offset, so
//!    [`SpillReader::read`] is a seek plus one decode. The alternative — an
//!    Arrow IPC *file* with a footer — cannot be appended to while it is being
//!    read, which is exactly what a streaming result set does.
//!
//! The per-chunk schema repetition costs a few hundred bytes per chunk. Design
//! §3.2 calls this path "correctness over speed" and that is the trade taken.
//!
//! Spill I/O is blocking; callers run it on the blocking pool (§3.4). Every
//! method here takes `&self` so an `Arc` clone can be moved into
//! `spawn_blocking`.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arrow_array::RecordBatch;
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{ArrowError, SchemaRef};

use crate::lock;

/// Failure modes of the spill path. Distinct from `DbError` because a spill
/// failure is ours, never the server's — the store degrades by keeping the
/// chunk resident (or parking) rather than blaming the driver.
#[derive(Debug, thiserror::Error)]
pub enum SpillError {
    #[error("spill i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("arrow error while spilling: {0}")]
    Arrow(#[from] ArrowError),
    #[error("spill file limit reached ({limit} bytes)")]
    LimitReached { limit: u64 },
    #[error("no spilled chunk at index {index}")]
    NoSuchChunk { index: usize },
    #[error("spilled chunk {index} decoded to nothing")]
    EmptyChunk { index: usize },
}

/// Where one chunk lives inside the spill file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkLoc {
    offset: u64,
    len: u64,
    rows: usize,
}

/// The file plus the lock that serialises seek+read/write pairs. Writer and
/// readers share one descriptor because on unix the path is already gone, so
/// re-opening is not an option.
struct SpillFile {
    file: Mutex<File>,
    /// Retained only for diagnostics, and for the non-unix deferred unlink.
    path: PathBuf,
    /// True only on platforms where the file could not be unlinked at creation;
    /// it is then removed on drop, which a crash can defeat. Unix never gets
    /// here.
    unlink_on_drop: bool,
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        if self.unlink_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Shared writer state: the chunk index and the running byte total.
#[derive(Debug, Default)]
struct WriterState {
    chunks: Vec<ChunkLoc>,
    bytes: u64,
}

struct SpillInner {
    file: SpillFile,
    schema: SchemaRef,
    state: Mutex<WriterState>,
    max_bytes: u64,
}

/// Append-only Arrow IPC sink for one result set. Cheap to clone (`Arc`), so a
/// clone can be handed to `spawn_blocking`.
#[derive(Clone)]
pub struct SpillWriter {
    inner: Arc<SpillInner>,
}

/// Monotonic suffix so two spills created in the same nanosecond cannot collide.
static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

impl SpillWriter {
    /// Create (and on unix immediately unlink) a spill file in `dir`.
    ///
    /// The window between `create_new` and `remove_file` is the only moment the
    /// file has a name; it is created with `create_new` so nothing can be
    /// pre-placed at the path and adopted by us.
    pub fn create(dir: &Path, schema: SchemaRef, max_bytes: u64) -> Result<Self, SpillError> {
        std::fs::create_dir_all(dir)?;
        let seq = SPILL_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            "datagrep-spill-{}-{seq}-{nanos}.arrows",
            std::process::id()
        ));

        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;

        // §3.2: unlink immediately so a crash cannot leak the file. On unix the
        // descriptor keeps the inode alive; the bytes vanish when we drop it.
        let unlink_on_drop = if cfg!(unix) {
            std::fs::remove_file(&path)?;
            false
        } else {
            true
        };

        tracing::debug!(?path, max_bytes, unlinked = !unlink_on_drop, "spill opened");
        Ok(Self {
            inner: Arc::new(SpillInner {
                file: SpillFile {
                    file: Mutex::new(file),
                    path,
                    unlink_on_drop,
                },
                schema,
                state: Mutex::new(WriterState::default()),
                max_bytes,
            }),
        })
    }

    /// A spill file in the process temp directory.
    pub fn in_temp_dir(schema: SchemaRef, max_bytes: u64) -> Result<Self, SpillError> {
        Self::create(&std::env::temp_dir(), schema, max_bytes)
    }

    /// Append one chunk; returns its index for [`SpillReader::read`].
    ///
    /// The batch is encoded into memory first so a half-written chunk can never
    /// reach the file: either the whole self-contained IPC stream lands or the
    /// offset table is not advanced.
    pub fn append(&self, batch: &RecordBatch) -> Result<usize, SpillError> {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = StreamWriter::try_new(&mut buf, &self.inner.schema)?;
            w.write(batch)?;
            w.finish()?;
        }

        let mut state = lock(&self.inner.state);
        if state.bytes + buf.len() as u64 > self.inner.max_bytes {
            return Err(SpillError::LimitReached {
                limit: self.inner.max_bytes,
            });
        }

        let offset = {
            let mut file = lock(&self.inner.file.file);
            let offset = file.seek(SeekFrom::End(0))?;
            file.write_all(&buf)?;
            file.flush()?;
            offset
        };

        state.chunks.push(ChunkLoc {
            offset,
            len: buf.len() as u64,
            rows: batch.num_rows(),
        });
        state.bytes += buf.len() as u64;
        let index = state.chunks.len() - 1;
        tracing::trace!(index, bytes = buf.len(), rows = batch.num_rows(), "spilled");
        Ok(index)
    }

    /// A reader over everything appended so far. Cheap: it shares the chunk
    /// index and the descriptor, so chunks appended later are visible too.
    pub fn reader(&self) -> SpillReader {
        SpillReader {
            inner: self.inner.clone(),
        }
    }

    /// Bytes written to the spill file.
    pub fn bytes(&self) -> u64 {
        lock(&self.inner.state).bytes
    }

    /// Chunks appended so far.
    pub fn len(&self) -> usize {
        lock(&self.inner.state).chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The schema every chunk in this file shares.
    pub fn schema(&self) -> SchemaRef {
        self.inner.schema.clone()
    }

    /// Remaining headroom before [`SpillError::LimitReached`].
    pub fn remaining(&self) -> u64 {
        self.inner.max_bytes.saturating_sub(self.bytes())
    }
}

impl fmt::Debug for SpillWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpillWriter")
            .field("chunks", &self.len())
            .field("bytes", &self.bytes())
            .field("max_bytes", &self.inner.max_bytes)
            .finish()
    }
}

/// Random-access reader over a [`SpillWriter`]'s chunks. Reading never disturbs
/// the writer: both hold the same descriptor behind one mutex and always seek
/// before they touch it.
#[derive(Clone)]
pub struct SpillReader {
    inner: Arc<SpillInner>,
}

impl SpillReader {
    /// Read back chunk `index` exactly as it was appended.
    pub fn read(&self, index: usize) -> Result<RecordBatch, SpillError> {
        let loc = *lock(&self.inner.state)
            .chunks
            .get(index)
            .ok_or(SpillError::NoSuchChunk { index })?;

        let mut buf = vec![0u8; loc.len as usize];
        {
            let mut file = lock(&self.inner.file.file);
            file.seek(SeekFrom::Start(loc.offset))?;
            file.read_exact(&mut buf)?;
        }

        let mut reader = StreamReader::try_new(Cursor::new(buf), None)?;
        match reader.next() {
            Some(batch) => Ok(batch?),
            None => Err(SpillError::EmptyChunk { index }),
        }
    }

    /// Row count of a spilled chunk, without decoding it.
    pub fn rows(&self, index: usize) -> Option<usize> {
        lock(&self.inner.state).chunks.get(index).map(|c| c.rows)
    }

    /// Chunks currently readable.
    pub fn len(&self) -> usize {
        lock(&self.inner.state).chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for SpillReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpillReader")
            .field("chunks", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::rows_to_record_batch;
    use datagrep_api::shape::{FieldDef, FieldFlags, LogicalType, RowSchema};
    use datagrep_api::value::Value;

    fn sample(offset: i64, n: i64) -> RecordBatch {
        let schema = RowSchema {
            fields: vec![
                FieldDef {
                    name: Arc::from("id"),
                    logical: LogicalType::I64,
                    flags: FieldFlags::empty(),
                    native_type: None,
                },
                FieldDef {
                    name: Arc::from("name"),
                    logical: LogicalType::Str,
                    flags: FieldFlags::NULLABLE,
                    native_type: None,
                },
            ],
            identity: None,
        };
        let rows = (0..n)
            .map(|i| {
                vec![
                    Value::I64(offset + i),
                    if i % 3 == 0 {
                        Value::Null
                    } else {
                        Value::Str(Arc::from(format!("row-{}", offset + i)))
                    },
                ]
            })
            .collect();
        rows_to_record_batch(&schema, rows)
    }

    /// Test 8: batches written to the spill file come back byte-identical, in
    /// any order — the property the windowed store depends on when a scroll
    /// lands on a chunk that was evicted from memory (design §3.2).
    #[test]
    fn spill_round_trip_is_exact() {
        let originals: Vec<RecordBatch> = (0..5).map(|i| sample(i * 100, 40)).collect();
        let writer = SpillWriter::in_temp_dir(originals[0].schema(), 64 * 1024 * 1024)
            .expect("create spill");

        for (i, b) in originals.iter().enumerate() {
            assert_eq!(writer.append(b).expect("append"), i);
        }
        assert_eq!(writer.len(), 5);
        assert!(writer.bytes() > 0);

        let reader = writer.reader();
        // Out of order, and repeated: random access must not be positional.
        for i in [3usize, 0, 4, 1, 3, 2] {
            let back = reader.read(i).expect("read back");
            assert_eq!(back, originals[i], "chunk {i} differs after round-trip");
            assert_eq!(reader.rows(i), Some(40));
        }
        assert!(matches!(
            reader.read(9),
            Err(SpillError::NoSuchChunk { index: 9 })
        ));
    }

    /// §3.2: the file must be nameless the moment it exists, so a crash cannot
    /// leave gigabytes behind in `$TMPDIR`.
    #[cfg(unix)]
    #[test]
    fn spill_file_is_unlinked_immediately() {
        let dir = std::env::temp_dir();
        let before = count_spill_files(&dir);
        let writer = SpillWriter::in_temp_dir(sample(0, 1).schema(), 1 << 20).expect("create");
        writer.append(&sample(0, 10)).expect("append");
        assert_eq!(
            count_spill_files(&dir),
            before,
            "a named spill file is visible on disk"
        );
        // Still fully readable through the surviving descriptor.
        assert_eq!(writer.reader().read(0).expect("read").num_rows(), 10);
    }

    #[cfg(unix)]
    fn count_spill_files(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with("datagrep-spill-")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// The spill budget is a hard ceiling: past it the store must keep chunks
    /// resident or park, never grow the file without bound.
    #[test]
    fn spill_honours_its_byte_limit() {
        let batch = sample(0, 200);
        let writer = SpillWriter::in_temp_dir(batch.schema(), 1).expect("create");
        assert!(matches!(
            writer.append(&batch),
            Err(SpillError::LimitReached { limit: 1 })
        ));
        assert_eq!(writer.len(), 0, "a rejected chunk is not indexed");
        assert_eq!(writer.bytes(), 0);
    }

    /// Interleaving reads and appends must not corrupt either side — the store
    /// reads back an old chunk while the feeder is still spilling new ones.
    #[test]
    fn reads_interleave_with_appends() {
        let first = sample(0, 20);
        let writer = SpillWriter::in_temp_dir(first.schema(), 1 << 20).expect("create");
        writer.append(&first).expect("append");
        let reader = writer.reader();

        for i in 1..5 {
            let b = sample(i * 100, 20);
            writer.append(&b).expect("append");
            assert_eq!(
                reader.read(0).expect("read"),
                first,
                "chunk 0 stayed intact"
            );
            assert_eq!(reader.read(i as usize).expect("read"), b);
        }
        assert_eq!(reader.len(), 5);
    }
}
