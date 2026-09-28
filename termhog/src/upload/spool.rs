//! A first-in, first-out store for events awaiting upload.
//!
//! Events are kept as JSON Lines in an unnamed temporary file, which leaves
//! nothing behind even on a crash and keeps memory flat however far uploads
//! fall behind. Without a temporary file, they're kept in memory.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;

use serde_json::Value;

/// Events read back for one upload.
pub struct Batch {
    /// Timestamp of the first event.
    pub first_timestamp: u64,
    pub events: Vec<Value>,
    /// Total serialized size of `events`.
    pub bytes: usize,
    /// Spool position just past the batch, for [`Spool::commit`].
    end: u64,
}

enum Storage {
    File(File),
    Memory(Vec<u8>),
}

pub struct Spool {
    storage: Storage,
    /// Position of the oldest event not yet committed.
    read: u64,
    /// Position where the next event is appended.
    write: u64,
}

/// Once this much at the front has been committed (and it's most of the
/// spool), the rest is moved to the front to reclaim the space.
const COMPACT_AFTER: u64 = 8 * 1024 * 1024;

impl Spool {
    /// An empty spool in an unnamed temporary file, or in memory if no temp
    /// file can be created.
    pub fn new() -> Spool {
        match tempfile::tempfile() {
            Ok(file) => Spool::with_storage(Storage::File(file)),
            Err(_) => Spool::in_memory(),
        }
    }

    /// An empty spool in memory.
    pub fn in_memory() -> Spool {
        Spool::with_storage(Storage::Memory(Vec::new()))
    }

    fn with_storage(storage: Storage) -> Spool {
        Spool {
            storage,
            read: 0,
            write: 0,
        }
    }

    /// Bytes waiting to be uploaded.
    pub fn pending(&self) -> u64 {
        self.write - self.read
    }

    pub fn push(&mut self, event: &Value) -> io::Result<()> {
        // Serialized JSON never contains a raw newline, so it's one line.
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        match &mut self.storage {
            Storage::File(file) => file.write_all_at(&line, self.write)?,
            Storage::Memory(buf) => buf.extend_from_slice(&line),
        }
        self.write += line.len() as u64;
        Ok(())
    }

    /// Read the oldest events, up to `max_bytes` of them (always at least one,
    /// so an event larger than the limit still goes out on its own). `None`
    /// when nothing is pending. Nothing is removed until [`Spool::commit`],
    /// except lines that can't be read back, which are dropped.
    pub fn peek(&mut self, max_bytes: usize) -> io::Result<Option<Batch>> {
        let mut batch = Batch {
            first_timestamp: 0,
            events: Vec::new(),
            bytes: 0,
            end: self.read,
        };
        let mut lines = self.lines()?;
        let mut line = Vec::new();
        loop {
            line.clear();
            if lines.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let len = line.len() - 1;
            if !batch.events.is_empty() && batch.bytes + len > max_bytes {
                break;
            }
            batch.end += line.len() as u64;
            if let Ok(event) = serde_json::from_slice::<Value>(&line) {
                if batch.events.is_empty() {
                    batch.first_timestamp = event["timestamp"].as_u64().unwrap_or(0);
                }
                batch.bytes += len;
                batch.events.push(event);
            }
        }
        drop(lines);
        if batch.events.is_empty() {
            self.commit(&batch);
            return Ok(None);
        }
        Ok(Some(batch))
    }

    /// Call `f` with each pending event's JSON, oldest first, without
    /// removing them.
    pub fn for_each(&mut self, mut f: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        for line in self.lines()?.split(b'\n') {
            f(&line?)?;
        }
        Ok(())
    }

    /// Remove a batch returned by [`Spool::peek`] (uploaded, or given up on).
    pub fn commit(&mut self, batch: &Batch) {
        self.read = batch.end;
        self.reclaim();
    }

    /// The pending lines, oldest first.
    fn lines(&mut self) -> io::Result<Box<dyn BufRead + '_>> {
        let (start, len) = (self.read, self.pending());
        Ok(match &mut self.storage {
            Storage::File(file) => {
                file.seek(SeekFrom::Start(start))?;
                Box::new(BufReader::new(file.take(len)))
            }
            Storage::Memory(buf) => Box::new(Cursor::new(&buf[start as usize..])),
        })
    }

    /// Free the space of committed events: all of it once drained, or by
    /// moving what's left to the front once most of the spool is committed.
    fn reclaim(&mut self) {
        if self.read == self.write {
            self.read = 0;
            self.write = 0;
            match &mut self.storage {
                Storage::File(file) => {
                    let _ = file.set_len(0);
                }
                Storage::Memory(buf) => buf.clear(),
            }
        } else if self.read >= COMPACT_AFTER && self.read > self.pending() {
            // If moving fails, the space is reclaimed on a later attempt.
            if self.move_to_front().is_ok() {
                self.write -= self.read;
                self.read = 0;
            }
        }
    }

    /// Copy the pending lines to the start of the storage and cut it there.
    /// Done in chunks, so memory stays flat.
    fn move_to_front(&mut self) -> io::Result<()> {
        let (start, len) = (self.read, self.pending());
        match &mut self.storage {
            Storage::File(file) => {
                let mut buf = vec![0u8; 1024 * 1024];
                let mut done = 0;
                while done < len {
                    let n = buf.len().min((len - done) as usize);
                    file.read_exact_at(&mut buf[..n], start + done)?;
                    file.write_all_at(&buf[..n], done)?;
                    done += n as u64;
                }
                file.set_len(len)
            }
            Storage::Memory(buf) => {
                buf.drain(..start as usize);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn event(timestamp: u64, size: usize) -> Value {
        json!({ "timestamp": timestamp, "data": "x".repeat(size) })
    }

    #[test]
    fn batches_respect_the_size_limit_and_order() {
        let mut spool = Spool::new();
        for i in 0..5 {
            spool.push(&event(100 + i, 10)).unwrap();
        }
        let one = serde_json::to_vec(&event(100, 10)).unwrap().len();
        let batch = spool.peek(2 * one + 1).unwrap().unwrap();
        assert_eq!(batch.first_timestamp, 100);
        assert_eq!(batch.events.len(), 2);
        spool.commit(&batch);
        let batch = spool.peek(usize::MAX).unwrap().unwrap();
        assert_eq!(batch.first_timestamp, 102);
        assert_eq!(batch.events.len(), 3);
        assert_eq!(batch.events[2], event(104, 10));
        spool.commit(&batch);
        assert_eq!(spool.pending(), 0);
        assert!(spool.peek(usize::MAX).unwrap().is_none());

        // An event over the limit still goes out, alone.
        spool.push(&event(1, 50)).unwrap();
        spool.push(&event(2, 5)).unwrap();
        assert_eq!(spool.peek(10).unwrap().unwrap().events.len(), 1);
    }

    #[test]
    fn compacts_once_most_is_committed() {
        let mut spool = Spool::new();
        for i in 0..12 {
            spool.push(&event(i, 1024 * 1024)).unwrap();
        }
        // Upload ten, while two stay pending: the front is moved out.
        let one = serde_json::to_vec(&event(0, 1024 * 1024)).unwrap().len();
        let batch = spool.peek(10 * one).unwrap().unwrap();
        assert_eq!(batch.events.len(), 10);
        spool.commit(&batch);
        assert_eq!(spool.read, 0);
        let rest = spool.peek(usize::MAX).unwrap().unwrap();
        assert_eq!((rest.first_timestamp, rest.events.len()), (10, 2));
    }
}
