//! Asks the host terminal for its colors and cell size, and picks the replies
//! out of the terminal's input.

use std::fs::File;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::osc::{self, Colors};

/// How long replies to the color/size query are expected on the terminal's
/// input. After this, input passes through unfiltered even if the terminal
/// never answered, so a terminal that doesn't can't hold up a program's own
/// queries for long.
const REPLY_WINDOW: Duration = Duration::from_secs(1);

/// A character cell's size in pixels, as the host terminal reports it. The
/// font can't be learned through a terminal, but the cell size can, and the
/// replay grid must match it to render without gaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellSize {
    pub width: u16,
    pub height: u16,
}

impl CellSize {
    /// Accept only sane cell dimensions, rejecting `0`, absurd sizes and
    /// shapes no font has (a cell is taller than it is wide), so a garbled
    /// reply can't poison the grid.
    fn from_dims(w: u32, h: u32) -> Option<CellSize> {
        if (2..=64).contains(&w) && (2..=128).contains(&h) && h >= w {
            Some(CellSize {
                width: w as u16,
                height: h as u16,
            })
        } else {
            None
        }
    }
}

/// What's known of the host terminal: its colors (those it reported) and its
/// character cell size.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalInfo {
    pub colors: Colors,
    pub cell: Option<CellSize>,
}

/// Whether to probe the terminal at all. GNU screen answers the probe's
/// sentinel itself but passes the color queries on, so their replies would
/// arrive after it and reach the program. A dumb terminal answers nothing.
pub fn should_probe() -> bool {
    std::env::var_os("STY").is_none() && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
}

/// Ask the host terminal for its foreground (OSC 10), background (OSC 11),
/// 16-color palette (OSC 4) and character-cell pixel size (XTWINOPS `CSI 16 t`,
/// with `CSI 14 t` as a fallback). This doesn't wait: the replies arrive on
/// the terminal's input among the user's keystrokes, where a [`ReplyFilter`]
/// takes them out.
///
/// A Primary Device Attributes request (`ESC [ c`) is sent last as a sentinel:
/// every terminal answers it, and terminals reply in order, so its answer marks
/// the end of the replies. A terminal that omits a reply (but answers DA1)
/// lacks that feature, and we keep the defaults.
///
/// Must be called with the terminal in raw mode, so replies aren't echoed.
pub fn send_queries(tty: &File) -> io::Result<()> {
    // The color queries end in BEL rather than ST, since urxvt answers an
    // ST-terminated query with a bare ESC. The DA1 sentinel goes last.
    let mut q = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for n in 0..16 {
        q.push_str(&format!("\x1b]4;{n};?\x07"));
    }
    q.push_str("\x1b[16t\x1b[14t");
    q.push_str("\x1b[c");
    let mut out = tty;
    out.write_all(q.as_bytes())?;
    out.flush()
}

/// Separates the terminal's replies to [`send_queries`] from keystrokes.
///
/// Replies can arrive any time, split across reads, and mixed with typing.
/// Until the DA1 reply ends them (or [`REPLY_WINDOW`] passes), the filter
/// removes reply sequences from the input and keeps them. A trailing partial
/// sequence that might still be a reply is held back until the next read, or
/// released by [`ReplyFilter::flush`] when no more input comes.
pub struct ReplyFilter {
    cols: u16,
    rows: u16,
    started: Instant,
    held: Vec<u8>,
    colors: Colors,
    /// The cell's size (from `CSI 16 t`), as width and height.
    cell: Option<(u32, u32)>,
    /// The text area's size (from `CSI 14 t`), as width and height.
    area: Option<(u32, u32)>,
    done: bool,
}

/// What an escape sequence at the start of the input turned out to be.
enum Sequence {
    /// A complete reply of this many bytes.
    Reply(usize, Reply),
    /// Not a reply. Forward it as typed.
    Other,
    /// Too short to tell yet.
    Partial,
}

enum Reply {
    /// An OSC 4/10/11 color report.
    Color,
    /// A `CSI … t` size report.
    Size,
    /// The DA1 answer, which ends the replies.
    Attributes,
}

