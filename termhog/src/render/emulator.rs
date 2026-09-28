//! The terminal emulator: `avt` (asciinema's own VT), which keeps the replay
//! faithful to how asciinema renders the same byte stream. It reports each
//! row as runs of cells sharing a style, ready for the projection.

/// A palette index or a direct RGB color. The projection resolves it to CSS
/// through the theme.
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
    /// The cell width of each character in `text`: 1, or 2 for wide ones.
    pub widths: Vec<u8>,
}

impl Run {
    /// How many grid cells the run covers.
    pub fn width(&self) -> u16 {
        self.widths.iter().map(|&w| w as u16).sum()
    }
}

/// A virtual terminal fed the child's output, queried for its screen.
pub struct Emulator {
    vt: avt::Vt,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        Emulator {
            // Only the viewport is projected, so keep no scrollback. Otherwise
            // every line scrolled off stays in memory for the whole session.
            vt: avt::Vt::builder()
                .size(cols as usize, rows as usize)
                .scrollback_limit(0)
                .build(),
        }
    }

    /// An emulator in the state [`Emulator::dump`] saved.
    pub fn restore(cols: u16, rows: u16, dump: &str) -> Self {
        let mut emu = Emulator::new(cols, rows);
        emu.vt.feed_str(dump);
        emu
    }

    /// The escape sequences that redraw the current state (screen, cursor,
    /// pen and modes) on a fresh emulator of the same size.
    pub fn dump(&self) -> String {
        self.vt.dump()
    }

    /// Feed output, already decoded (the screen's `Utf8Decoder` holds back
    /// multi-byte sequences split across reads).
    pub fn feed_str(&mut self, s: &str) {
        self.vt.feed_str(s);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.vt.resize(cols as usize, rows as usize);
    }

    /// Style-runs of a viewport row (0-based), with the cursor's cell in
    /// reverse video when it's visible there. Empty for a blank row.
    pub fn row_runs(&self, row: u16) -> Vec<Run> {
        let c = self.vt.cursor();
        self.runs(row, (c.visible && c.row == row as usize).then_some(c.col))
    }

    /// Style-runs of a row, with the cursor drawn at `cursor_col`.
    fn runs(&self, row: u16, cursor_col: Option<usize>) -> Vec<Run> {
        let line = self.vt.line(row as usize);
        let cells = line.cells();
        // On the right half of a wide character, the cursor covers the whole
        // character. Past the last column (pending wrap), it sits on the last.
        let cursor = cursor_col.filter(|_| !cells.is_empty()).map(|col| {
            let col = col.min(cells.len() - 1);
            if col > 0 && cells[col].width() == 0 {
                col - 1
            } else {
                col
            }
        });

        // Runs also break around box-drawing and block characters, so each such
        // run repeats a single character the projection can draw as a shape.
        let drawn = |ch: char| super::glyphs::is_drawn(ch).then_some(ch);
        let mut runs: Vec<Run> = Vec::new();
        for (col, cell) in cells.iter().enumerate() {
            let width = cell.width();
            if width == 0 {
                continue; // right half of a wide character, drawn by its left
            }
            let mut style = style_of(cell.pen());
            if cursor == Some(col) {
                style.inverse = !style.inverse;
            }
            let same_glyph_kind =
                |last: &Run| last.text.chars().next().and_then(drawn) == drawn(cell.char());
            match runs.last_mut() {
                Some(last) if last.style == style && same_glyph_kind(last) => {
                    last.text.push(cell.char());
                    last.widths.push(width);
                }
                _ => runs.push(Run {
                    text: cell.char().to_string(),
                    style,
                    widths: vec![width],
                }),
            }
        }

        trim_trailing_blanks(&mut runs);
        runs
    }
}

/// Drop the invisible padding at the end of a row: avt pads every line to full
/// width with default-styled blanks. Only a default-styled final run is
/// trimmed, since blanks with a background (or the cursor) are visible. The
/// run before it differs in style or is a drawn glyph, so it's never padding.
fn trim_trailing_blanks(runs: &mut Vec<Run>) {
    let Some(last) = runs.last_mut().filter(|r| r.style == Style::default()) else {
        return;
    };
    let text_len = last.text.trim_end_matches(' ').len();
    // Blanks are single-width spaces, so their byte count is their cell count.
    let kept_chars = last.widths.len() - (last.text.len() - text_len);
    last.text.truncate(text_len);
    last.widths.truncate(kept_chars);
    if last.text.is_empty() {
        runs.pop();
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
    fn splits_runs_on_style_change() {
        let mut e = Emulator::new(80, 24);
        // "AB" default, "CD" bold red fg, back to default "EF"
        e.feed_str("AB\x1b[1;31mCD\x1b[0mEF");
        let runs = e.runs(0, None);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].text, "AB");
        assert!(!runs[0].style.bold);
        assert_eq!(runs[1].text, "CD");
        assert!(runs[1].style.bold);
        assert_eq!(runs[1].style.fg, Some(Color::Indexed(1)));
        assert_eq!(runs[2].text, "EF");
        assert_eq!(runs[2].style, Style::default());
    }

    // The erase tests guard the termhog-avt fix
    // (https://github.com/asciinema/avt/pull/29), until upstream has it.
    #[test]
    fn erase_does_not_extend_underline() {
        let mut e = Emulator::new(80, 24);
        // Underline "Yes", then clear to end of line before resetting the pen,
        // the way crossterm-based TUIs redraw a line.
        e.feed_str("\x1b[4mYes\x1b[K\x1b[0m No");
        let runs = e.runs(0, None);
        assert_eq!(runs[0].text, "Yes");
        assert!(runs[0].style.underline);
        assert_eq!(runs[1].text, " No");
        assert_eq!(runs.len(), 2);

        let mut e = Emulator::new(80, 24);
        e.feed_str("\x1b[4mYes\x1b[K\x1b[0m");
        let runs = e.runs(0, None);
        assert_eq!(runs.len(), 1, "erased cells render as nothing: {runs:?}");

        // Text after the erased gap: the gap and the text are one plain run.
        let mut e = Emulator::new(40, 2);
        e.feed_str("\x1b[4mYes\x1b[K\x1b[0m\x1b[20GNo");
        let runs = e.runs(0, None);
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[1].style, Style::default());
        assert!(runs[1].text.ends_with("No"));
    }

    #[test]
    fn erase_keeps_background() {
        let mut e = Emulator::new(10, 2);
        // Erased cells take the background color (BCE), so it stays visible.
        e.feed_str("\x1b[4;41mA\x1b[K");
        let runs = e.runs(0, None);
        assert_eq!(runs[0].text, "A");
        assert_eq!(runs[1].text, " ".repeat(9));
        assert_eq!(runs[1].style.bg, Some(Color::Indexed(1)));
        assert!(!runs[1].style.underline);
    }

    #[test]
    fn cursor_is_reverse_video_and_covers_wide_chars() {
        let mut e = Emulator::new(80, 24);
        e.feed_str("a日b");
        assert_eq!(e.runs(0, None)[0].widths, [1, 2, 1]);
        // Column 2 is the right half of 日, so the whole character is marked.
        let runs = e.runs(0, Some(2));
        assert_eq!(runs[1].text, "日");
        assert!(runs[1].style.inverse);
        // Past the text, the cursor gets its own blank cell.
        let runs = e.runs(0, Some(6));
        let last = runs.last().unwrap();
        assert_eq!((last.text.as_str(), last.width()), (" ", 1));
        assert!(last.style.inverse);
        assert_eq!(runs.iter().map(Run::width).sum::<u16>(), 7);
    }
}
