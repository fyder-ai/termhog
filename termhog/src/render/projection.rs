//! Projects emulator screen state into rrweb events.
//!
//! The player reconstructs a DOM, so we render the terminal as a fixed
//! `rows`×`cols` grid: a `<pre>` containing one `<div>` per row, each holding
//! a `<span>` per style run, which holds a fixed-width `<span>` per cell. Row
//! `<div>`s paint all backgrounds and box-drawing glyphs (see `paint`).
//!
//! A keyframe is a Meta event and a FullSnapshot of the skeleton (stylesheet
//! and empty rows), followed by mutations filling in the rows, at the same
//! timestamp, so no event gets too large. Between keyframes, mutations
//! replace the children of the rows that changed. The cursor is drawn by
//! overlaying reverse-video on its cell, so cursor moves surface as ordinary
//! row changes.

use serde_json::{Value, json};

use super::emulator::{Color, Emulator, Run, Style};
use super::rrweb::*;
use super::theme::Theme;
use super::{glyphs, paint};
use crate::tty::osc::{css_hex, parse_rgb};
use crate::tty::probe::CellSize;

// Stable skeleton node ids.
const ID_DOCUMENT: i64 = 1;
const ID_HTML: i64 = 2;
const ID_HEAD: i64 = 3;
const ID_STYLE: i64 = 4;
const ID_STYLE_TEXT: i64 = 5;
const ID_BODY: i64 = 6;
const ID_PRE: i64 = 7;
/// Row `<div>` ids are `ID_ROW_BASE + row`, stable across mutations.
const ID_ROW_BASE: i64 = 100;
/// Dynamic node ids (spans / text) start above any possible row id.
const ID_DYNAMIC_BASE: i64 = 1_000_000;

/// The cell size when the terminal doesn't report one. A 13px monospace
/// font's advance (~0.6em) is close to its width, keeping the player's
/// viewport aligned with the content.
const DEFAULT_CELL: CellSize = CellSize {
    width: 8,
    height: 16,
};
const FONT_STACK: &str = "'DejaVu Sans Mono','Menlo','Consolas',monospace";
const HREF: &str = "termhog://session";

/// Turns emulator state into rrweb events, tracking each row's DOM so only
/// changed rows are resent between keyframes.
pub struct Projector {
    cols: u16,
    rows: u16,
    /// The colors to draw in. After changing them, call
    /// [`Projector::repaint`].
    pub theme: Theme,
    /// The character cell's size in pixels, which sizes the viewport and the
    /// grid. Send a keyframe after changing it.
    pub cell: CellSize,
    next_id: i64,
    /// What each row shows: the runs last sent (cursor included), and the ids
    /// of its children, for removing them.
    shown: Vec<(Vec<Run>, Vec<i64>)>,
    /// The stylesheet last sent.
    sheet: String,
    /// Every row must be sent again, as the colors changed.
    repaint: bool,
}

impl Projector {
    pub fn new(cols: u16, rows: u16, theme: Theme, cell: Option<CellSize>) -> Self {
        Projector {
            cols,
            rows,
            theme,
            cell: cell.unwrap_or(DEFAULT_CELL),
            next_id: ID_DYNAMIC_BASE,
            shown: vec![Default::default(); rows as usize],
            sheet: String::new(),
            repaint: false,
        }
    }

    /// Redraw everything in the next diff, as after a color change: the
    /// stylesheet, and every row.
    pub fn repaint(&mut self) {
        self.repaint = true;
    }

