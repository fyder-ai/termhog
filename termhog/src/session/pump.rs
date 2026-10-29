//! The threads that move bytes: child output to the real stream and the
//! recording, terminal input to the child, and terminal resizes to the pty.
//! Attaching to the terminal, with its input and resizes, is each platform's
//! own (`pump/unix.rs`, `pump/windows.rs`). Output is the same everywhere.
//!
//! Invariant: recording never gates the tee. Output is written to the real
//! stream before the recording sees it, and the render thread's queue is
//! unbounded, so a slow render never holds up the terminal.

use std::io::{self, Read};
use std::thread::{self, JoinHandle};

use crate::render::Feed;
use crate::tty::osc::{Change, Colors, Scanner};
use crate::tty::probe::TerminalInfo;
use crate::tty::pty;
use crate::util::AsStream;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Attached, Attachment, PtyOutput};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Attached, Attachment, PtyOutput};

const READ_BUF: usize = 32 * 1024;

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

/// Start a thread teeing child output from `pipe` to `out` (see
/// [`read_loop`]).
pub fn tee(
    pipe: impl Read + AsStream + Send + 'static,
    out: impl AsStream + Send + 'static,
    feed: &Feed,
) -> JoinHandle<()> {
    let feed = feed.clone();
    thread::spawn(move || read_loop(pipe, out, Source::Pipe, feed))
}

/// Copy child output (pty master or pipe) byte-exact to the real stream it
/// stands in for, then hand it to the recording. stdout and stderr share one
/// recorded stream in arrival order, as a terminal interleaves them.
pub fn read_loop(mut reader: impl Read + AsStream, out: impl AsStream, source: Source, feed: Feed) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match reader.read(&mut buf) {
            // The pty master is non-blocking: wait for output.
            #[cfg(unix)]
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                crate::util::poll_readable(&reader, std::time::Duration::MAX);
            }
            Ok(0) => break,
            Ok(n) => {
                // Hot path first: never let recording delay the terminal.
                #[cfg(unix)]
                let written = crate::util::write_all_fd(&out, &buf[..n], || false);
                #[cfg(windows)]
                let written = crate::util::write_all_handle(&out, &buf[..n]);
                if let Err(e) = written {
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
