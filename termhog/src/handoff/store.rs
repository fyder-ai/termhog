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
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tempfile::NamedTempFile;
use uuid::Uuid;

/// Checkpoints older than this are deleted unfinished.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Past this total, the oldest checkpoints are deleted unfinished.
const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
/// A temporary file unchanged for this long was left by a writer that died.
const ABANDONED_AFTER: Duration = Duration::from_secs(10 * 60);
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
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    // On Windows the user's cache folder is already private to them.
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(&dir).ok()?;
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
    try_lock(temp.as_file())?;
    let mut out = BufWriter::new(temp);
    write(&mut out)?;
    let temp = out.into_inner().map_err(|e| e.into_error())?;
    #[cfg(unix)]
    return temp.persist(path).map_err(|e| e.error);
    // The checkpoint being replaced is open and locked by whoever claimed
    // it, which `persist` can't rename over on Windows.
    #[cfg(windows)]
    {
        let (file, temp_path) = temp.keep().map_err(|e| e.error)?;
        if let Err(e) = crate::util::replace_file(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            return Err(e);
        }
        Ok(file)
    }
}

/// Open `path` and take its lock, unless someone else holds it. The file may
/// have been replaced (or deleted) between opening and locking it, and the
/// lock of a file no longer at `path` guards nothing, so that's `None` too.
fn try_claim(path: &Path) -> Option<File> {
    let file = File::open(path).ok()?;
    try_lock(&file).ok()?;
    is_at(&file, path).then_some(file)
}

/// Take an exclusive lock on `file`, failing at once if someone holds it.
#[cfg(unix)]
fn try_lock(file: &File) -> io::Result<()> {
    use rustix::fs::{FlockOperation, flock};
    Ok(flock(file, FlockOperation::NonBlockingLockExclusive)?)
}

/// Take an exclusive lock on `file`, failing at once if someone holds it.
/// Windows locks are mandatory, but only other handles are kept out, so the
/// holder can still read and replace the file.
#[cfg(windows)]
fn try_lock(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    let flags = LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY;
    // SAFETY: a zeroed OVERLAPPED locks from offset 0, and the call is
    // synchronous, so it doesn't outlive the call.
    let mut overlapped = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            flags,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether the open `file` is the one now at `path`.
fn is_at(file: &File, path: &Path) -> bool {
    File::open(path).is_ok_and(|current| crate::util::same_file(file, &current))
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
        if !older_than(meta, MAX_AGE) && total <= MAX_TOTAL_BYTES {
            continue;
        }
        // Deleted with the lock held, so no one claims it midway.
        if let Some(_claim) = try_claim(path) {
            let _ = fs::remove_file(path);
            total -= meta.len();
        }
    }
    // A writer's temporary file is unlocked for a moment after it's
    // created, so only one left unchanged for a while is a dead writer's.
    for (path, meta) in files(dir, "tmp") {
        if !older_than(&meta, ABANDONED_AFTER) {
            continue;
        }
        if let Some(_claim) = try_claim(&path) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Whether the file was last changed more than `age` ago.
fn older_than(meta: &fs::Metadata, age: Duration) -> bool {
    let changed = meta.modified().ok().and_then(|t| t.elapsed().ok());
    changed.is_some_and(|changed| changed > age)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, Write};

    use super::*;

    /// What the uploader does with a checkpoint it rendered: replace its
    /// contents while holding it, and keep holding it.
    #[test]
    fn a_claimed_checkpoint_can_be_replaced_and_stays_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("one.{EXTENSION}"));
        write_new(&path, |out| out.write_all(b"before")).unwrap();
        let mut claimed = Claimed {
            file: try_claim(&path).unwrap(),
            path: path.clone(),
        };

        replace(&mut claimed, |out| out.write_all(b"after")).unwrap();

        // Read through the claim: Windows locks keep other handles out.
        let mut contents = String::new();
        claimed.file.rewind().unwrap();
        claimed.file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "after");
        assert!(is_at(&claimed.file, &path));
        assert!(try_claim(&path).is_none(), "someone else claimed it");
    }
}