    /// The grid's size as `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Resize the grid. Send a keyframe after.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.shown = vec![Default::default(); rows as usize];
    }

    fn cell_w(&self) -> u32 {
        self.cell.width as u32
    }

    fn cell_h(&self) -> u32 {
        self.cell.height as u32
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Meta event: sizes the player viewport in pixels.
    fn meta(&self, timestamp_ms: u64) -> Value {
        json!({
            "type": META,
            "data": {
                "href": HREF,
                "width": self.row_width(),
                "height": self.rows as u32 * self.cell_h(),
            },
            "timestamp": timestamp_ms,
        })
    }

    /// A keyframe: Meta, then a FullSnapshot of the skeleton (stylesheet and
    /// painted but empty rows), then mutations that fill in the rows, all at
    /// the same timestamp. The player only starts a seek from a snapshot with
    /// a Meta in front of it, as rrweb's own recorder always emits. Splitting
    /// the content out keeps every event small however large the screen is,
    /// since PostHog limits each event's size. Resets the tracked per-row
    /// state to match.
    pub fn keyframe(&mut self, emu: &Emulator, timestamp_ms: u64) -> Vec<Event> {
        let mut row_divs = Vec::with_capacity(self.rows as usize);
        let mut batcher = Batcher::new(timestamp_ms);
        self.sheet = self.stylesheet();
        self.repaint = false;
        for row in 0..self.rows {
            let runs = emu.row_runs(row);
            let children = self.alloc_row_children(&runs);
            let attributes = Attributes::style(&self.row_css(&runs));
            row_divs.push(Node::element("div", row_div_id(row), attributes));
            batcher.push_row(row_div_id(row), &children, Vec::new(), None);
            self.shown[row as usize] = (runs, child_ids(&children));
        }

        let node = json!({
            "type": NODE_DOCUMENT,
            "id": ID_DOCUMENT,
            "childNodes": [ {
                "type": NODE_ELEMENT, "tagName": "html", "attributes": {}, "id": ID_HTML,
                "childNodes": [
                    {
                        "type": NODE_ELEMENT, "tagName": "head", "attributes": {}, "id": ID_HEAD,
                        "childNodes": [ {
                            "type": NODE_ELEMENT, "tagName": "style",
                            "attributes": {"type": "text/css"}, "id": ID_STYLE,
                            "childNodes": [ {
                                "type": NODE_TEXT, "id": ID_STYLE_TEXT,
                                "textContent": self.sheet,
                            } ],
                        } ],
                    },
                    {
                        "type": NODE_ELEMENT, "tagName": "body", "attributes": {}, "id": ID_BODY,
                        "childNodes": [ {
                            "type": NODE_ELEMENT, "tagName": "pre", "attributes": {}, "id": ID_PRE,
                            "childNodes": row_divs,
                        } ],
                    },
                ],
            } ],
        });

        let snapshot = json!({
            "type": FULL_SNAPSHOT,
            "data": { "node": node, "initialOffset": {"top": 0, "left": 0} },
            "timestamp": timestamp_ms,
        });
        let mut events = vec![
            Event::Other(self.meta(timestamp_ms)),
            Event::Other(snapshot),
        ];
        events.extend(batcher.finish());
        events
    }

    /// Mutations for whatever changed since the last emit (the stylesheet, and
    /// every row after a [`Projector::repaint`], or else only the rows that
    /// changed), split so no event gets too large. Empty if nothing changed.
    pub fn diff(&mut self, emu: &Emulator, timestamp_ms: u64) -> Vec<Event> {
        let mut batcher = Batcher::new(timestamp_ms);
        let sheet = self.stylesheet();
        if sheet != self.sheet {
            batcher.set_text(ID_STYLE_TEXT, &sheet);
            self.sheet = sheet;
        }
        let repaint = std::mem::take(&mut self.repaint);
        for row in 0..self.rows {
            let runs = emu.row_runs(row);
            if !repaint && runs == self.shown[row as usize].0 {
                continue;
            }
            let children = self.alloc_row_children(&runs);
            let css = self.row_css(&runs);
            let shown = (runs, child_ids(&children));
            let (_, removed) = std::mem::replace(&mut self.shown[row as usize], shown);
            batcher.push_row(row_div_id(row), &children, removed, Some(css));
        }
        batcher.finish()
    }

    /// Allocate ids for a row's children, one span per run. Keyframes and
    /// diffs both build rows through this, so they stay consistent. An empty
    /// row's `<div>` keeps its height from the stylesheet.
    fn alloc_row_children(&mut self, runs: &[Run]) -> Vec<RowChild> {
        runs.iter()
            .map(|run| {
                let span_id = self.alloc_id();
                // The row paints backgrounds and drawn glyphs, so a run with
                // nothing else to show (blanks without a line through or under
                // them, or drawn glyphs) is just an empty span of its width.
                let decorated = run.style.underline || run.style.strikethrough;
                let blank = run.text.chars().all(|c| c == ' ') && !decorated;
                if blank || run.text.chars().next().is_some_and(glyphs::is_drawn) {
                    let css = format!("width:{}px", run.width() as u32 * self.cell_w());
                    return RowChild {
                        span_id,
                        css,
                        cells: Vec::new(),
                    };
                }
                // Text gets one span per cell.
                let cells = run
                    .text
                    .chars()
                    .zip(&run.widths)
                    .map(|(ch, &width)| Cell {
                        span_id: self.alloc_id(),
                        text_id: self.alloc_id(),
                        ch,
                        wide: width > 1,
                    })
                    .collect();
                RowChild {
                    span_id,
                    css: self.style_css(&run.style),
                    cells,
                }
            })
            .collect()
    }

    fn stylesheet(&self) -> String {
        // Every character sits in its own fixed-width cell span (`w` marks a
        // wide one), so glyph widths in the viewer's font can't shift the
        // grid. Cells inherit text-decoration explicitly, since it doesn't
        // reach into inline-block children on its own.
        //
        // Rows paint all backgrounds (see `paint`): each is opaque and extends
        // OVERLAP_PX into the next row, which is painted over it, so a
        // fractionally scaled row edge never exposes the page behind. The
        // default row paint is a gradient, not a background-color: two layers
        // anti-aliased along the same edge would let the page show through.
        let (cw, ch) = (self.cell_w(), self.cell_h());
        // Monospace advance is ~0.6em, so a font this size is about as wide
        // as the cell, but never taller than it (which would overflow it).
        let font_size = ((cw as f64 / 0.6).round() as u32).clamp(6, ch.saturating_sub(1).max(6));
        format!(
            "body{{margin:0;background:{bg}}}\
             pre{{margin:0;font-family:{font};font-size:{fs}px;line-height:{ch}px;\
             white-space:pre;color:{fg}}}\
             pre span{{display:inline-block;height:{ch}px;line-height:{ch}px;\
             vertical-align:top}}\
             pre span span{{width:{cw}px;text-decoration:inherit}}\
             pre span span.w{{width:{cw2}px}}\
             div{{height:{ch}px;padding-bottom:{ov}px;margin-bottom:-{ov}px;\
             background-image:linear-gradient({bg},{bg});background-size:{w}px {ph}px;\
             background-repeat:no-repeat}}",
            bg = self.theme.background(),
            fg = self.theme.foreground(),
            font = FONT_STACK,
            fs = font_size,
            cw2 = cw * 2,
            ov = paint::OVERLAP_PX,
            w = self.row_width(),
            ph = ch + paint::OVERLAP_PX,
        )
    }

    /// Inline CSS painting a row's backgrounds and drawn glyphs (see
    /// [`paint`]). Empty when the row shows only the default background.
    fn row_css(&self, runs: &[Run]) -> String {
        let grid = paint::Grid {
            cell_w: self.cell_w(),
            cell_h: self.cell_h(),
            row_width: self.row_width(),
        };
        paint::row_css(runs, grid, &self.theme.background(), |style| {
            let (fg, bg) = self.colors(style);
            // Text shows faint with opacity, which a painted glyph can't use,
            // so it's drawn in a color 60% of the way to the background.
            let fg = if style.faint { mix(&fg, &bg, 0.6) } else { fg };
            let has_bg = style.bg.is_some() || style.inverse;
            (fg, has_bg.then_some(bg))
        })
    }

    /// A row's full width in pixels.
    fn row_width(&self) -> u32 {
        self.cols as u32 * self.cell_w()
    }

    /// Inline CSS for a run's text. Empty for the default style (the `<pre>`
    /// already sets the base color). Backgrounds are painted by the row.
    fn style_css(&self, style: &Style) -> String {
        if *style == Style::default() {
            return String::new();
        }
        let (fg, _) = self.colors(style);

        let mut css = format!("color:{fg};");
        if style.bold {
            css.push_str("font-weight:700;");
        }
        if style.faint {
            css.push_str("opacity:.6;");
        }
        if style.italic {
            css.push_str("font-style:italic;");
        }
        match (style.underline, style.strikethrough) {
            (true, true) => css.push_str("text-decoration:underline line-through;"),
            (true, false) => css.push_str("text-decoration:underline;"),
            (false, true) => css.push_str("text-decoration:line-through;"),
            (false, false) => {}
        }
        css
    }

    /// The CSS foreground and background a style shows, after inverse.
    fn colors(&self, style: &Style) -> (String, String) {
        let fg = self
            .resolve(style.fg)
            .unwrap_or_else(|| self.theme.foreground());
        let bg = self
            .resolve(style.bg)
            .unwrap_or_else(|| self.theme.background());
        if style.inverse { (bg, fg) } else { (fg, bg) }
    }

    /// Resolve an emulator color to CSS hex.
    fn resolve(&self, color: Option<Color>) -> Option<String> {
        match color? {
            Color::Rgb(r, g, b) => Some(css_hex([r, g, b])),
            Color::Indexed(i) => Some(self.theme.indexed(i)),
        }
    }
}

