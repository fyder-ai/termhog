//! Host terminal control: raw mode, bulletproof restoration, panic hook.
//!
//! The child's PTY slave owns line discipline and signal generation. The host
//! terminal is put into raw mode so keystrokes (including `^C` = 0x03) flow
//! through untouched to the child. Restoration must survive normal return, `?`
//! propagation, panic, and signals — hence a `Drop` guard plus a panic hook
//! that both restore from the same saved termios.

use std::ffi::c_void;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::sync::Mutex;

use rustix::termios::{self, OptionalActions, Termios, Winsize};

/// Crash-only recovery escapes: show cursor, leave alt screen, disable every
/// mouse reporting mode. Emitted ONLY when we panic — i.e. when our own failure
/// aborts the normal teardown and would otherwise leave the terminal wedged. On
/// a clean exit we emit nothing: the child's own cleanup (or lack thereof) has
/// already flowed through the PTY to the real terminal, and a transparent
/// wrapper must preserve exactly what direct invocation would leave behind.
const RECOVERY: &[u8] = b"\x1b[?25h\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";

/// The original host termios, saved when raw mode is entered so the panic hook
/// can restore it even when unwinding won't reach the `Drop` guard in time.
static ORIGINAL_TERMIOS: Mutex<Option<Termios>> = Mutex::new(None);

/// Read the host terminal size from stdin. Returns `(cols, rows)`.
pub fn host_winsize() -> io::Result<(u16, u16)> {
    let ws: Winsize = termios::tcgetwinsize(io::stdin().as_fd())?;
    Ok((ws.ws_col, ws.ws_row))
}

/// Whether the given standard stream is connected to a TTY.
pub fn stdout_is_tty() -> bool {
    termios::isatty(io::stdout().as_fd())
}

pub fn stdin_is_tty() -> bool {
    termios::isatty(io::stdin().as_fd())
}

/// Puts the host terminal into raw mode and restores it on drop. Also emits the
/// recovery escape bundle on restore so a child that crashed mid-repaint (in alt
/// screen, cursor hidden, mouse on) doesn't leave the shell wedged.
pub struct RawModeGuard {
    original: Termios,
    active: bool,
}

impl RawModeGuard {
    /// Enter raw mode on stdin. Saves the original attributes both on the guard
    /// (for `Drop`) and in the global (for the panic hook).
    pub fn enter() -> io::Result<Self> {
        let stdin = io::stdin();
        let original = termios::tcgetattr(stdin.as_fd())?;

        let mut raw = original.clone();
        raw.make_raw();
        termios::tcsetattr(stdin.as_fd(), OptionalActions::Flush, &raw)?;

        if let Ok(mut slot) = ORIGINAL_TERMIOS.lock() {
            *slot = Some(original.clone());
        }

        Ok(RawModeGuard {
            original,
            active: true,
        })
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // Undo ONLY our own change: raw mode on the host tty. We deliberately do
        // not emit RECOVERY here — screen state (alt screen, cursor, mouse) is
        // the child's, and preserving it is what makes passthrough transparent.
        let stdin = io::stdin();
        let _ = termios::tcsetattr(stdin.as_fd(), OptionalActions::Now, &self.original);
        if let Ok(mut slot) = ORIGINAL_TERMIOS.lock() {
            *slot = None;
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Install a panic hook that restores the terminal from the saved termios before
/// delegating to the previous hook. Needed because a panic on a worker thread
/// won't unwind the main thread's `RawModeGuard` promptly.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_from_global();
        previous(info);
    }));
}

/// Restore the terminal using the globally-saved termios, if raw mode is active.
/// Safe to call from any thread and idempotent.
pub fn restore_from_global() {
    if let Ok(mut slot) = ORIGINAL_TERMIOS.lock() {
        if let Some(original) = slot.take() {
            let stdin = io::stdin();
            let _ = termios::tcsetattr(stdin.as_fd(), OptionalActions::Now, &original);
            let mut out = io::stdout();
            let _ = out.write_all(RECOVERY);
            let _ = out.flush();
        }
    }
}

/// Colors captured from the host terminal, as CSS hex (`#rrggbb`).
#[derive(Debug, Clone)]
pub struct ThemeColors {
    pub fg: String,
    pub bg: String,
    /// The 16 ANSI palette colors, index 0..=15.
    pub palette: Vec<String>,
}

/// A character cell's size in device pixels, as reported by the host terminal.
/// The terminal's font is not knowable over a PTY, but its cell geometry is, and
/// that geometry is what the projected grid must match to render without gaps.
#[derive(Debug, Clone, Copy)]
pub struct CellSize {
    pub width: u16,
    pub height: u16,
}

impl CellSize {
    /// Accept only sane cell dimensions, rejecting `0` and absurd values so a
    /// garbled reply can't poison the grid.
    fn from_dims(w: u32, h: u32) -> Option<CellSize> {
        if (2..=64).contains(&w) && (2..=128).contains(&h) {
            Some(CellSize {
                width: w as u16,
                height: h as u16,
            })
        } else {
            None
        }
    }
}

