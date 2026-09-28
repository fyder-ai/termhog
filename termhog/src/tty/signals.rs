//! Signal handling while a recording is live (the behavior is described in
//! the [crate docs](crate#signals)). Only signals still at their default
//! action are handled.
//!
//! - SIGTERM and SIGHUP (`kill`, `timeout`, a closed terminal) would kill the
//!   process with the terminal left raw and the recording cut short. They're
//!   passed to the child, the recording gets a moment to flush, the terminal
//!   is restored, then the process dies from the signal.
//! - SIGINT and SIGQUIT (Ctrl+C, Ctrl+\\) belong to the child. Typed on a
//!   terminal, the kernel already delivered them to it, so any copy we get is
//!   absorbed. One sent by another process (`kill -INT`) is passed on.

use std::ffi::{c_int, c_void};
use std::io::{self, Read};
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, killpg};
use nix::unistd::Pid;

use crate::util::die_from;

/// Signals that end the process: forwarded to the child, flushed, then fatal.
const TERMINATING: [Signal; 2] = [Signal::SIGTERM, Signal::SIGHUP];
/// Keyboard signals that belong to the child: absorbed when the terminal sent
/// them, forwarded when another process did.
const KEYBOARD: [Signal; 2] = [Signal::SIGINT, Signal::SIGQUIT];
/// How long a terminating signal waits for recordings to finish.
const FINISH_TIMEOUT: Duration = Duration::from_secs(3);
/// Once recordings finish, how long the app gets to report (like the CLI
/// printing the replay link) before the process dies.
const REPORT_GRACE: Duration = Duration::from_millis(250);

/// Write end of the pipe the signal handler wakes the handling thread with.
/// Negative until the pipe exists.
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

struct Registry {
    watches: Vec<Arc<Watch>>,
    /// Signals whose handler we installed, to put back to SIG_DFL.
    installed: Vec<Signal>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    watches: Vec::new(),
    installed: Vec::new(),
});

/// One live recording, and whether it has finished.
struct Watch {
    /// The recorded child, from its spawn until it exits. Cleared before the
    /// child is reaped, so a signal is never sent to a reused process ID.
    child: Mutex<Option<Child>>,
    done: Mutex<bool>,
    finished: Condvar,
}

#[derive(Clone, Copy)]
struct Child {
    pid: Pid,
    /// The child leads its own process group (the pty is its controlling
    /// terminal), so the signal goes to its whole job.
    group: bool,
}

/// Keeps the safety net active for one recording. Drop it once the
/// recording has flushed.
pub struct Guard(Arc<Watch>);

/// Cover a recording from before it touches the terminal. The child is
/// attached with [`Guard::set_child`] once it's spawned.
pub fn watch() -> Guard {
    let watch = Arc::new(Watch {
        child: Mutex::new(None),
        done: Mutex::new(false),
        finished: Condvar::new(),
    });
    let ready = start_thread();
    if let Ok(mut registry) = REGISTRY.lock() {
        if registry.watches.is_empty() && ready {
            registry.installed = TERMINATING
                .into_iter()
                .chain(KEYBOARD)
                .filter(|&s| install(s))
                .collect();
        }
        registry.watches.push(Arc::clone(&watch));
    }
    Guard(watch)
}

impl Guard {
    /// Forward signals to the spawned child `pid`, or to its whole process
    /// group when `group` is set.
    pub fn set_child(&self, pid: u32, group: bool) {
        if let Ok(mut child) = self.0.child.lock() {
            *child = Some(Child {
                pid: Pid::from_raw(pid as i32),
                group,
            });
        }
    }

    /// Reap the child once it has exited. Forwarding to it stops first,
    /// since its process ID can belong to anyone once it's reaped.
    pub fn reap(&self, child: &mut std::process::Child) -> io::Result<ExitStatus> {
        if let Ok(mut watched) = self.0.child.lock() {
            *watched = None;
        }
        child.wait()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut done) = self.0.done.lock() {
            *done = true;
        }
        self.0.finished.notify_all();
        if let Ok(mut registry) = REGISTRY.lock() {
            registry.watches.retain(|w| !Arc::ptr_eq(w, &self.0));
            if registry.watches.is_empty() {
                for signal in registry.installed.drain(..) {
                    restore_default(signal);
                }
            }
        }
    }
}

