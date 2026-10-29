//! Small shared helpers.

use std::cell::{Cell, RefCell};
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, PanicHookInfo};
use std::process::ExitStatus;
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

thread_local! {
    /// Set on recorder threads, whose panics end only the recording.
    static RECORDER: Cell<bool> = const { Cell::new(false) };
    /// The message of a recorder thread's panic, kept for its report.
    static PANIC_MESSAGE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Spawn a recorder thread. A panic in it stops the recording and
/// goes to `on_panic` with its message, instead of being printed over the
/// child's output or touching the terminal. The thread gives `None` then.
pub fn spawn_recorder<T, F, P>(body: F, on_panic: P) -> JoinHandle<Option<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
    P: FnOnce(String) + Send + 'static,
{
    thread::spawn(move || {
        RECORDER.set(true);
        match std::panic::catch_unwind(AssertUnwindSafe(body)) {
            Ok(value) => Some(value),
            Err(_) => {
                on_panic(PANIC_MESSAGE.take().unwrap_or_default());
                None
            }
        }
    })
}

/// For the panic hook: if this is a recorder thread, keep the panic's
/// message for its report and return true, so the hook stays quiet.
pub fn capture_recorder_panic(info: &PanicHookInfo) -> bool {
    if RECORDER.get() {
        PANIC_MESSAGE.set(Some(info.to_string()));
    }
    RECORDER.get()
}

/// Write `value` as one line of JSON Lines.
pub fn write_json_line(out: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")
}

/// Current wall-clock time as epoch milliseconds (for rrweb/event timestamps).
pub fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// An exit status as a shell would report it: the exit code, or `128 + signum`
/// when the child was killed by a signal. On Windows every status has a code.
pub fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
