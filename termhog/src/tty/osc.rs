//! Terminal colors as escape sequences carry them: the OSC 4/10/11 sequences
//! a program sends to set them, the same sequences a terminal answers a
//! query with, and the resets (OSC 104/110/111, and a full reset, `ESC c`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A color that sequences can set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Foreground,
    Background,
    /// A palette entry, 0 to 255.
    Palette(u8),
}

/// One color change a sequence makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Set a color, as CSS hex (`#rrggbb`).
    Set(Slot, String),
    /// Put a color back to the terminal's own.
    Reset(Slot),
    /// Put every palette entry back to the terminal's own.
    ResetPalette,
    /// Put every color back (a full terminal reset).
    ResetAll,
}

/// A set of colors, any of which may be unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Colors {
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub palette: BTreeMap<u8, String>,
}

impl Colors {
    pub fn apply(&mut self, change: &Change) {
        match change {
            Change::Set(Slot::Foreground, color) => self.foreground = Some(color.clone()),
            Change::Set(Slot::Background, color) => self.background = Some(color.clone()),
            Change::Set(Slot::Palette(i), color) => {
                self.palette.insert(*i, color.clone());
            }
            Change::Reset(Slot::Foreground) => self.foreground = None,
            Change::Reset(Slot::Background) => self.background = None,
            Change::Reset(Slot::Palette(i)) => {
                self.palette.remove(i);
            }
            Change::ResetPalette => self.palette.clear(),
            Change::ResetAll => *self = Colors::default(),
        }
    }

    /// Take every color `other` knows.
    pub fn merge(&mut self, other: &Colors) {
        self.foreground = other.foreground.clone().or(self.foreground.take());
        self.background = other.background.clone().or(self.background.take());
        self.palette
            .extend(other.palette.iter().map(|(i, c)| (*i, c.clone())));
    }

    /// A palette entry, if known.
    pub fn palette(&self, index: u8) -> Option<&str> {
        self.palette.get(&index).map(String::as_str)
    }
}

/// Picks the color sequences out of a byte stream, across reads.
#[derive(Default)]
pub struct Scanner {
    state: State,
    body: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Ground,
    Escape,
    /// In an OSC sequence, collecting its body.
    Osc,
    /// An ESC inside an OSC sequence, likely starting its terminator.
    OscEscape,
    /// In a DCS, SOS, PM or APC string, whose contents aren't colors.
    Text,
    TextEscape,
}

/// Longer than any color sequence, so a runaway one is dropped.
const MAX_BODY: usize = 4096;

impl Scanner {
    /// The color changes in the next part of the stream.
    pub fn scan(&mut self, mut bytes: &[u8]) -> Vec<Change> {
        let mut changes = Vec::new();
        while let Some((&b, rest)) = bytes.split_first() {
            bytes = rest;
            self.state = match (self.state, b) {
                // Most output is plain text: skip to the next escape.
                (State::Ground, 0x1b) => State::Escape,
                (State::Ground, _) => {
                    let next = bytes.iter().position(|&b| b == 0x1b);
                    bytes = &bytes[next.unwrap_or(bytes.len())..];
                    State::Ground
                }
                (State::Escape, b']') => {
                    self.body.clear();
                    State::Osc
                }
                (State::Escape, b'c') => {
                    changes.push(Change::ResetAll);
                    State::Ground
                }
                (State::Escape, b'P' | b'X' | b'^' | b'_') => State::Text,
                (State::Escape, 0x1b) => State::Escape,
                (State::Escape, _) => State::Ground,
                // BEL, or ST (`ESC \`), ends it. CAN and SUB cancel it.
                (State::Osc, 0x07) => self.finish(&mut changes),
                (State::Osc, 0x1b) => State::OscEscape,
                (State::Osc, 0x18 | 0x1a) => State::Ground,
                (State::Osc, _) if self.body.len() >= MAX_BODY => State::Ground,
                (State::Osc, _) => {
                    self.body.push(b);
                    State::Osc
                }
                (State::OscEscape, b'\\') => self.finish(&mut changes),
                // Some terminals end it with a bare ESC. This one may start
                // the next sequence.
                (State::OscEscape, _) => {
                    self.finish(&mut changes);
                    match b {
                        b']' => State::Osc,
                        0x1b => State::Escape,
                        _ => State::Ground,
                    }
                }
                (State::Text, 0x1b) => State::TextEscape,
                (State::Text, 0x18 | 0x1a) => State::Ground,
                (State::Text, _) => State::Text,
                (State::TextEscape, b'\\') => State::Ground,
                (State::TextEscape, _) => State::Text,
            };
        }
        changes
    }

    fn finish(&mut self, changes: &mut Vec<Change>) -> State {
        if let Ok(body) = std::str::from_utf8(&self.body) {
            changes.extend(parse(body));
        }
        self.body.clear();
        State::Ground
    }
}

