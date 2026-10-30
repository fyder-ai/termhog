//! Attaching to a Windows console: console modes, a pseudoconsole to match,
//! console input to the child, and following the console's size.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Console::{INPUT_RECORD, KEY_EVENT, ReadConsoleInputW};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

use super::report_colors;
use crate::render::Feed;
use crate::tty::osc::Scanner;
use crate::tty::probe::{self, ReplyFilter};
use crate::tty::pty::{self, PseudoConsole, Pty};
use crate::tty::{self, Console, OutputModeGuard, RawModeGuard, TerminalLease, signals};
use crate::{Error, Result};

/// Everything needed to attach a child to the console, set up before the
/// child is spawned so that nothing can fail once it's running.
pub struct Attachment {
    console: Console,
    pseudoconsole: Arc<PseudoConsole>,
    /// Declared before the lease, so the console is restored before another
    /// recording can take it.
    raw_guard: RawModeGuard,
    lease: TerminalLease,
    /// The pseudoconsole's size.
    pub size: (u16, u16),
    /// The terminal was asked for its colors and cell size.
    pub probed: bool,
}

impl Attachment {
    /// Take over the console stdout shows on, with a pseudoconsole for the
    /// child to match it. `None` when there's no console to open. The ConPTY
    /// is written into `executable_dir` the first time (see
    /// [`crate::TermHog::executable_dir`]).
    ///
    /// The console's output stays raw for as long as the returned guard is
    /// held, which is by whatever mirrors the pseudoconsole's output to it
    /// (see [`PtyOutput`]).
    pub fn open(
        fallback_size: (u16, u16),
        executable_dir: Option<&Path>,
    ) -> Result<Option<(Attachment, Pty, OutputModeGuard)>> {
        let Ok(console) = Console::open() else {
            return Ok(None);
        };
        let lease = TerminalLease::acquire().ok_or(Error::TerminalBusy)?;
        let size = tty::winsize(&console.output).unwrap_or(fallback_size);
        // The child starts at the cursor, as it would natively. Its position
        // query is answered through the input forwarding. Without the
        // embedded ConPTY the recording fails, rather than falling back to
        // one that re-renders the output.
        let pty = pty::open(size, true, executable_dir)?;
        let (raw_guard, output_mode) = RawModeGuard::enter(&console)?;
        // Colors are cosmetic. If the probe can't be sent, record without it.
        // It goes out before the child starts, so its DA1 sentinel is
        // answered before the one the pseudoconsole sends.
        let probed = probe::should_probe() && probe::send_queries(&console.output).is_ok();
        let attachment = Attachment {
            pseudoconsole: Arc::clone(&pty.console),
            console,
            raw_guard,
            lease,
            size,
            probed,
        };
        Ok(Some((attachment, pty, output_mode)))
    }

    /// Give the console back without starting the child (it failed to
    /// spawn). The terminal may still be answering the color/size probe, so
    /// its replies are taken first, or they'd land at the shell's prompt as
    /// junk.
    pub fn abandon(self) {
        let mut replies = ReplyFilter::new(self.size.0, self.size.1, self.probed);
        let mut reader = KeyReader::default();
        while !replies.is_done() {
            match reader.read(&self.console.input, 20) {
                Ok(Some(typed)) => {
                    let _ = replies.feed(&typed);
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    }

    /// Start forwarding console input into the pseudoconsole, and resizes.
    pub fn start(self, feed: &Feed) -> Attached {
        let shutdown = Arc::new(AtomicBool::new(false));
        // Ctrl+Break isn't typed input (see `signals`). Without a handle to
        // spare, it just doesn't reach the child.
        signals::forward_breaks_to(self.pseudoconsole.input.try_clone().ok());
        let input = {
            let shutdown = Arc::clone(&shutdown);
            let feed = feed.clone();
            let (console, pseudoconsole) = (self.console, self.pseudoconsole);
            let replies = ReplyFilter::new(self.size.0, self.size.1, self.probed);
            let size = self.size;
            thread::spawn(move || input_loop(console, pseudoconsole, size, replies, shutdown, feed))
        };
        Attached {
            shutdown,
            input,
            raw_guard: self.raw_guard,
            _lease: self.lease,
        }
    }
}

/// The pseudoconsole's output, for the reader that mirrors it to the console.
/// It holds the console's output in raw mode until the reader is done, which
/// can be after the child is detached (a dropped [`crate::Recording`]).
pub struct PtyOutput {
    output: File,
    /// Restores the console's output when the reader drops this.
    _raw: OutputModeGuard,
}

impl PtyOutput {
    pub fn new(output: File, raw: OutputModeGuard) -> PtyOutput {
        PtyOutput { output, _raw: raw }
    }
}

impl Read for PtyOutput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.output.read(buf)
    }
}

impl AsHandle for PtyOutput {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.output.as_handle()
    }
}