impl ReplyFilter {
    /// `cols`/`rows` are the grid, used to derive a cell size from the
    /// `CSI 14 t` text-area reply when the terminal doesn't answer `CSI 16 t`.
    /// Without `probed`, no replies are expected and nothing is filtered.
    pub fn new(cols: u16, rows: u16, probed: bool) -> ReplyFilter {
        ReplyFilter {
            cols,
            rows,
            started: Instant::now(),
            held: Vec::new(),
            colors: Colors::default(),
            cell: None,
            area: None,
            done: !probed,
        }
    }

    /// Filter one read of terminal input. Returns the bytes to forward as
    /// keystrokes, plus the parsed terminal info once all replies are in.
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, Option<TerminalInfo>) {
        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(input);
        if self.is_done() {
            self.done = true;
            return (data, None);
        }

        let mut keys = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if data[i] != 0x1b {
                keys.push(data[i]);
                i += 1;
                continue;
            }
            match classify(&data[i..]) {
                Sequence::Reply(len, Reply::Attributes) => {
                    self.done = true;
                    keys.extend_from_slice(&data[i + len..]);
                    let info = TerminalInfo {
                        colors: std::mem::take(&mut self.colors),
                        cell: self.cell_size(),
                    };
                    return (keys, Some(info));
                }
                Sequence::Reply(len, reply) => {
                    self.absorb(&data[i..i + len], reply);
                    i += len;
                }
                Sequence::Other => {
                    keys.push(0x1b);
                    i += 1;
                }
                Sequence::Partial => {
                    self.held = data[i..].to_vec();
                    break;
                }
            }
        }
        (keys, None)
    }

    /// Release held bytes once no more input arrived for a while. A lone ESC
    /// keypress would otherwise wait for the next key.
    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }

    /// Whether the replies are over (or no longer expected).
    pub fn is_done(&self) -> bool {
        self.done || self.started.elapsed() > REPLY_WINDOW
    }

    /// Take in one reply (other than the DA1 answer).
    fn absorb(&mut self, reply: &[u8], kind: Reply) {
        // Between the introducer (`ESC ]` or `ESC [`) and the terminator.
        let body = &reply[2..];
        match kind {
            Reply::Color => {
                let body = body.strip_suffix(b"\x07").or(body.strip_suffix(b"\x1b\\"));
                if let Some(body) = body.and_then(|b| std::str::from_utf8(b).ok()) {
                    for change in osc::parse(body) {
                        self.colors.apply(&change);
                    }
                }
            }
            Reply::Size => {
                // The code, then the height and width.
                let params = std::str::from_utf8(&body[..body.len() - 1]).unwrap_or_default();
                let params: Vec<u32> = params.split(';').filter_map(|p| p.parse().ok()).collect();
                match params[..] {
                    [6, h, w] => self.cell = Some((w, h)),
                    [4, h, w] => self.area = Some((w, h)),
                    _ => {}
                }
            }
            Reply::Attributes => {}
        }
    }

    /// The cell size: as reported (`CSI 16 t`), or else the text area
    /// (`CSI 14 t`) divided by the grid. `None` if neither came, or the
    /// numbers are implausible.
    fn cell_size(&self) -> Option<CellSize> {
        if let Some((w, h)) = self.cell {
            return CellSize::from_dims(w, h);
        }
        let (w, h) = self.area?;
        if self.cols == 0 || self.rows == 0 {
            return None;
        }
        CellSize::from_dims(w / self.cols as u32, h / self.rows as u32)
    }
}