/// Hide the player's mouse cursor, which a terminal doesn't have. rrweb adds
/// a permanent `touch-device` class if any event is a TouchStart
/// MouseInteraction (its init scan checks only source and type), and both
/// rrweb's and PostHog's CSS hide the cursor under that class.
///
/// With `id: -1` (no target), the replayer bails out of the handler before
/// moving the mouse, so applying the event does nothing (no move, trail or
/// tap ring), even on seek.
pub fn hide_mouse(timestamp_ms: u64) -> Event {
    Event::Other(json!({
        "type": INCREMENTAL_SNAPSHOT,
        "data": {
            "source": MOUSE_INTERACTION,
            "type": TOUCH_START,
            "id": -1,
            "pointerType": POINTER_TOUCH,
        },
        "timestamp": timestamp_ms,
    }))
}

/// A no-op event that marks user activity so the player counts active time
/// and doesn't treat typing as skippable. rrweb counts sources in
/// `(Mutation, Input]` as active. A MouseInteraction with `id: -1` counts yet
/// applies as a no-op (the handler bails on `id === -1`). Carries no
/// keystroke content.
pub fn activity(timestamp_ms: u64) -> Event {
    Event::Other(json!({
        "type": INCREMENTAL_SNAPSHOT,
        "data": { "source": MOUSE_INTERACTION, "type": MOUSE_UP, "id": -1 },
        "timestamp": timestamp_ms,
    }))
}