/// Install our handler for `signal` if its action is still the default.
fn install(signal: Signal) -> bool {
    if current_handler(signal) != Some(libc::SIG_DFL) {
        return false;
    }
    let handler = SigAction::new(
        SigHandler::SigAction(on_signal),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler only calls write(2), which is async-signal-safe.
    unsafe { nix::sys::signal::sigaction(signal, &handler) }.is_ok()
}

/// Put `signal` back to its default action, unless the app has since
/// installed its own handler, which is then left in place.
fn restore_default(signal: Signal) {
    if current_handler(signal) == Some(on_signal as *const () as usize) {
        // SAFETY: restoring the default action we replaced.
        let _ = unsafe { nix::sys::signal::signal(signal, SigHandler::SigDfl) };
    }
}

/// The address of `signal`'s current handler (or SIG_DFL/SIG_IGN), read
/// without changing it.
fn current_handler(signal: Signal) -> Option<usize> {
    // SAFETY: an all-zero sigaction is valid, and a null new action makes
    // sigaction only read the current one into `old`.
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::sigaction(signal as c_int, std::ptr::null(), &mut old) };
    (ret == 0).then_some(old.sa_sigaction)
}

/// Wake the handling thread with the signal, and whether another process
/// sent it (as opposed to the kernel, like a terminal's Ctrl+C).
extern "C" fn on_signal(signal: c_int, info: *mut libc::siginfo_t, _: *mut c_void) {
    let fd = WAKE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    // A failed write must not change errno for the code this interrupted.
    let errno = Errno::last_raw();
    // SAFETY: the kernel passes a valid siginfo to SA_SIGINFO handlers. The
    // sender's pid is zero when the kernel itself raised the signal.
    let sent = !info.is_null() && unsafe { (*info).si_pid() } != 0;
    let message = [signal as u8, sent as u8];
    // SAFETY: write(2) is async-signal-safe, and the fd is the pipe's write
    // end, open for the life of the process. It's non-blocking, so a full
    // pipe drops the wakeup instead of hanging the handler.
    unsafe { libc::write(fd, message.as_ptr().cast(), message.len()) };
    Errno::set_raw(errno);
}

/// Start (once) the thread that handles signals outside the signal handler.
/// Returns whether it's running. Without it, no handler is installed.
fn start_thread() -> bool {
    static START: Once = Once::new();
    START.call_once(|| {
        // Both ends are close-on-exec. Only the write end is non-blocking: the
        // reader waits for signals.
        let Ok((mut read, write)) = UnixStream::pair() else {
            return;
        };
        if write.set_nonblocking(true).is_err() {
            return;
        }
        // Deliberately never closed: the handler may write to it any time.
        WAKE_FD.store(write.into_raw_fd(), Ordering::Relaxed);
        thread::spawn(move || {
            let mut message = [0u8; 2];
            while read.read_exact(&mut message).is_ok() {
                if let Ok(signal) = Signal::try_from(message[0] as i32) {
                    handle(signal, message[1] != 0);
                }
            }
        });
    });
    WAKE_FD.load(Ordering::Relaxed) >= 0
}

fn handle(signal: Signal, sent: bool) {
    if TERMINATING.contains(&signal) {
        terminate(signal);
    } else if sent {
        // A keyboard signal from another process was meant for the program.
        // One typed on the terminal already reached it, so it's absorbed.
        forward(signal);
    }
}

/// Pass `signal` to every recorded child that's still running.
fn forward(signal: Signal) -> Vec<Arc<Watch>> {
    let watches = REGISTRY
        .lock()
        .map(|r| r.watches.clone())
        .unwrap_or_default();
    for watch in &watches {
        // Held while signalling, so the child can't be reaped in between.
        let Ok(child) = watch.child.lock() else {
            continue;
        };
        if let Some(child) = *child {
            let _ = if child.group {
                killpg(child.pid, signal)
            } else {
                kill(child.pid, signal)
            };
        }
    }
    watches
}

/// Pass `signal` to every recorded child, give the recordings a moment to
/// flush, restore the terminal, then die from the signal.
fn terminate(signal: Signal) {
    let watches = forward(signal);
    let deadline = Instant::now() + FINISH_TIMEOUT;
    for watch in &watches {
        if let Ok(done) = watch.done.lock() {
            let left = deadline.saturating_duration_since(Instant::now());
            let _ = watch.finished.wait_timeout_while(done, left, |done| !*done);
        }
    }
    super::restore_saved();
    thread::sleep(REPORT_GRACE);
    die_from(signal);
}
