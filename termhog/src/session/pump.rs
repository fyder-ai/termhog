//! The threads that move bytes: child output to the real stream and the
//! recording, terminal input to the child, and terminal resizes to the pty.
//!
//! Invariant: recording never gates the tee. Output is written to the real
//! stream before the recording sees it, and the render thread's queue is
//! unbounded, so a slow render never holds up the terminal.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::Signal;
use signal_hook::iterator::Signals;

use crate::render::Feed;
use crate::tty::osc::{Change, Colors, Scanner};
use crate::tty::probe::{self, ReplyFilter, TerminalInfo};
use crate::tty::pty::{self, Pty};
use crate::tty::{self, RawModeGuard, TerminalLease};
use crate::util::{poll_readable, write_all_fd};
use crate::{Error, Result};

const READ_BUF: usize = 32 * 1024;

/// Everything needed to attach a child to the host terminal, set up before
/// the child is spawned so that nothing can fail once it's running.
pub struct Attachment {
    tty: File,
    /// The pty master, for input.
    master: File,
    /// For resizes: the listener, the terminal and the pty master.
    winch: Option<(Signals, File, File)>,
    /// Lets the input thread know it's time to stop, by closing.
    wake: (UnixStream, UnixStream),
    /// Declared before the lease, so the terminal is restored before another
    /// recording can take it.
    raw_guard: RawModeGuard,
    lease: TerminalLease,
    /// The pty's size.
    pub size: (u16, u16),
    /// The terminal was asked for its colors and cell size.
    pub probed: bool,
}

impl Attachment {
    /// Take over the controlling terminal that stdout shows on, with a pty
    /// for the child to match it: raw mode, then the color/size probe (its
    /// replies are picked out of the input later, without waiting), then
    /// resize tracking. `None` when stdout is some other terminal, when there
    /// is no controlling terminal, or when we're a background job (changing
    /// its modes would stop us with SIGTTOU, and its input belongs to the
    /// foreground job). A terminal that reports no size (some ptys do) gets
    /// `fallback_size`.
    pub fn open(fallback_size: (u16, u16)) -> Result<Option<(Attachment, Pty)>> {
        let Ok(tty) = tty::open_controlling() else {
            return Ok(None);
        };
        if !tty::is_controlling(io::stdout()) || !tty::is_foreground(&tty) {
            return Ok(None);
        }
        let lease = TerminalLease::acquire().ok_or(Error::TerminalBusy)?;
        let size = tty::winsize(&tty).unwrap_or(fallback_size);
        let raw_guard = RawModeGuard::enter(&tty)?;
        // Colors are cosmetic. If the probe can't be sent, record without it.
        let probed = probe::should_probe() && probe::send_queries(&tty).is_ok();
        let pty = pty::open(size, &raw_guard.original)?;
        let master = pty.master.try_clone()?;
        let wake = UnixStream::pair()?;
        // Without resize tracking the child just keeps its initial size.
        let winch = match Signals::new([Signal::SIGWINCH as i32]) {
            Ok(signals) => Some((signals, tty.try_clone()?, pty.master.try_clone()?)),
            Err(_) => None,
        };
        // Measured again once listening, so a resize meanwhile isn't lost.
        let size = match tty::winsize(&tty) {
            Some(now) if now != size => {
                pty::resize(&pty.master, now)?;
                now
            }
            _ => size,
        };
        let attachment = Attachment {
            tty,
            master,
            winch,
            wake,
            raw_guard,
            lease,
            size,
            probed,
        };
        Ok(Some((attachment, pty)))
    }

    /// Start forwarding terminal input into the pty, and pty resizes on SIGWINCH.
    pub fn start(self, feed: &Feed) -> Attached {
        let shutdown = Arc::new(AtomicBool::new(false));
        let (wake, woken) = self.wake;
        let input = {
            let (tty, master, shutdown) = (self.tty, self.master, Arc::clone(&shutdown));
            let replies = ReplyFilter::new(self.size.0, self.size.1, self.probed);
            let feed = feed.clone();
            thread::spawn(move || input_loop(tty, master, replies, (shutdown, woken), feed))
        };
        let winch = self.winch.map(|(signals, tty, master)| {
            let handle = signals.handle();
            let feed = feed.clone();
            let join = thread::spawn(move || winch_loop(signals, tty, master, feed));
            (handle, join)
        });
        Attached {
            shutdown,
            wake,
            input,
            winch,
            raw_guard: self.raw_guard,
            _lease: self.lease,
        }
    }
}

/// State held while a child is attached to the host terminal through a pty.
pub struct Attached {
    shutdown: Arc<AtomicBool>,
    wake: UnixStream,
    input: JoinHandle<()>,
    winch: Option<(signal_hook::iterator::Handle, JoinHandle<()>)>,
    /// Declared before the lease, so the terminal is restored before another
    /// recording can take it.
    raw_guard: RawModeGuard,
    _lease: TerminalLease,
}

impl Attached {
    /// Stop the input and resize threads, then restore the terminal.
    pub fn detach(self) {
        self.shutdown.store(true, Ordering::SeqCst);
        drop(self.wake);
        let _ = self.input.join();
        if let Some((handle, join)) = self.winch {
            handle.close();
            let _ = join.join();
        }
        drop(self.raw_guard);
    }
}