/// The color `amount` (0 to 1) of the way from `from` to `to`, both CSS hex.
fn mix(from: &str, to: &str, amount: f32) -> String {
    let (Some(from_rgb), Some(to_rgb)) = (parse_rgb(from), parse_rgb(to)) else {
        return from.to_string();
    };
    css_hex(std::array::from_fn(|i| {
        let (a, b) = (f32::from(from_rgb[i]), f32::from(to_rgb[i]));
        (a + (b - a) * amount).round() as u8
    }))
}

/// A run's span in a row `<div>`, with allocated ids. `cells` is empty for a
/// run the row paints (blanks and drawn glyphs).
struct RowChild {
    span_id: i64,
    css: String,
    cells: Vec<Cell>,
}

/// One grid cell of a run.
struct Cell {
    span_id: i64,
    text_id: i64,
    ch: char,
    /// Spans two columns (the stylesheet's `w` class).
    wide: bool,
}

/// The top-level child ids of a row, for removal in a later mutation. Removing a
/// span removes its descendants with it, so only the run span id is tracked.
fn child_ids(children: &[RowChild]) -> Vec<i64> {
    children.iter().map(|c| c.span_id).collect()
}

/// Most grid cells one mutation event may carry (each cell is two nodes of
/// roughly 100 bytes of JSON), keeping events well under PostHog's size limit.
const MAX_CELLS_PER_EVENT: usize = 2000;

/// Collects row changes into mutation events at one timestamp, starting a new
/// event whenever the current one would pass [`MAX_CELLS_PER_EVENT`].
struct Batcher {
    timestamp: u64,
    events: Vec<Event>,
    mutation: Mutation,
    cells: usize,
}

impl Batcher {
    fn new(timestamp: u64) -> Batcher {
        Batcher {
            timestamp,
            events: Vec::new(),
            mutation: Mutation::default(),
            cells: 0,
        }
    }

    /// Replace text node `id`'s content.
    fn set_text(&mut self, id: i64, value: &str) {
        let value = value.to_string();
        self.mutation.texts.push(TextChange { id, value });
    }

