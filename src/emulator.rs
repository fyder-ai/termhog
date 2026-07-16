//! Terminal emulator abstraction.
//!
//! The projection consumes emulator-agnostic types (`Run`, `Style`, `Cursor`) so
//! the underlying VT engine is swappable and testable. The default engine is
//! `avt` (asciinema's own VT), which keeps the projection faithful to how
//! asciinema itself renders the same byte stream.

/// A color as reported by the emulator: either a palette index (0..=255) or a
/// direct RGB triple. Resolved to CSS by the projection using the captured theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Rendered attributes of a run of cells.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub inverse: bool,
}

/// A maximal run of adjacent cells sharing one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

/// Cursor position and visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
}

/// A virtual terminal fed the child's output byte stream, queried for its screen.
pub trait Emulator {
    /// Feed decoded output. Callers must hand over valid UTF-8 (the session
    /// thread buffers split multi-byte sequences before calling).
    fn feed_str(&mut self, s: &str);
    /// Resize the screen.
    fn resize(&mut self, cols: u16, rows: u16);
    /// Cursor state.
    fn cursor(&self) -> Cursor;
    /// Style-runs of the given viewport row (0-based). Empty for a blank row.
    fn row_runs(&self, row: u16) -> Vec<Run>;
}

/// `avt`-backed emulator.
pub struct AvtEmulator {
    vt: avt::Vt,
}

impl AvtEmulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        AvtEmulator {
            vt: avt::Vt::builder()
                .size(cols as usize, rows as usize)
                .build(),
        }
    }
}

impl Emulator for AvtEmulator {
    fn feed_str(&mut self, s: &str) {
        self.vt.feed_str(s);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.vt.resize(cols as usize, rows as usize);
    }

    fn cursor(&self) -> Cursor {
        let c = self.vt.cursor();
        Cursor {
            col: c.col as u16,
            row: c.row as u16,
            visible: c.visible,
        }
    }

    fn row_runs(&self, row: u16) -> Vec<Run> {
        let line = self.vt.line(row as usize);
        // `chunks` splits wherever the predicate returns true; Pen is Eq, so we
        // split at every style change to get maximal same-style runs.
        let mut runs: Vec<Run> = line
            .chunks(|a, b| a.pen() != b.pen())
            .map(|cells| Run {
                text: cells.iter().map(|c| c.char()).collect(),
                style: style_of(cells[0].pen()),
            })
            .collect();

        // avt pads each line to full width with default cells. Strip that
        // trailing invisible padding (default style, blank), but keep interior
        // spaces and any run with a real background — those are visible layout.
        if let Some(last) = runs.last_mut() {
            if last.style == Style::default() {
                let trimmed_len = last.text.trim_end_matches(' ').len();
                if trimmed_len == 0 {
                    runs.pop();
                } else {
                    last.text.truncate(trimmed_len);
                }
            }
        }
        runs
    }
}

fn style_of(pen: &avt::Pen) -> Style {
    Style {
        fg: pen.foreground().map(convert_color),
        bg: pen.background().map(convert_color),
        bold: pen.is_bold(),
        faint: pen.is_faint(),
        italic: pen.is_italic(),
        underline: pen.is_underline(),
        strikethrough: pen.is_strikethrough(),
        inverse: pen.is_inverse(),
    }
}

fn convert_color(c: avt::Color) -> Color {
    match c {
        avt::Color::Indexed(i) => Color::Indexed(i),
        avt::Color::RGB(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_row() {
        let mut e = AvtEmulator::new(80, 24);
        e.feed_str("hello");
        let runs = e.row_runs(0);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "hello");
        assert_eq!(runs[0].style, Style::default());
        assert_eq!(e.cursor().col, 5);
        assert_eq!(e.cursor().row, 0);
    }

    #[test]
    fn splits_runs_on_style_change() {
        let mut e = AvtEmulator::new(80, 24);
        // "AB" default, "CD" bold red fg, back to default "EF"
        e.feed_str("AB\x1b[1;31mCD\x1b[0mEF");
        let runs = e.row_runs(0);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].text, "AB");
        assert!(!runs[0].style.bold);
        assert_eq!(runs[1].text, "CD");
        assert!(runs[1].style.bold);
        assert_eq!(runs[1].style.fg, Some(Color::Indexed(1)));
        assert_eq!(runs[2].text, "EF");
        assert_eq!(runs[2].style, Style::default());
    }

    #[test]
    fn truecolor_and_attrs() {
        let mut e = AvtEmulator::new(80, 24);
        e.feed_str("\x1b[38;2;10;20;30;4mx");
        let runs = e.row_runs(0);
        assert_eq!(runs[0].style.fg, Some(Color::Rgb(10, 20, 30)));
        assert!(runs[0].style.underline);
    }

    #[test]
    fn second_row_after_newline() {
        let mut e = AvtEmulator::new(80, 24);
        e.feed_str("one\r\ntwo");
        assert_eq!(e.row_runs(0)[0].text, "one");
        assert_eq!(e.row_runs(1)[0].text, "two");
        assert_eq!(e.cursor().row, 1);
    }
}
