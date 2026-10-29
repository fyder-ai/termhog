//! Windows helpers: positional file I/O, waiting on processes, pipes, file
//! identity and raw writes.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileExt as _, OpenOptionsExt};
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, CompareObjectHandles, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileRenameInfoEx, GetFileInformationByHandle, SetFileInformationByHandle,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT,
    CreateProcessW, DETACHED_PROCESS, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
    WaitForSingleObject,
};

/// What an open file, pipe or console is borrowed as, named alike on Unix.
pub use std::os::windows::io::AsHandle as AsStream;

/// `read_exact_at`/`write_all_at` as on Unix. Windows' positional calls also
/// move the file cursor, so callers seek before any cursor-based read.
pub trait FileExt {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
}

impl FileExt for File {
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.seek_read(buf, offset)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
            }
        }
        Ok(())
    }

    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.seek_write(buf, offset)? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                n => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
            }
        }
        Ok(())
    }
}

/// Wait for the process `child` to exit. With `nohang`, only checks.
/// Returns whether it has exited.
pub fn wait_exited(child: impl AsHandle, nohang: bool) -> io::Result<bool> {
    wait_handle(child, if nohang { 0 } else { INFINITE })
}

/// Wait up to `timeout_ms` (or INFINITE) for `handle` to be signaled, like a
/// process exiting. Returns whether it was.
pub fn wait_handle(handle: impl AsHandle, timeout_ms: u32) -> io::Result<bool> {
    // SAFETY: the handle is borrowed, so it's open for the call.
    match unsafe { WaitForSingleObject(handle.as_handle().as_raw_handle(), timeout_ms) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

/// An anonymous pipe, as its read end and write end. Neither is inheritable,
/// so only a child given one explicitly gets it.
pub fn pipe() -> io::Result<(File, OwnedHandle)> {
    let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: both handles are owned as soon as the call succeeds.
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        Ok((
            File::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        ))
    }
}

/// Whether two handles refer to the same file or pipe: the same object (like
/// stdout and stderr after `2>&1`), or one file opened twice.
pub fn same_file(a: impl AsHandle, b: impl AsHandle) -> bool {
    let (a, b) = (a.as_handle(), b.as_handle());
    // SAFETY: both handles are borrowed, so open for the call.
    let same_object = unsafe { CompareObjectHandles(a.as_raw_handle(), b.as_raw_handle()) } != 0;
    same_object || matches!((file_id(a), file_id(b)), (Some(a), Some(b)) if a == b)
}

/// A file's volume serial number and index, which identify it.
fn file_id(handle: impl AsHandle) -> Option<(u32, u32, u32)> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let handle = handle.as_handle().as_raw_handle();
    // SAFETY: `info` is a valid out-pointer for the call.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return None;
    }
    Some((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

/// `s` as a NUL-terminated UTF-16 string, as Win32 calls take it.
pub fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// A UTF-16 environment block: `KEY=VALUE` entries sorted by name, each
/// NUL-terminated, then a final NUL.
pub fn env_block(vars: &[(OsString, OsString)]) -> Vec<u16> {
    let mut vars: Vec<_> = vars.iter().collect();
    vars.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (key, val) in vars {
        block.extend(key.encode_wide());
        block.push(u16::from(b'='));
        block.extend(val.encode_wide());
        block.push(0);
    }
    // An empty block still needs its terminator pair.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// Start `exe`, with no arguments, our environment plus `var`, and `dir` as
/// its working folder, and let it run on its own: with no console, in its
/// own process group (so Ctrl+C here doesn't reach it), and out of our job
/// object if that's allowed (sshd and some terminals end the whole job when
/// the session closes, the way a hangup does on Unix).
///
/// It inherits no handles. std can't start a process that way: it hands the
/// child every inheritable handle we have, like the pipe our output goes to,
/// which then stays open as long as the child runs.
pub fn spawn_detached(exe: &Path, var: (&str, &str), dir: Option<&Path>) -> io::Result<()> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(key, _)| !key.eq_ignore_ascii_case(var.0))
        .collect();
    vars.push((var.0.into(), var.1.into()));
    let block = env_block(&vars);
    let app = wide(exe.as_os_str());
    let dir = dir.map(|d| wide(d.as_os_str()));
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT;
    let spawn = |flags| {
        // Its command line is just the program, quoted.
        let mut cmdline: Vec<u16> = std::iter::once(u16::from(b'"'))
            .chain(exe.as_os_str().encode_wide())
            .chain([u16::from(b'"'), 0])
            .collect();
        // SAFETY: zeroed STARTUPINFOW and PROCESS_INFORMATION are the
        // documented starting points.
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: every pointer is valid for the call.
        let ok = unsafe {
            CreateProcessW(
                app.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                flags,
                block.as_ptr().cast(),
                dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
                &si,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both handles are ours, and nothing waits on the child.
        unsafe {
            CloseHandle(pi.hProcess);
            CloseHandle(pi.hThread);
        }
        Ok(())
    };
    // A job that doesn't allow breaking away refuses the spawn. Then the
    // child stays in it, which still outlives this process.
    spawn(flags | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(flags))
}

/// `FileRenameInfoEx` flags, from `winbase.h` (windows-sys lacks them).
const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;

/// Rename `from` to `to` (a full path), replacing whatever is at `to` even
/// while it's open, as a rename does on Unix: whoever holds the old file
/// keeps it until they close it. The usual rename (`MoveFileExW`) refuses to
/// replace an open file. Filesystems without this (FAT, some network shares)
/// get the usual rename.
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    let file = File::options()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(from)?;
    let name: Vec<u16> = to.as_os_str().encode_wide().collect();
    // The name is read as a NUL-terminated string, so room is left for one.
    let size = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + std::mem::size_of_val(&*name) + 2;
    // u64s keep the buffer aligned for the struct, and zeroed, it supplies
    // the NUL.
    let mut buffer = vec![0u64; size.div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: the buffer holds the struct with `name` at its end, and the
    // call reads `size` bytes of it.
    let ok = unsafe {
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).FileNameLength = std::mem::size_of_val(&*name) as u32;
        let dest = std::ptr::addr_of_mut!((*info).FileName).cast::<u16>();
        std::ptr::copy_nonoverlapping(name.as_ptr(), dest, name.len());
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfoEx,
            info.cast(),
            size as u32,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    drop(file);
    match e.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported => std::fs::rename(from, to),
        _ => Err(e),
    }
}

/// Write all bytes to a raw handle. Bypasses `std::io::Stdout`, which on a
/// console re-encodes as UTF-16 and rejects a UTF-8 sequence split across
/// two writes. A `File` writes the bytes as they are.
pub fn write_all_handle(handle: impl AsHandle, buf: &[u8]) -> io::Result<()> {
    // SAFETY: the handle is borrowed for the call, and ManuallyDrop keeps the
    // File from closing it.
    let raw = handle.as_handle().as_raw_handle();
    let file = ManuallyDrop::new(unsafe { File::from_raw_handle(raw) });
    (&*file).write_all(buf)
}