    /// Replace the children of row div `parent`: remove the `removed` ids and
    /// add `children`. `css` repaints the row (empty removes its style).
    fn push_row(
        &mut self,
        parent: i64,
        children: &[RowChild],
        removed: Vec<i64>,
        css: Option<String>,
    ) {
        // Rough size: one per cell, plus the run's span itself.
        let cells: usize = children.iter().map(|c| c.cells.len() + 1).sum();
        if self.cells > 0 && self.cells + cells > MAX_CELLS_PER_EVENT {
            self.flush();
        }
        self.cells += cells;
        let mutation = &mut self.mutation;
        if let Some(css) = css {
            let style = (!css.is_empty()).then_some(css);
            let attributes = StyleChange { style };
            mutation.attributes.push(AttributeChange {
                id: parent,
                attributes,
            });
        }
        let removes = removed.into_iter().map(|id| Remove {
            parent_id: parent,
            id,
        });
        mutation.removes.extend(removes);
        for child in children {
            let span = Node::element("span", child.span_id, Attributes::style(&child.css));
            mutation.adds.push(Add::new(parent, span));
            for cell in &child.cells {
                let class = cell.wide.then_some("w");
                let attributes = Attributes { style: None, class };
                let span = Node::element("span", cell.span_id, attributes);
                mutation.adds.push(Add::new(child.span_id, span));
                let text = Node::text(cell.text_id, cell.ch.to_string());
                mutation.adds.push(Add::new(cell.span_id, text));
            }
        }
    }

    fn flush(&mut self) {
        if self.mutation.is_empty() {
            return;
        }
        let mutation = std::mem::take(&mut self.mutation);
        let timestamp = self.timestamp;
        self.events.push(Event::Mutation {
            mutation,
            timestamp,
        });
        self.cells = 0;
    }

    fn finish(mut self) -> Vec<Event> {
        self.flush();
        self.events
    }
}

