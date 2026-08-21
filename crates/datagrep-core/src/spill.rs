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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkLoc {
    offset: u64,
    len: u64,
    rows: usize,
}

struct SpillFile {
    file: Mutex<File>,
    path: PathBuf,
    unlink_on_drop: bool,
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        if self.unlink_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

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

#[derive(Clone)]
pub struct SpillWriter {
    inner: Arc<SpillInner>,
}

static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

impl SpillWriter {
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

    pub fn in_temp_dir(schema: SchemaRef, max_bytes: u64) -> Result<Self, SpillError> {
        Self::create(&std::env::temp_dir(), schema, max_bytes)
    }

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

    pub fn reader(&self) -> SpillReader {
        SpillReader {
            inner: self.inner.clone(),
        }
    }

    pub fn bytes(&self) -> u64 {
        lock(&self.inner.state).bytes
    }

    pub fn len(&self) -> usize {
        lock(&self.inner.state).chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn schema(&self) -> SchemaRef {
        self.inner.schema.clone()
    }

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

#[derive(Clone)]
pub struct SpillReader {
    inner: Arc<SpillInner>,
}

impl SpillReader {
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

    pub fn rows(&self, index: usize) -> Option<usize> {
        lock(&self.inner.state).chunks.get(index).map(|c| c.rows)
    }

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
