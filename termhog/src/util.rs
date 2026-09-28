//! Small shared helpers.

use std::cell::{Cell, RefCell};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::process::ExitStatusExt;
use std::panic::{AssertUnwindSafe, PanicHookInfo};
use std::process::ExitStatus;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SigHandler, Signal, raise};
use rustix::io::retry_on_intr;
use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
use serde::Serialize;

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
/// when the child was killed by a signal.
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Wait up to `timeout` for `fd` to become readable (`Duration::MAX` waits
/// indefinitely). Hangups and errors count as readable so the caller's next
/// read surfaces them.
pub fn poll_readable(fd: impl AsFd, timeout: Duration) -> bool {
    poll_fd(fd, PollFlags::POLLIN, timeout)
}

fn poll_fd(fd: impl AsFd, events: PollFlags, timeout: Duration) -> bool {
    let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::NONE);
    loop {
        let mut fds = [PollFd::new(fd.as_fd(), events)];
        match poll(&mut fds, timeout) {
            Ok(ready) => return ready > 0,
            Err(Errno::EINTR) => continue,
            Err(_) => return true,
        }
    }
}

/// Wait for the child `pid` to exit without reaping it, so its process ID
/// can't be reused until the caller reaps it. With `nohang`, only checks.
/// Returns whether it has exited.
pub fn wait_exited(pid: u32, nohang: bool) -> io::Result<bool> {
    let pid = Pid::from_raw(pid as i32).ok_or(io::ErrorKind::InvalidInput)?;
    let mut options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
    if nohang {
        options |= WaitIdOptions::NOHANG;
    }
    let status = retry_on_intr(|| waitid(WaitId::Pid(pid), options))?;
    Ok(status.is_some())
}

/// Write all bytes to a raw fd, looping over partial writes. Bypasses
/// `std::io::Stdout`, whose line buffering would hold back output that
/// doesn't end in a newline (like a prompt).
///
/// The fd may be non-blocking: its open file description can be shared with
/// other programs that set O_NONBLOCK (Node tools often leave a tty that way).
/// Then a full buffer returns EAGAIN, so wait until it drains, as a blocking
/// write would, giving up once `stop` returns true.
pub fn write_all_fd(fd: impl AsFd, mut buf: &[u8], stop: impl Fn() -> bool) -> io::Result<()> {
    while !buf.is_empty() {
        match nix::unistd::write(&fd, buf) {
            Ok(n) => buf = &buf[n..],
            Err(Errno::EINTR) => {}
            Err(Errno::EAGAIN) if stop() => return Err(io::Error::other("stopped")),
            Err(Errno::EAGAIN) => {
                poll_fd(&fd, PollFlags::POLLOUT, Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Die from `signal` with its default action, as the process would have
/// without us. The signal may be ignored here (Rust ignores SIGPIPE, and
/// ignores are inherited), so its default action is set first. Returns if
/// that action doesn't end the process.
pub fn die_from(signal: Signal) {
    // SAFETY: setting the default action installs no handler code.
    let _ = unsafe { nix::sys::signal::signal(signal, SigHandler::SigDfl) };
    let _ = raise(signal);
}
