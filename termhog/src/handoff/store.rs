//! Checkpoints waiting for the background uploader, one file each, in
//! `<user cache dir>/termhog/`.
//!
//! A file only appears under its final name once it's completely written.
//! Whoever works on one holds an exclusive lock on it, so any number of
//! uploaders can share the folder without doing the same work twice. Files
//! are readable only by their owner, and ones nobody finishes are deleted
//! after a week, or sooner once the folder passes a size cap.

use std::fs::{self, DirBuilder, File};
use std::io::{self, BufWriter};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rustix::fs::{FlockOperation, flock};
use tempfile::NamedTempFile;
use uuid::Uuid;

/// Checkpoints older than this are deleted unfinished.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Past this total, the oldest checkpoints are deleted unfinished.
const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
const EXTENSION: &str = "pending";

/// A checkpoint this process holds the lock on.
pub struct Claimed {
    pub path: PathBuf,
    /// Open with the lock held. Closing it releases the lock.
    pub file: File,
}

/// The checkpoint folder, which may not exist yet. `None` without a cache
/// dir.
fn path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("termhog"))
}

/// The checkpoint folder, created if needed.
pub fn dir() -> Option<PathBuf> {
    let dir = path()?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()?;
    Some(dir)
}

pub type Writer = BufWriter<NamedTempFile>;

/// Save a new checkpoint, written by `write`, under a fresh name.
pub fn save(write: impl FnOnce(&mut Writer) -> io::Result<()>) -> io::Result<()> {
    let dir = dir().ok_or_else(|| io::Error::other("no cache directory"))?;
    let path = dir.join(format!("{}.{EXTENSION}", Uuid::new_v4()));
    write_new(&path, write).map(drop)
}

/// Replace a claimed checkpoint's contents with what `write` writes, keeping
/// the claim.
pub fn replace(
    claimed: &mut Claimed,
    write: impl FnOnce(&mut Writer) -> io::Result<()>,
) -> io::Result<()> {
    claimed.file = write_new(&claimed.path, write)?;
    Ok(())
}

/// Delete a finished checkpoint.
pub fn remove(claimed: Claimed) {
    let _ = fs::remove_file(&claimed.path);
}

/// Claim every checkpoint no one else is working on, after deleting expired
/// ones.
pub fn claim_all() -> Vec<Claimed> {
    let Some(dir) = dir() else {
        return Vec::new();
    };
    tidy(&dir);
    files(&dir, EXTENSION)
        .into_iter()
        .filter_map(|(path, _)| {
            Some(Claimed {
                file: try_claim(&path)?,
                path,
            })
        })
        .collect()
}

/// Whether any checkpoint is waiting with no one working on it.
pub fn any_unclaimed() -> bool {
    let Some(dir) = path() else {
        return false;
    };
    // Probing takes the lock, which is released again as the file closes.
    files(&dir, EXTENSION)
        .iter()
        .any(|(path, _)| try_claim(path).is_some())
}

/// Write `path` through a temporary file that's renamed into place once
/// complete, so a checkpoint is never seen half-written. The new file is
/// locked before it appears, and returned still locked.
fn write_new(path: &Path, write: impl FnOnce(&mut Writer) -> io::Result<()>) -> io::Result<File> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no folder"))?;
    // Deleted unless it's moved into place. One left by a writer that died
    // is deleted by `tidy`.
    let temp = tempfile::Builder::new().suffix(".tmp").tempfile_in(dir)?;
    flock(temp.as_file(), FlockOperation::NonBlockingLockExclusive)?;
    let mut out = BufWriter::new(temp);
    write(&mut out)?;
    let temp = out.into_inner().map_err(|e| e.into_error())?;
    temp.persist(path).map_err(|e| e.error)
}

/// Open `path` and take its lock, unless someone else holds it.
fn try_claim(path: &Path) -> Option<File> {
    let file = File::open(path).ok()?;
    flock(&file, FlockOperation::NonBlockingLockExclusive).ok()?;
    Some(file)
}

/// The files in `dir` with extension `ext`, oldest first, with their
/// metadata.
fn files(dir: &Path, ext: &str) -> Vec<(PathBuf, fs::Metadata)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == ext))
        .filter_map(|path| Some((path.clone(), fs::metadata(&path).ok()?)))
        .collect();
    files.sort_by_key(|(_, meta)| meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    files
}

/// Delete checkpoints past their age, then the oldest ones while the folder
/// is over its size cap, skipping any being worked on. Also deletes
/// temporary files left by writers that died.
fn tidy(dir: &Path) {
    let checkpoints = files(dir, EXTENSION);
    let mut total: u64 = checkpoints.iter().map(|(_, meta)| meta.len()).sum();
    for (path, meta) in &checkpoints {
        let age = meta.modified().ok().and_then(|t| t.elapsed().ok());
        let expired = age.is_some_and(|age| age > MAX_AGE);
        if (expired || total > MAX_TOTAL_BYTES) && try_claim(path).is_some() {
            let _ = fs::remove_file(path);
            total -= meta.len();
        }
    }
    for (path, _) in files(dir, "tmp") {
        if try_claim(&path).is_some() {
            let _ = fs::remove_file(&path);
        }
    }
}