/// The color changes an OSC sequence's body makes.
pub(super) fn parse(body: &str) -> Vec<Change> {
    let mut parts = body.split(';');
    let code = parts.next().unwrap_or_default();
    let args: Vec<&str> = parts.collect();
    let set = |slot, spec: &str| parse_color(spec).map(|c| Change::Set(slot, c));
    match code {
        // Pairs of palette index and color.
        "4" => args
            .chunks(2)
            .filter_map(|pair| set(Slot::Palette(pair[0].parse().ok()?), pair.get(1)?))
            .collect(),
        // Further values go on to the next colors in order: foreground,
        // background, then ones that aren't tracked.
        "10" => [Slot::Foreground, Slot::Background]
            .into_iter()
            .zip(&args)
            .filter_map(|(slot, spec)| set(slot, spec))
            .collect(),
        "11" => args
            .first()
            .and_then(|spec| set(Slot::Background, spec))
            .into_iter()
            .collect(),
        "104" if args.iter().all(|a| a.is_empty()) => vec![Change::ResetPalette],
        "104" => args
            .iter()
            .filter_map(|i| Some(Change::Reset(Slot::Palette(i.parse().ok()?))))
            .collect(),
        "110" => vec![Change::Reset(Slot::Foreground)],
        "111" => vec![Change::Reset(Slot::Background)],
        _ => Vec::new(),
    }
}

/// Parse an X11 color spec into CSS hex (`#rrggbb`).
fn parse_color(spec: &str) -> Option<String> {
    parse_rgb(spec).map(css_hex)
}

/// A color as CSS hex (`#rrggbb`).
pub fn css_hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Parse a color's channels: an X11 color spec, `rgb:R/G/B` (1 to 4 hex
/// digits each, scaled), `#RGB` in 3, 6, 9 or 12 digits (the leading bits),
/// or `rgbi:R/G/B` (0 to 1). Color names aren't supported.
pub fn parse_rgb(spec: &str) -> Option<[u8; 3]> {
    // urxvt puts an alpha value first, like `[90]#000000`.
    let spec = match spec.strip_prefix('[') {
        Some(rest) => &rest[rest.find(']')? + 1..],
        None => spec,
    };
    let channels: Vec<u8> = if let Some(hex) = spec.strip_prefix('#') {
        let n = hex.len() / 3;
        if hex.len() % 3 != 0 || !(1..=4).contains(&n) {
            return None;
        }
        (0..3)
            .map(|i| {
                let value = u32::from_str_radix(hex.get(i * n..(i + 1) * n)?, 16).ok()?;
                // The digits are the leading bits of a 16-bit value.
                Some((value << (16 - 4 * n) >> 8) as u8)
            })
            .collect::<Option<_>>()?
    } else if let Some(rest) = spec.strip_prefix("rgbi:") {
        rest.split('/')
            .map(|c| {
                let v: f32 = c.parse().ok()?;
                (0.0..=1.0).contains(&v).then(|| (v * 255.0).round() as u8)
            })
            .collect::<Option<_>>()?
    } else {
        // `rgba:` (in some replies) carries a fourth, alpha, channel.
        let rest = spec
            .strip_prefix("rgb:")
            .or_else(|| spec.strip_prefix("rgba:"))?;
        rest.split('/')
            .take(3)
            .map(scale_channel)
            .collect::<Option<_>>()?
    };
    channels.try_into().ok()
}

/// Scale a 1 to 4 hex-digit channel value to 8 bits.
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

    #[test]
    fn parses_color_specs() {
        assert_eq!(
            parse_color("rgb:ffff/0000/8080").as_deref(),
            Some("#ff0080")
        );
        assert_eq!(parse_color("rgb:f/0/8").as_deref(), Some("#ff0088"));
        assert_eq!(parse_color("#f00").as_deref(), Some("#f00000"));
        assert_eq!(parse_color("#123456").as_deref(), Some("#123456"));
        assert_eq!(parse_color("rgbi:1/0/0.5").as_deref(), Some("#ff0080"));
        assert_eq!(parse_color("[90]#102030").as_deref(), Some("#102030"));
        assert_eq!(parse_color("?"), None);
        assert_eq!(parse_color("red"), None);
    }

    #[test]
    fn finds_changes_across_reads() {
        let mut scanner = Scanner::default();
        assert!(scanner.scan(b"hi \x1b]4;1;rgb:ff/00/00;2;#00ff").is_empty());
        let changes = scanner.scan(b"00\x07 \x1b]110\x1b\\\x1bc");
        assert_eq!(
            changes,
            [
                Change::Set(Slot::Palette(1), "#ff0000".into()),
                Change::Set(Slot::Palette(2), "#00ff00".into()),
                Change::Reset(Slot::Foreground),
                Change::ResetAll,
            ]
        );
        // Strings that aren't OSC, and CSI sequences, hold no colors.
        assert!(
            scanner
                .scan(b"\x1bP\x1b]4;1;#fff\x07\x1b\\\x1b[c")
                .is_empty()
        );
    }
}