/// State held while a child is attached to the console.
pub struct Attached {
    shutdown: Arc<AtomicBool>,
    input: JoinHandle<()>,
    /// Declared before the lease, so the console is restored before another
    /// recording can take it.
    raw_guard: RawModeGuard,
    _lease: TerminalLease,
}

impl Attached {
    /// Stop the input thread, then restore the console.
    pub fn detach(self) {
        signals::forward_breaks_to(None);
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.input.join();
        drop(self.raw_guard);
    }
}

/// Forward console input to the pseudoconsole until `shutdown` is set, and
/// resize it to follow the console.
///
/// In raw mode keys arrive as key events carrying VT text (arrows as
/// `ESC [ A` and so on), and Ctrl+C as `0x03`, which the pseudoconsole turns
/// into the child's Ctrl+C. Waiting on the console handle with a timeout
/// keeps the loop able to stop, and gives a moment to check the size.
///
/// The terminal's replies to the color/size probe arrive here too, as typed
/// text. `replies` takes them out, as on Unix.
fn input_loop(
    console: Console,
    pseudoconsole: Arc<PseudoConsole>,
    mut size: (u16, u16),
    mut replies: ReplyFilter,
    shutdown: Arc<AtomicBool>,
    feed: Feed,
) {
    let mut reader = KeyReader::default();
    let mut snooped = Scanner::default();
    while !shutdown.load(Ordering::SeqCst) {
        // Resize events don't fire for every window change, so poll.
        if let Some(now) = tty::winsize(&console.output) {
            if now != size {
                size = now;
                let _ = pseudoconsole.resize(now);
                feed.resize(now.0, now.1);
            }
        }
        let keys = match reader.read(&console.input, 100) {
            Ok(Some(typed)) => {
                let (keys, info) = replies.feed(&typed);
                if let Some(info) = info {
                    feed.terminal(info);
                }
                keys
            }
            // Quiet for a while: release a held partial sequence (a lone ESC).
            Ok(None) => replies.flush(),
            Err(_) => break,
        };
        if keys.is_empty() {
            continue;
        }
        report_colors(&mut snooped, &keys, &feed);
        if (&pseudoconsole.input).write_all(&keys).is_err() {
            break;
        }
        // Mark activity (no content) so typing counts as active time.
        feed.input();
    }
    // The child is gone, but the terminal may still be answering the probe
    // (the child exited at once). Take the replies, which would otherwise
    // land at the shell's prompt as junk.
    while !replies.is_done() {
        match reader.read(&console.input, 20) {
            Ok(Some(typed)) => {
                if let (_, Some(info)) = replies.feed(&typed) {
                    feed.terminal(info);
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
}

/// Reads typed text from the console's key events.
#[derive(Default)]
struct KeyReader {
    /// A high surrogate waiting for its pair from a later key event.
    high: Option<u16>,
}

impl KeyReader {
    /// Wait up to `timeout_ms` for input, returning what was typed as UTF-8,
    /// or `None` if nothing was (other events, like focus or mouse, are
    /// dropped).
    fn read(&mut self, console: &File, timeout_ms: u32) -> io::Result<Option<Vec<u8>>> {
        let handle = console.as_raw_handle();
        // SAFETY: the console input handle is open.
        if unsafe { WaitForSingleObject(handle, timeout_ms) } != WAIT_OBJECT_0 {
            return Ok(None);
        }
        // SAFETY: INPUT_RECORD is plain data, and zeroed is a valid value.
        let mut records: [INPUT_RECORD; 128] = unsafe { std::mem::zeroed() };
        let mut count = 0;
        // SAFETY: `records` holds `len` records, and `count` is an
        // out-pointer.
        let ok = unsafe {
            ReadConsoleInputW(
                handle,
                records.as_mut_ptr(),
                records.len() as u32,
                &mut count,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut typed = Vec::new();
        for record in &records[..count as usize] {
            if u32::from(record.EventType) != KEY_EVENT {
                continue;
            }
            // SAFETY: the union holds a key event, per EventType.
            let key = unsafe { record.Event.KeyEvent };
            let unit = unsafe { key.uChar.UnicodeChar };
            if key.bKeyDown == 0 || unit == 0 {
                continue;
            }
            for _ in 0..key.wRepeatCount.max(1) {
                self.push(unit, &mut typed);
            }
        }
        Ok((!typed.is_empty()).then_some(typed))
    }

    /// Append the UTF-16 code unit `unit` to `out` as UTF-8, holding a high
    /// surrogate until its pair arrives. An unpaired one becomes U+FFFD.
    fn push(&mut self, unit: u16, out: &mut Vec<u8>) {
        let c = match (self.high.take(), unit) {
            (_, 0xD800..=0xDBFF) => {
                self.high = Some(unit);
                return;
            }
            (Some(high), 0xDC00..=0xDFFF) => char::decode_utf16([high, unit]).next(),
            (_, unit) => char::decode_utf16([unit]).next(),
        };
        let c = c
            .and_then(|c| c.ok())
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
}