/// Everything we probe from the host terminal in a single query round-trip.
#[derive(Default)]
pub struct TerminalInfo {
    pub theme: Option<ThemeColors>,
    pub cell: Option<CellSize>,
}

/// Ask the host terminal for its foreground (OSC 10), background (OSC 11),
/// 16-color palette (OSC 4) and character-cell pixel size (XTWINOPS `CSI 16 t`,
/// with `CSI 14 t` as a fallback), then read the replies from stdin.
///
/// A Primary Device Attributes request (`ESC [ c`) is sent last as a sentinel:
/// every terminal answers it, and terminals reply in order, so its answer marks
/// the end of the replies. We block-read until that answer arrives — no timeout,
/// no guessing. A terminal that omits a reply (but answers DA1) definitively
/// lacks that feature, and we fall back (default theme / default cell metrics).
/// The pathological "never answers DA1" case surfaces as a visible hang rather
/// than silently-wrong output, which is the intended failure mode.
///
/// `cols`/`rows` are the current grid, used to derive a cell size from the
/// `CSI 14 t` text-area reply when the terminal doesn't answer `CSI 16 t`.
///
/// Must be called with stdin in raw mode and before any other reader owns stdin,
/// so the replies aren't echoed or line-buffered. Returns empty info off a tty.
pub fn query_terminal(cols: u16, rows: u16) -> TerminalInfo {
    if !stdin_is_tty() || !stdout_is_tty() {
        return TerminalInfo::default();
    }

    // Emit all queries up front. The DA1 sentinel must come last.
    let mut q = String::from("\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    for n in 0..16 {
        q.push_str(&format!("\x1b]4;{n};?\x1b\\"));
    }
    // Cell size in pixels (16t) then text-area size in pixels (14t) as fallback.
    q.push_str("\x1b[16t\x1b[14t");
    q.push_str("\x1b[c");
    {
        let mut out = io::stdout();
        if out.write_all(q.as_bytes()).is_err() || out.flush().is_err() {
            return TerminalInfo::default();
        }
    }

    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len(),
            )
        };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break; // stdin closed
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if contains_da1_reply(&buf) {
            break;
        }
    }
    TerminalInfo {
        theme: parse_theme(&buf),
        cell: parse_cell_size(&buf, cols, rows),
    }
}

/// Detect a Primary Device Attributes reply: `ESC [ ? <digits/;> c`.
fn contains_da1_reply(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let mut j = i + 3;
            while j < buf.len() {
                match buf[j] {
                    b'c' => return true,
                    b'0'..=b'9' | b';' => j += 1,
                    _ => break,
                }
            }
        }
        i += 1;
    }
    false
}

/// Derive the character cell's pixel size from XTWINOPS replies. `CSI 6;h;w t`
/// (from `CSI 16 t`) reports the cell directly and is authoritative; `CSI 4;h;w t`
/// (from `CSI 14 t`) reports the whole text area, which we divide by the grid.
/// Returns `None` if neither is present or the numbers are implausible.
fn parse_cell_size(buf: &[u8], cols: u16, rows: u16) -> Option<CellSize> {
    let mut cell = None;
    let mut area = None;
    for params in csi_t_replies(buf) {
        // Each reply is `code ; height ; width`.
        match params.as_slice() {
            [6, h, w] => cell = Some((*w, *h)),
            [4, h, w] => area = Some((*w, *h)),
            _ => {}
        }
    }
    if let Some((w, h)) = cell {
        return CellSize::from_dims(w, h);
    }
    if let Some((w, h)) = area {
        if cols > 0 && rows > 0 {
            return CellSize::from_dims(w / cols as u32, h / rows as u32);
        }
    }
    None
}