fn row_div_id(row: u16) -> i64 {
    ID_ROW_BASE + row as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(events: Vec<Event>) -> Vec<Value> {
        events.iter().map(Event::to_json).collect()
    }

    /// Apply a keyframe's row mutations to its skeleton snapshot, the way the
    /// replayer does, and return the complete document node.
    fn assemble(events: &[Value]) -> Value {
        use std::collections::HashMap;
        fn attach(node: &mut Value, adds: &mut HashMap<i64, Vec<Value>>) {
            if let Some(children) = node["id"].as_i64().and_then(|id| adds.remove(&id)) {
                node["childNodes"].as_array_mut().unwrap().extend(children);
            }
            if let Some(children) = node["childNodes"].as_array_mut() {
                for child in children {
                    attach(child, adds);
                }
            }
        }
        assert_eq!(events[0]["type"], META);
        assert_eq!(events[1]["type"], FULL_SNAPSHOT);
        let mut adds: HashMap<i64, Vec<Value>> = HashMap::new();
        for event in &events[2..] {
            assert_eq!(event["timestamp"], events[0]["timestamp"]);
            for add in event["data"]["adds"].as_array().unwrap() {
                let parent = add["parentId"].as_i64().unwrap();
                adds.entry(parent).or_default().push(add["node"].clone());
            }
        }
        let mut doc = events[1]["data"]["node"].clone();
        attach(&mut doc, &mut adds);
        assert!(adds.is_empty(), "every add has a parent");
        doc
    }

    fn pre_children(events: &[Value]) -> Vec<Value> {
        // document -> html -> [head, body] -> pre -> childNodes
        let doc = assemble(events);
        let html = &doc["childNodes"][0];
        let body = &html["childNodes"][1];
        let pre = &body["childNodes"][0];
        pre["childNodes"].as_array().unwrap().clone()
    }

    /// A run span's text, read back from its cell spans.
    fn run_text(span: &Value) -> String {
        span["childNodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cell| cell["childNodes"][0]["textContent"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn diff_emits_mutation_on_change() {
        let mut e = Emulator::new(80, 10);
        e.feed_str("hi");
        let mut p = Projector::new(80, 10, Theme::default(), None);
        let _ = p.keyframe(&e, 0);
        assert!(p.diff(&e, 1).is_empty(), "nothing changed yet");
        e.feed_str("!"); // row 0 becomes "hi!"
        let events = json(p.diff(&e, 2));
        assert_eq!(events.len(), 1, "one small change is one event");
        let m = &events[0];
        assert_eq!(m["type"], INCREMENTAL_SNAPSHOT);
        assert_eq!(m["data"]["source"], MUTATION);
        assert!(!m["data"]["removes"].as_array().unwrap().is_empty());
        let adds = m["data"]["adds"].as_array().unwrap();

        // Adds must be flat: every added node has no nested childNodes (the
        // replayer builds with skipChild:true), and each arrives after its
        // parent (the row div, or a span added earlier in this mutation).
        let mut known = vec![row_div_id(0)];
        for a in adds {
            let kids = a["node"]["childNodes"].as_array();
            assert!(
                kids.map(|k| k.is_empty()).unwrap_or(true),
                "add is not flat: {a}"
            );
            let parent = a["parentId"].as_i64().unwrap();
            assert!(known.contains(&parent), "parent not added first: {a}");
            known.push(a["node"]["id"].as_i64().unwrap());
        }
        // The text adds spell the new row content, one character per cell.
        // The blank cell under the cursor is painted by the row, not text.
        let text: String = adds
            .iter()
            .filter(|a| a["node"]["type"] == NODE_TEXT)
            .map(|a| a["node"]["textContent"].as_str().unwrap())
            .collect();
        assert_eq!(text, "hi!");
    }

    #[test]
    fn cursor_overlay_marks_a_cell() {
        let mut e = Emulator::new(80, 3);
        e.feed_str("ab"); // cursor now at col 2, row 0
        let mut p = Projector::new(80, 3, Theme::default(), None);
        let rows = pre_children(&json(p.keyframe(&e, 0)));
        // Row 0 paints the reverse-video cursor cell at col 2 (8px cells), in
        // the default foreground color.
        let css = rows[0]["attributes"]["style"].as_str().unwrap();
        assert!(css.contains("#d0d0d0 16px,#d0d0d0 24px"), "got: {css}");
        // Rows without a cursor or colors carry no paint of their own.
        assert!(rows[1]["attributes"]["style"].is_null());
    }

    #[test]
    fn big_screens_split_into_small_events() {
        // A 200x60 screen of background-colored spaces (like opencode) is
        // ~1.4 MB as one event, over PostHog's per-event limit.
        let (cols, rows) = (200, 60);
        let mut e = Emulator::new(cols, rows);
        e.feed_str("\x1b[48;5;232m\x1b[2J");
        let mut p = Projector::new(cols, rows, Theme::default(), None);
        let keyframe = json(p.keyframe(&e, 0));
        assert!(keyframe.len() > 2, "split into {} events", keyframe.len());
        for event in &keyframe {
            let size = serde_json::to_vec(event).unwrap().len();
            assert!(size < 600 * 1024, "event of {size} bytes");
        }
        assert_eq!(pre_children(&keyframe).len(), rows as usize);

        // A full-screen change splits the same way.
        e.feed_str("\x1b[48;5;17m\x1b[2J");
        for event in json(p.diff(&e, 1)) {
            let size = serde_json::to_vec(&event).unwrap().len();
            assert!(size < 600 * 1024, "event of {size} bytes");
        }
    }

    #[test]
    fn cells_are_on_the_grid_and_box_chars_are_drawn() {
        let mut e = Emulator::new(20, 2);
        e.feed_str("\x1b[34m┃\x1b[0m 日 ok");
        let mut p = Projector::new(
            20,
            2,
            Theme::default(),
            Some(CellSize {
                width: 9,
                height: 18,
            }),
        );
        let row = &pre_children(&json(p.keyframe(&e, 0)))[0];
        let spans = row["childNodes"].as_array().unwrap();

        // The bar's span holds its cell width with no glyph. The row paints it.
        let bar_css = spans[0]["attributes"]["style"].as_str().unwrap();
        assert_eq!(bar_css, "width:9px");
        assert!(spans[0]["childNodes"].as_array().unwrap().is_empty());
        let row_css = row["attributes"]["style"].as_str().unwrap();
        assert!(row_css.contains("background-image:"), "{row_css}");

        // One cell span per character, with the wide one marked.
        assert_eq!(run_text(&spans[1]), " 日 ok");
        let cells = spans[1]["childNodes"].as_array().unwrap();
        assert_eq!(cells[1]["attributes"]["class"], "w");
        assert!(cells[0]["attributes"]["class"].is_null());
    }
}