/// Classify the escape sequence at the start of `data` (which starts with
/// ESC) as one of the replies [`send_queries`] asks for: an OSC 4/10/11 color
/// report, a `CSI … t` size report, or the `CSI ? … c` DA1 answer.
fn classify(data: &[u8]) -> Sequence {
    /// Longer than any reply, so a runaway sequence is never held forever.
    const MAX_REPLY: usize = 64;
    match data.get(1) {
        None => Sequence::Partial,
        Some(b']') => {
            let body = &data[2..];
            let end = body.iter().enumerate().find_map(|(j, &b)| match b {
                0x07 => Some(j + 1),
                0x1b if body.get(j + 1) == Some(&b'\\') => Some(j + 2),
                _ => None,
            });
            let is_color = [&b"10;"[..], b"11;", b"4;"]
                .iter()
                .any(|p| body.starts_with(p) || p.starts_with(body));
            match end {
                Some(len) if is_color => Sequence::Reply(2 + len, Reply::Color),
                None if is_color && data.len() < MAX_REPLY => Sequence::Partial,
                _ => Sequence::Other,
            }
        }
        Some(b'[') => {
            let body = &data[2..];
            let params = body
                .iter()
                .take_while(|b| b.is_ascii_digit() || matches!(b, b';' | b'?'))
                .count();
            let reply = |kind| Sequence::Reply(2 + params + 1, kind);
            match body.get(params) {
                None if data.len() < MAX_REPLY => Sequence::Partial,
                Some(b'c') if body.first() == Some(&b'?') => reply(Reply::Attributes),
                Some(b't') if params > 0 && body.first() != Some(&b'?') => reply(Reply::Size),
                _ => Sequence::Other,
            }
        }
        Some(_) => Sequence::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full set of replies, then the DA1 sentinel: fg light gray, bg
    /// near-black, 16 palette entries (all mid-gray here), and the cell size.
    fn all_replies() -> Vec<u8> {
        let mut s = String::from("\x1b]10;rgb:d0d0/d0d0/d0d0\x1b\\");
        s.push_str("\x1b]11;rgb:2121/2121/2121\x07"); // BEL terminator variant
        for n in 0..16 {
            s.push_str(&format!("\x1b]4;{n};rgb:8080/8080/8080\x1b\\"));
        }
        s.push_str("\x1b[6;18;9t\x1b[?62;c");
        s.into_bytes()
    }

    /// The cell size from these size replies, on an 80x24 grid.
    fn cell_size(replies: &[u8]) -> Option<(u16, u16)> {
        let mut input = replies.to_vec();
        input.extend_from_slice(b"\x1b[?62;c");
        let (_, info) = ReplyFilter::new(80, 24, true).feed(&input);
        info.unwrap().cell.map(|c| (c.width, c.height))
    }

    #[test]
    fn parses_cell_size() {
        // The cell's own size (16t, height first) wins over the text area's.
        assert_eq!(cell_size(b"\x1b[4;480;720t\x1b[6;18;9t"), Some((9, 18)));
        // A 720x480 text area over an 80x24 grid is 9x20 cells.
        assert_eq!(cell_size(b"\x1b[4;480;720t"), Some((9, 20)));
        // The DA1 answer alone reports no size.
        assert_eq!(cell_size(b""), None);
    }

    #[test]
    fn filter_strips_replies_mixed_with_typing() {
        let mut f = ReplyFilter::new(80, 24, true);
        let mut input = b"ls".to_vec();
        input.extend(all_replies());
        input.extend_from_slice(b" -la\r");
        let (keys, info) = f.feed(&input);
        assert_eq!(keys, b"ls -la\r");
        let info = info.expect("DA1 completes the replies");
        assert_eq!(info.colors.background.as_deref(), Some("#212121"));
        assert_eq!(info.colors.palette(15), Some("#808080"));
        let cell = info.cell.unwrap();
        assert_eq!((cell.width, cell.height), (9, 18));
    }

    #[test]
    fn filter_handles_replies_split_across_reads() {
        let mut f = ReplyFilter::new(80, 24, true);
        let replies = all_replies();
        let (first, rest) = replies.split_at(7); // mid OSC 10 reply
        let (keys, info) = f.feed(first);
        assert!(keys.is_empty() && info.is_none(), "partial reply is held");
        let (keys, info) = f.feed(rest);
        assert!(keys.is_empty(), "{keys:?}");
        assert!(info.is_some());
    }

    #[test]
    fn filter_passes_keys_through() {
        let mut f = ReplyFilter::new(80, 24, true);
        // Arrow keys and other escape sequences are forwarded untouched.
        assert_eq!(f.feed(b"\x1b[A\x1b[1;5C\x1bOP").0, b"\x1b[A\x1b[1;5C\x1bOP");
        // A lone ESC could start a reply, so it's held until input goes quiet.
        assert!(f.feed(b"\x1b").0.is_empty());
        assert_eq!(f.flush(), b"\x1b");
        // After the DA1 reply, nothing is filtered anymore.
        let (_, info) = f.feed(b"\x1b[?62;c");
        assert!(info.is_some());
        assert_eq!(f.feed(b"\x1b[?62;c").0, b"\x1b[?62;c");
    }
}
