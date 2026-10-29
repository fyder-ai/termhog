//! The Unix host terminal: the controlling terminal, raw mode and its
//! restoration.

use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};

use rustix::fs::fstat;
use rustix::termios::{self, OptionalActions, Termios};

/// The original host termios, saved when raw mode is entered so the panic
/// hook and signal handling can restore it without the `Drop` guard.
static ORIGINAL_TERMIOS: Mutex<Option<Saved>> = Mutex::new(None);

struct Saved {
    termios: Termios,
    tty: File,
    /// The thread that entered raw mode, which owns the recording.
    owner: ThreadId,
}

/// Open the controlling terminal (`/dev/tty`), where keystrokes come from
/// even when stdin is a pipe.
pub fn open_controlling() -> io::Result<File> {
    File::options().read(true).write(true).open("/dev/tty")
}

/// Whether `fd` is this process's controlling terminal. `/dev/tty` reports
/// its own device number, not the terminal's, so this can't be told from
/// comparing it with `fd`.
pub fn is_controlling(fd: impl AsFd) -> bool {
    termios::tcgetsid(fd).is_ok()
}

/// Whether two terminal fds refer to the same terminal device.
pub fn same_terminal(a: impl AsFd, b: impl AsFd) -> bool {
    match (fstat(a), fstat(b)) {
        (Ok(a), Ok(b)) => a.st_rdev == b.st_rdev,
        _ => false,
    }
}

/// Whether our process group is the terminal's foreground job.
pub fn is_foreground(tty: &File) -> bool {
    let own = nix::unistd::getpgrp().as_raw();
    termios::tcgetpgrp(tty).is_ok_and(|pgrp| pgrp.as_raw_nonzero().get() == own)
}

/// Read a terminal's size as `(cols, rows)`. `None` if it can't be read or is
/// unset (a pty nobody sized reports 0x0, which no screen can render).
pub fn winsize(tty: impl AsFd) -> Option<(u16, u16)> {
    let ws = termios::tcgetwinsize(tty).ok()?;
    (ws.ws_col > 0 && ws.ws_row > 0).then_some((ws.ws_col, ws.ws_row))
}

/// Puts the host terminal into raw mode and restores it on drop.
pub struct RawModeGuard {
    /// The terminal's settings from before raw mode.
    pub original: Termios,
}

impl RawModeGuard {
    /// Enter raw mode, saving the original settings where the guard, the
    /// panic hook and signal handling all restore them from.
    pub fn enter(tty: &File) -> io::Result<Self> {
        let original = termios::tcgetattr(tty)?;
        // Cloned first, so nothing can fail once the terminal is raw.
        let saved_tty = tty.try_clone()?;
        let mut raw = original.clone();
        raw.make_raw();
        // Apply immediately, keeping any typeahead for the child.
        termios::tcsetattr(tty, OptionalActions::Now, &raw)?;
        *saved() = Some(Saved {
            termios: original.clone(),
            tty: saved_tty,
            owner: thread::current().id(),
        });
        Ok(RawModeGuard { original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Undo only raw mode. Screen state (alt screen, cursor, mouse) is the
        // child's, so RECOVERY isn't sent here.
        restore_saved();
    }
}

/// Run `f` with the terminal's output processing (like turning `\n` into
/// `\r\n`) back on while raw mode is active, so what it prints looks as it
/// would natively. The saved state stays locked meanwhile, so raw mode can't
/// end halfway through and then be set again.
pub(super) fn with_output_processing(f: impl FnOnce()) {
    let saved = saved();
    let raw = saved.as_ref().and_then(|s| {
        let raw = termios::tcgetattr(&s.tty).ok()?;
        let mut printing = raw.clone();
        printing.output_modes = s.termios.output_modes;
        termios::tcsetattr(&s.tty, OptionalActions::Now, &printing).ok()?;
        Some((&s.tty, raw))
    });
    f();
    if let Some((tty, raw)) = raw {
        let _ = termios::tcsetattr(tty, OptionalActions::Now, &raw);
    }
}

/// Whether this thread entered the raw mode that's active.
pub(super) fn owns_terminal() -> bool {
    saved()
        .as_ref()
        .is_some_and(|s| s.owner == thread::current().id())
}

/// Restore the saved termios, if raw mode is active, and hand back the
/// terminal. Safe to call from any thread, and idempotent.
pub(super) fn restore_saved() -> Option<File> {
    let saved = saved().take()?;
    let _ = termios::tcsetattr(&saved.tty, OptionalActions::Now, &saved.termios);
    Some(saved.tty)
}

fn saved() -> MutexGuard<'static, Option<Saved>> {
    ORIGINAL_TERMIOS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}