/// How output bytes reach the recording.
#[derive(Clone, Copy)]
pub enum Source {
    /// From the pty, which already applied the terminal's output processing.
    Pty,
    /// From a raw pipe. A terminal would turn each `\n` into `\r\n` (ONLCR),
    /// so the recording gets that translation to show lines as they'd appear.
    Pipe,
}

impl Source {
    fn for_recording(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Source::Pty => bytes.to_vec(),
            Source::Pipe => {
                let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 8);
                for &b in bytes {
                    if b == b'\n' {
                        out.push(b'\r');
                    }
                    out.push(b);
                }
                out
            }
        }
    }
}

/// Copy child output (pty master or pipe) byte-exact to the real stream it
/// stands in for, then hand it to the recording. stdout and stderr share one
/// recorded stream in arrival order, as a terminal interleaves them.
pub fn read_loop(mut reader: impl Read + AsFd, out: impl AsFd, source: Source, feed: Feed) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match reader.read(&mut buf) {
            // The pty master is non-blocking: wait for output.
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                poll_readable(&reader, Duration::MAX);
            }
            Ok(0) => break,
            Ok(n) => {
                // Hot path first: never let recording delay the terminal.
                if let Err(e) = write_all_fd(&out, &buf[..n], || false) {
                    log::warn!("output write error: {e}");
                    break;
                }
                feed.output(source.for_recording(&buf[..n]));
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(ref e) if pty::is_eof_error(e) => break, // child gone (Linux EIO)
            Err(e) => {
                log::warn!("output read error: {e}");
                break;
            }
        }
    }
}

/// Forward terminal input to the pty, until `shutdown` is set (and `woken`
/// closes, to wake the loop).
///
/// The terminal's replies to the color/size probe arrive here too. `replies`
/// takes them out, so they reach the recording instead of the child. Replies
/// to the child's own color queries (neovim asks on every theme switch) are
/// passed on, and the recording reads a copy.
///
/// Keys like Ctrl+C are forwarded as plain bytes. The pty turns them into
/// signals for the program, exactly as a terminal would when running it
/// directly, so a program that handles Ctrl+C (a shell, a REPL) keeps going
/// and we're unaffected, like `script` or `ssh`.
fn input_loop(
    mut tty: File,
    master: File,
    mut replies: ReplyFilter,
    (shutdown, woken): (Arc<AtomicBool>, UnixStream),
    feed: Feed,
) {
    let mut buf = [0u8; READ_BUF];
    let mut snooped = Scanner::default();
    let stopped = || shutdown.load(Ordering::SeqCst);
    while !stopped() {
        let keys = if wait_for_input(&tty, &woken) {
            let n = match tty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let (keys, info) = replies.feed(&buf[..n]);
            if let Some(info) = info {
                feed.terminal(info);
            }
            keys
        } else {
            // Quiet for a while: release a held partial sequence (a lone ESC).
            replies.flush()
        };
        if keys.is_empty() {
            continue;
        }
        report_colors(&mut snooped, &keys, &feed);
        // A child that isn't reading leaves no room, so this waits for room,
        // giving up once it's time to stop.
        if write_all_fd(&master, &keys, stopped).is_err() {
            break;
        }
        // Mark activity (no content) so typing counts as active time.
        feed.input();
    }
    // The child is gone, but the terminal may still be answering the probe
    // (the child exited at once). Take the replies, which would otherwise
    // land at the shell's prompt as junk. Keys typed meanwhile go with them.
    while !replies.is_done() {
        if poll_readable(&tty, Duration::from_millis(20)) {
            match tty.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let (_, Some(info)) = replies.feed(&buf[..n]) {
                        feed.terminal(info);
                    }
                }
            }
        }
    }
}

/// Wait up to 100 ms for terminal input, returning whether there is some.
/// Returns early, with none, once `woken` closes.
fn wait_for_input(tty: &File, woken: &UnixStream) -> bool {
    let mut fds = [
        PollFd::new(tty.as_fd(), PollFlags::POLLIN),
        PollFd::new(woken.as_fd(), PollFlags::POLLIN),
    ];
    // A signal cutting the wait short isn't the quiet it waits for.
    while poll(&mut fds, PollTimeout::from(100u8)) == Err(Errno::EINTR) {}
    fds[0].any().unwrap_or(false)
}

/// Tell the recording about color reports in the terminal's input.
fn report_colors(scanner: &mut Scanner, input: &[u8], feed: &Feed) {
    let mut colors = Colors::default();
    for change in scanner.scan(input) {
        if let Change::Set(..) = change {
            colors.apply(&change);
        }
    }
    if colors != Colors::default() {
        feed.terminal(TerminalInfo { colors, cell: None });
    }
}

/// Resize the pty whenever the host terminal resizes. The SIGWINCH handler
/// runs alongside any the app already has.
fn winch_loop(mut signals: Signals, tty: File, master: File, feed: Feed) {
    for _ in signals.forever() {
        if let Some(size) = tty::winsize(&tty) {
            let _ = pty::resize(&master, size);
            feed.resize(size.0, size.1);
        }
    }
}
