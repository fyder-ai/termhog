//! Unix helpers: polling, pipes, file identity, raw writes and signals.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::process::ExitStatus;
use std::time::Duration;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SigHandler, Signal, raise};
use rustix::fs::fstat;
use rustix::io::retry_on_intr;
#[cfg(not(target_os = "linux"))]
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};

pub use std::os::fd::AsFd as AsStream;
pub use std::os::unix::fs::FileExt;

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

/// A pipe, as its read end and write end, both close-on-exec so no other
/// child inherits them.
pub fn pipe() -> io::Result<(File, OwnedFd)> {
    #[cfg(target_os = "linux")]
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    #[cfg(not(target_os = "linux"))]
    let (read, write) = {
        // No atomic close-on-exec flag here, so set it right away.
        let (read, write) = rustix::pipe::pipe()?;
        for fd in [&read, &write] {
            fcntl_setfd(fd, fcntl_getfd(fd)? | FdFlags::CLOEXEC)?;
        }
        (read, write)
    };
    Ok((File::from(read), write))
}

/// Whether two fds refer to the same file (or pipe).
pub fn same_file(a: impl AsFd, b: impl AsFd) -> bool {
    match (fstat(a), fstat(b)) {
        (Ok(a), Ok(b)) => (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino),
        _ => false,
    }
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

/// Die from the signal that killed the child with `status`, if one did,
/// with core dumps disabled first. Returns otherwise, or if that signal
/// doesn't end the process.
pub fn die_like(status: ExitStatus) {
    use nix::sys::resource::{Resource, setrlimit};
    use std::os::unix::process::ExitStatusExt;

    if let Some(signal) = status.signal().and_then(|s| Signal::try_from(s).ok()) {
        let _ = setrlimit(Resource::RLIMIT_CORE, 0, 0);
        // Crash handlers like systemd-coredump ignore the core limit.
        #[cfg(target_os = "linux")]
        let _ = nix::sys::prctl::set_dumpable(false);
        die_from(signal);
    }
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
