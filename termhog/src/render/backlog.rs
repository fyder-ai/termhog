//! Holds output the render thread hasn't processed yet.
//!
//! Output is passed along in memory while the render thread keeps up. When
//! it falls far behind (a program printing faster than the emulator parses),
//! further chunks go to an unnamed temporary file instead, and only their
//! position travels through the queue, so memory stays flat however far
//! behind it gets. Nothing is skipped: every byte is rendered, in order.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// In-memory output the render thread may fall behind by before spilling to
/// disk.
const MEMORY_LIMIT: usize = 16 * 1024 * 1024;

/// A chunk of output, held in memory or spilled to the backlog file.
pub enum Chunk {
    Memory(Vec<u8>),
    Spilled { offset: u64, len: usize },
}

/// Shared by the readers (which add output) and the render thread (which
/// takes it).
#[derive(Clone)]
pub struct Backlog(Arc<Shared>);

struct Shared {
    /// Bytes held in memory, not yet rendered.
    in_memory: AtomicUsize,
    spill: Mutex<Spill>,
}

struct Spill {
    file: File,
    /// Where the next chunk is appended.
    end: u64,
    /// Bytes written but not yet rendered.
    pending: u64,
}

impl Backlog {
    /// An empty backlog, with a new unnamed temporary file to spill to.
    pub fn new() -> io::Result<Backlog> {
        let spill = Spill {
            file: tempfile::tempfile()?,
            end: 0,
            pending: 0,
        };
        Ok(Backlog(Arc::new(Shared {
            in_memory: AtomicUsize::new(0),
            spill: Mutex::new(spill),
        })))
    }

    /// Hold `bytes` for the render thread: in memory while it keeps up, on
    /// disk once it's far behind. If writing to disk fails (a full disk,
    /// say), they're kept in memory rather than lost.
    pub fn hold(&self, bytes: Vec<u8>) -> Chunk {
        let behind = self.0.in_memory.load(Ordering::Relaxed) + bytes.len() > MEMORY_LIMIT;
        if behind {
            if let Some(chunk) = self.spill(&bytes) {
                return chunk;
            }
        }
        self.0.in_memory.fetch_add(bytes.len(), Ordering::Relaxed);
        Chunk::Memory(bytes)
    }

    /// Take a chunk's bytes back, releasing what held them.
    pub fn take(&self, chunk: Chunk) -> io::Result<Vec<u8>> {
        match chunk {
            Chunk::Memory(bytes) => {
                self.0.in_memory.fetch_sub(bytes.len(), Ordering::Relaxed);
                Ok(bytes)
            }
            Chunk::Spilled { offset, len } => {
                let mut spill = self
                    .0
                    .spill
                    .lock()
                    .map_err(|_| io::Error::other("poisoned"))?;
                let mut bytes = vec![0; len];
                let result = spill.file.read_exact_at(&mut bytes, offset);
                spill.pending -= len as u64;
                if spill.pending == 0 {
                    // Everything written has been rendered: start over.
                    spill.end = 0;
                    let _ = spill.file.set_len(0);
                }
                result.map(|()| bytes)
            }
        }
    }

    /// Append `bytes` to the backlog file. `None` if it can't be written.
    fn spill(&self, bytes: &[u8]) -> Option<Chunk> {
        let mut spill = self.0.spill.lock().ok()?;
        let offset = spill.end;
        spill.file.write_all_at(bytes, offset).ok()?;
        spill.end += bytes.len() as u64;
        spill.pending += bytes.len() as u64;
        Some(Chunk::Spilled {
            offset,
            len: bytes.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spills_once_far_behind_and_keeps_order() {
        let backlog = Backlog::new().unwrap();
        let big = vec![b'a'; MEMORY_LIMIT];
        let first = backlog.hold(big.clone());
        assert!(matches!(first, Chunk::Memory(_)));
        let second = backlog.hold(b"next".to_vec());
        assert!(matches!(second, Chunk::Spilled { .. }));
        assert_eq!(backlog.take(first).unwrap(), big);
        assert_eq!(backlog.take(second).unwrap(), b"next");
        // Caught up: memory is used again.
        assert!(matches!(backlog.hold(b"x".to_vec()), Chunk::Memory(_)));
    }
}