/// Yield the numeric parameters of each XTWINOPS reply (`ESC [ <digits/;> t`) in
/// `buf`. DA1 (`… c`) and OSC (`ESC ]`) replies don't match, so they're ignored.
fn csi_t_replies(buf: &[u8]) -> Vec<Vec<u32>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' {
            let start = i + 2;
            let mut j = start;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j > start && j < buf.len() && buf[j] == b't' {
                if let Ok(s) = std::str::from_utf8(&buf[start..j]) {
                    let params: Vec<u32> = s.split(';').filter_map(|p| p.parse().ok()).collect();
                    if !params.is_empty() {
                        out.push(params);
                    }
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Parse accumulated OSC replies into a full theme. Requires fg, bg, and all 16
/// palette entries. Returns `None` otherwise so the caller uses a default.
fn parse_theme(buf: &[u8]) -> Option<ThemeColors> {
    let mut fg = None;
    let mut bg = None;
    let mut palette: Vec<Option<String>> = vec![None; 16];

    for body in osc_bodies(buf) {
        let mut parts = body.split(';');
        match parts.next() {
            Some("10") => fg = parts.next().and_then(parse_osc_rgb),
            Some("11") => bg = parts.next().and_then(parse_osc_rgb),
            Some("4") => {
                if let (Some(idx), Some(spec)) = (parts.next(), parts.next()) {
                    if let Ok(i) = idx.parse::<usize>() {
                        if i < 16 {
                            palette[i] = parse_osc_rgb(spec);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let palette: Option<Vec<String>> = palette.into_iter().collect();
    Some(ThemeColors {
        fg: fg?,
        bg: bg?,
        palette: palette?,
    })
}

/// Yield the body (between `ESC ]` and the ST/BEL terminator) of each complete
/// OSC sequence in `buf`.
fn osc_bodies(buf: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b']' {
            let start = i + 2;
            let mut j = start;
            let mut body_end = None;
            while j < buf.len() {
                if buf[j] == 0x07 {
                    body_end = Some((j, j + 1)); // BEL terminator
                    break;
                }
                if buf[j] == 0x1b && j + 1 < buf.len() && buf[j + 1] == b'\\' {
                    body_end = Some((j, j + 2)); // ST terminator
                    break;
                }
                j += 1;
            }
            match body_end {
                Some((end, next)) => {
                    if let Ok(s) = std::str::from_utf8(&buf[start..end]) {
                        out.push(s.to_string());
                    }
                    i = next;
                    continue;
                }
                None => break, // incomplete trailing sequence
            }
        }
        i += 1;
    }
    out
}

/// Parse an OSC color spec `rgb:RRRR/GGGG/BBBB` (1–4 hex digits per channel)
/// into `#rrggbb`.
fn parse_osc_rgb(spec: &str) -> Option<String> {
    let rest = spec.strip_prefix("rgb:")?;
    let mut ch = rest.split('/');
    let r = scale_channel(ch.next()?)?;
    let g = scale_channel(ch.next()?)?;
    let b = scale_channel(ch.next()?)?;
    if ch.next().is_some() {
        return None;
    }
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

/// Scale a 1–4 hex-digit channel value to 8 bits.
fn scale_channel(h: &str) -> Option<u8> {
    if h.is_empty() || h.len() > 4 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(h, 16).ok()?;
    let max = (1u32 << (4 * h.len())) - 1;
    Some((value * 255 / max) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_reply() -> Vec<u8> {
        // fg white, bg near-black, then 16 palette entries (all mid-gray here).
        let mut s = String::from("\x1b]10;rgb:d0d0/d0d0/d0d0\x1b\\");
        s.push_str("\x1b]11;rgb:2121/2121/2121\x07"); // BEL terminator variant
        for n in 0..16 {
            s.push_str(&format!("\x1b]4;{n};rgb:8080/8080/8080\x1b\\"));
        }
        s.into_bytes()
    }

    #[test]
    fn parses_full_theme() {
        let t = parse_theme(&full_reply()).unwrap();
        assert_eq!(t.fg, "#d0d0d0");
        assert_eq!(t.bg, "#212121");
        assert_eq!(t.palette.len(), 16);
        assert_eq!(t.palette[0], "#808080");
        assert_eq!(t.palette[15], "#808080");
    }

    #[test]
    fn incomplete_theme_is_none() {
        // fg + bg but no palette -> None (caller falls back to default).
        let s = b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\";
        assert!(parse_theme(s).is_none());
    }

    #[test]
    fn scales_channel_widths() {
        assert_eq!(scale_channel("ffff"), Some(255));
        assert_eq!(scale_channel("0000"), Some(0));
        assert_eq!(scale_channel("ff"), Some(255));
        assert_eq!(scale_channel("80"), Some(128));
        assert_eq!(scale_channel("xy"), None);
        assert_eq!(scale_channel(""), None);
    }

    #[test]
    fn parses_two_digit_rgb() {
        assert_eq!(parse_osc_rgb("rgb:ff/00/80"), Some("#ff0080".to_string()));
    }

    #[test]
    fn cell_size_from_16t() {
        // `CSI 6;height;width t` — cell is 9 wide, 18 tall.
        let c = parse_cell_size(b"\x1b[6;18;9t", 80, 24).unwrap();
        assert_eq!((c.width, c.height), (9, 18));
    }

    #[test]
    fn cell_size_16t_wins_over_14t() {
        // Both present: the direct cell report (16t) is authoritative.
        let buf = b"\x1b[4;480;720t\x1b[6;18;9t";
        let c = parse_cell_size(buf, 80, 24).unwrap();
        assert_eq!((c.width, c.height), (9, 18));
    }

    #[test]
    fn cell_size_from_14t_divides_by_grid() {
        // Text area 720x480 over an 80x24 grid -> 9x20 cells.
        let c = parse_cell_size(b"\x1b[4;480;720t", 80, 24).unwrap();
        assert_eq!((c.width, c.height), (9, 20));
    }

    #[test]
    fn cell_size_absent_or_absurd_is_none() {
        assert!(parse_cell_size(b"", 80, 24).is_none());
        // Zero-sized cell is rejected.
        assert!(parse_cell_size(b"\x1b[6;0;0t", 80, 24).is_none());
        // DA1 reply must not be mistaken for a cell report.
        assert!(parse_cell_size(b"\x1b[?62;c", 80, 24).is_none());
    }
}
