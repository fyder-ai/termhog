//! Projects emulator screen state into rrweb events.
//!
//! The player reconstructs a DOM, so we render the terminal as a fixed
//! `rows`×`cols` grid: a `<pre>` containing one `<div>` per row, each holding
//! style-run `<span>`s. A FullSnapshot builds the whole tree; incremental
//! Mutations replace the children of the rows that changed since the last emit.
//! The cursor is drawn by overlaying reverse-video on its cell, so cursor moves
//! surface as ordinary row changes.

use serde_json::{Value, json};

use crate::emulator::{Color, Cursor, Emulator, Run, Style};
use crate::terminal::{CellSize, ThemeColors};

// rrweb EventType
const FULL_SNAPSHOT: i64 = 2;
const INCREMENTAL_SNAPSHOT: i64 = 3;
const META: i64 = 4;
// rrweb IncrementalSource
const MUTATION: i64 = 0;
const MOUSE_INTERACTION: i64 = 2;
// rrweb MouseInteractions
const MOUSE_UP: i64 = 0;
const TOUCH_START: i64 = 7;
// rrweb PointerTypes: Mouse=0, Pen=1, Touch=2
const POINTER_TOUCH: i64 = 2;
// rrweb-snapshot NodeType
const NODE_DOCUMENT: i64 = 0;
const NODE_ELEMENT: i64 = 2;
const NODE_TEXT: i64 = 3;

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

// Fallback cell metrics, used when the terminal doesn't report its pixel cell
// size. Chosen so a 13px monospace advance (~0.6em) is close to the cell width,
// keeping the player viewport aligned with the rendered content.
const DEFAULT_CELL_W_PX: u32 = 8;
const DEFAULT_CELL_H_PX: u32 = 16;
/// Monospace advance is ~0.6em, so pick a font size whose advance is close to the
/// real cell width — but never taller than the cell, to avoid vertical overflow.
fn font_size_for(cell_w: u32, cell_h: u32) -> u32 {
    let by_width = ((cell_w as f64) / 0.6).round() as u32;
    // Cap below the cell height, but never below the 6px floor (keeps clamp's
    // min <= max invariant when the cell is tiny).
    let max = cell_h.saturating_sub(1).max(6);
    by_width.clamp(6, max)
}
const FONT_STACK: &str = "'DejaVu Sans Mono','Menlo','Consolas',monospace";
const HREF: &str = "ph-capture://session";

/// Resolved colors used to turn emulator colors into CSS.
struct Theme {
    fg: String,
    bg: String,
    palette: [String; 16],
}

impl Theme {
    fn resolve(source: Option<ThemeColors>) -> Self {
        match source {
            Some(t) if t.palette.len() == 16 => {
                let palette: [String; 16] =
                    std::array::from_fn(|i| t.palette[i].clone());
                Theme {
                    fg: t.fg,
                    bg: t.bg,
                    palette,
                }
            }
            _ => Theme {
                fg: DEFAULT_FG.to_string(),
                bg: DEFAULT_BG.to_string(),
                palette: DEFAULT_PALETTE.map(|s| s.to_string()),
            },
        }
    }
}

const DEFAULT_FG: &str = "#d0d0d0";
const DEFAULT_BG: &str = "#000000";
const DEFAULT_PALETTE: [&str; 16] = [
    "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
    "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
];

/// Turns emulator state into rrweb events, tracking per-row DOM so it can emit
/// minimal mutations between snapshots.
pub struct Projector {
    cols: u16,
    rows: u16,
    theme: Theme,
    /// Character cell size in pixels, from the terminal when known (see
    /// [`CellSize`]) or a sane default. Sizes the viewport and the CSS grid.
    cell_w: u32,
    cell_h: u32,
    font_size: u32,
    next_id: i64,
    /// The cursor-inclusive runs last emitted for each row (index = row).
    prev_rows: Vec<Vec<Run>>,
    /// The child node ids currently under each row `<div>`, for removal.
    row_children: Vec<Vec<i64>>,
}

impl Projector {
    pub fn new(cols: u16, rows: u16, theme: Option<ThemeColors>, cell: Option<CellSize>) -> Self {
        let (cell_w, cell_h) = match cell {
            Some(c) => (c.width as u32, c.height as u32),
            None => (DEFAULT_CELL_W_PX, DEFAULT_CELL_H_PX),
        };
        Projector {
            cols,
            rows,
            theme: Theme::resolve(theme),
            cell_w,
            cell_h,
            font_size: font_size_for(cell_w, cell_h),
            next_id: ID_DYNAMIC_BASE,
            prev_rows: vec![Vec::new(); rows as usize],
            row_children: vec![Vec::new(); rows as usize],
        }
    }

    /// Update geometry after a resize. The caller should emit a fresh Meta +
    /// FullSnapshot afterwards, since the grid changed.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.prev_rows = vec![Vec::new(); rows as usize];
        self.row_children = vec![Vec::new(); rows as usize];
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Meta event: sizes the player viewport in pixels.
    pub fn meta(&self, timestamp_ms: u64) -> Value {
        json!({
            "type": META,
            "data": {
                "href": HREF,
                "width": self.cols as u32 * self.cell_w,
                "height": self.rows as u32 * self.cell_h,
            },
            "timestamp": timestamp_ms,
        })
    }

    /// Hide the replayer's mouse cursor. The player always draws a passive mouse
    /// overlay and a terminal has none. rrweb adds a permanent `touch-device`
    /// class if any event is a `MouseInteraction` of type `TouchStart` (its init
    /// scan checks only source+type), and both rrweb's and PostHog's CSS blank
    /// the passive cursor for that class.
    ///
    /// We give it `id: -1` (no target): the replayer bails at the top of the
    /// MouseInteraction handler before positioning the mouse, so applying it is a
    /// pure no-op — no mouse move, no red tail trail, no tap ring — and it's safe
    /// on seek. The init scan still flips the class.
    pub fn hide_mouse(&self, timestamp_ms: u64) -> Value {
        json!({
            "type": INCREMENTAL_SNAPSHOT,
            "data": {
                "source": MOUSE_INTERACTION,
                "type": TOUCH_START,
                "id": -1,
                "pointerType": POINTER_TOUCH,
            },
            "timestamp": timestamp_ms,
        })
    }

    /// A no-op event that marks user activity so the player counts active time
    /// and doesn't treat typing as skippable. rrweb counts sources in
    /// `(Mutation, Input]` as active; a MouseInteraction with `id: -1` counts yet
    /// applies as a pure no-op (the handler bails on `id === -1`). Carries no
    /// keystroke content.
    pub fn activity(&self, timestamp_ms: u64) -> Value {
        json!({
            "type": INCREMENTAL_SNAPSHOT,
            "data": { "source": MOUSE_INTERACTION, "type": MOUSE_UP, "id": -1 },
            "timestamp": timestamp_ms,
        })
    }

    /// FullSnapshot: rebuild the whole document from the current screen. Resets
    /// the tracked per-row state to match what we just emitted.
    pub fn full_snapshot(&mut self, emu: &dyn Emulator, timestamp_ms: u64) -> Value {
        let cursor = emu.cursor();
        let mut row_divs = Vec::with_capacity(self.rows as usize);
        for row in 0..self.rows {
            let runs = self.render_row(emu, row, cursor);
            let children = self.alloc_row_children(&runs);
            self.row_children[row as usize] = child_ids(&children);
            // FullSnapshot builds the whole subtree, so nest text inside spans.
            row_divs.push(div_node(row_div_id(row), nested_row_nodes(&children)));
            self.prev_rows[row as usize] = runs;
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
                                "textContent": self.stylesheet(),
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

        json!({
            "type": FULL_SNAPSHOT,
            "data": { "node": node, "initialOffset": {"top": 0, "left": 0} },
            "timestamp": timestamp_ms,
        })
    }

    /// Incremental mutation for whatever rows changed since the last emit.
    /// Returns `None` if nothing changed.
    pub fn diff(&mut self, emu: &dyn Emulator, timestamp_ms: u64) -> Option<Value> {
        let cursor = emu.cursor();
        let mut removes = Vec::new();
        let mut adds = Vec::new();

        for row in 0..self.rows {
            let runs = self.render_row(emu, row, cursor);
            if runs == self.prev_rows[row as usize] {
                continue;
            }
            let parent = row_div_id(row);
            for old in self.row_children[row as usize].drain(..) {
                removes.push(json!({ "parentId": parent, "id": old }));
            }
            let children = self.alloc_row_children(&runs);
            // The replayer builds each added node with skipChild:true, so a
            // subtree must arrive as separate flat adds, parent before child.
            for child in &children {
                match child {
                    RowChild::Nbsp { id } => {
                        adds.push(add_node(parent, text_node(*id, NBSP)));
                    }
                    RowChild::Span {
                        span_id,
                        css,
                        text_id,
                        text,
                    } => {
                        adds.push(add_node(parent, span_node(*span_id, css, vec![])));
                        adds.push(add_node(*span_id, text_node(*text_id, text)));
                    }
                }
            }
            self.row_children[row as usize] = child_ids(&children);
            self.prev_rows[row as usize] = runs;
        }

        if removes.is_empty() && adds.is_empty() {
            return None;
        }

        Some(json!({
            "type": INCREMENTAL_SNAPSHOT,
            "data": {
                "source": MUTATION,
                "texts": [],
                "attributes": [],
                "removes": removes,
                "adds": adds,
            },
            "timestamp": timestamp_ms,
        }))
    }

    /// Render one row's runs including the cursor overlay. Reverse-videos the
    /// cursor cell when the cursor is visible and on this row.
    fn render_row(&self, emu: &dyn Emulator, row: u16, cursor: Cursor) -> Vec<Run> {
        let runs = emu.row_runs(row);
        if !(cursor.visible && cursor.row == row) {
            return runs;
        }

        // Expand to cells, overlay the cursor, regroup into runs.
        let mut cells: Vec<(char, Style)> = Vec::new();
        for run in &runs {
            for ch in run.text.chars() {
                cells.push((ch, run.style.clone()));
            }
        }
        let col = cursor.col as usize;
        while cells.len() <= col {
            cells.push((' ', Style::default()));
        }
        cells[col].1.inverse = !cells[col].1.inverse;

        let mut out: Vec<Run> = Vec::new();
        for (ch, style) in cells {
            match out.last_mut() {
                Some(last) if last.style == style => last.text.push(ch),
                _ => out.push(Run {
                    text: ch.to_string(),
                    style,
                }),
            }
        }
        out
    }

    /// Allocate ids for a row's children. Empty rows get a single non-breaking
    /// space so the `<div>` keeps its height. Used by both the FullSnapshot
    /// (nested build) and diff (flat adds) so the two stay consistent.
    fn alloc_row_children(&mut self, runs: &[Run]) -> Vec<RowChild> {
        if runs.is_empty() {
            return vec![RowChild::Nbsp {
                id: self.alloc_id(),
            }];
        }
        runs.iter()
            .map(|run| RowChild::Span {
                span_id: self.alloc_id(),
                css: self.style_css(&run.style),
                text_id: self.alloc_id(),
                text: run.text.clone(),
            })
            .collect()
    }

    fn stylesheet(&self) -> String {
        // Run spans are `inline-block` at the full cell height so a run's
        // background-color fills the whole line box. Plain inline spans only
        // paint the font's content area (~font-size tall), leaving the line-box
        // leading unpainted — that gap shows through between rows that carry a
        // background (status bars, selections, the reverse-video cursor).
        format!(
            "body{{margin:0;background:{bg}}}\
             pre{{margin:0;font-family:{font};font-size:{fs}px;line-height:{ch}px;\
             white-space:pre;color:{fg}}}\
             pre span{{display:inline-block;height:{ch}px;line-height:{ch}px;\
             vertical-align:top}}\
             div{{height:{ch}px}}",
            bg = self.theme.bg,
            fg = self.theme.fg,
            font = FONT_STACK,
            fs = self.font_size,
            ch = self.cell_h,
        )
    }

    /// Inline CSS for a run's style. Empty for the default style (the `<pre>`
    /// already sets the base fg/bg).
    fn style_css(&self, style: &Style) -> String {
        if *style == Style::default() {
            return String::new();
        }
        let mut fg = self.resolve(style.fg).unwrap_or_else(|| self.theme.fg.clone());
        let mut bg = self.resolve(style.bg).unwrap_or_else(|| self.theme.bg.clone());
        let has_bg = style.bg.is_some() || style.inverse;
        if style.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }

        let mut css = format!("color:{fg};");
        if has_bg {
            css.push_str(&format!("background-color:{bg};"));
        }
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

    /// Resolve an emulator color to a CSS hex/rgb string.
    fn resolve(&self, color: Option<Color>) -> Option<String> {
        match color? {
            Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
            Color::Indexed(i) => Some(self.resolve_indexed(i)),
        }
    }

    fn resolve_indexed(&self, i: u8) -> String {
        match i {
            0..=15 => self.theme.palette[i as usize].clone(),
            16..=231 => {
                let i = i - 16;
                let comp = |v: u8| -> u8 {
                    if v == 0 {
                        0
                    } else {
                        v * 40 + 55
                    }
                };
                let r = comp(i / 36);
                let g = comp((i / 6) % 6);
                let b = comp(i % 6);
                format!("#{r:02x}{g:02x}{b:02x}")
            }
            232..=255 => {
                let level = (i - 232) * 10 + 8;
                format!("#{level:02x}{level:02x}{level:02x}")
            }
        }
    }
}

const NBSP: &str = "\u{a0}";

/// A row `<div>`'s children with allocated ids, rendered either nested (for a
/// FullSnapshot) or flat (for a mutation's adds).
enum RowChild {
    Nbsp { id: i64 },
    Span {
        span_id: i64,
        css: String,
        text_id: i64,
        text: String,
    },
}

/// The top-level child ids of a row, for removal in a later mutation. Removing a
/// span removes its text child with it, so only the span id is tracked.
fn child_ids(children: &[RowChild]) -> Vec<i64> {
    children
        .iter()
        .map(|c| match c {
            RowChild::Nbsp { id } => *id,
            RowChild::Span { span_id, .. } => *span_id,
        })
        .collect()
}

/// Build a row's children as a nested subtree (text nested inside each span).
fn nested_row_nodes(children: &[RowChild]) -> Vec<Value> {
    children
        .iter()
        .map(|c| match c {
            RowChild::Nbsp { id } => text_node(*id, NBSP),
            RowChild::Span {
                span_id,
                css,
                text_id,
                text,
            } => span_node(*span_id, css, vec![text_node(*text_id, text)]),
        })
        .collect()
}

/// A single `addedNodeMutation` appending `node` as the last child of `parent`.
fn add_node(parent_id: i64, node: Value) -> Value {
    json!({ "parentId": parent_id, "nextId": Value::Null, "node": node })
}

fn row_div_id(row: u16) -> i64 {
    ID_ROW_BASE + row as i64
}

fn text_node(id: i64, text: &str) -> Value {
    json!({ "type": NODE_TEXT, "id": id, "textContent": text })
}

fn span_node(id: i64, style_css: &str, children: Vec<Value>) -> Value {
    let attributes = if style_css.is_empty() {
        json!({})
    } else {
        json!({ "style": style_css })
    };
    json!({
        "type": NODE_ELEMENT,
        "tagName": "span",
        "attributes": attributes,
        "id": id,
        "childNodes": children,
    })
}

fn div_node(id: i64, children: Vec<Value>) -> Value {
    json!({
        "type": NODE_ELEMENT,
        "tagName": "div",
        "attributes": {},
        "id": id,
        "childNodes": children,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::AvtEmulator;

    fn pre_children(full: &Value) -> Vec<Value> {
        // document -> html -> [head, body] -> pre -> childNodes
        let html = &full["data"]["node"]["childNodes"][0];
        let body = &html["childNodes"][1];
        let pre = &body["childNodes"][0];
        pre["childNodes"].as_array().unwrap().clone()
    }

    #[test]
    fn full_snapshot_has_one_div_per_row() {
        let mut e = AvtEmulator::new(80, 10);
        e.feed_str("hello");
        let mut p = Projector::new(80, 10, None, None);
        let full = p.full_snapshot(&e, 1000);
        assert_eq!(full["type"], FULL_SNAPSHOT);
        let rows = pre_children(&full);
        assert_eq!(rows.len(), 10);
        // row 0 first span's text node is "hello"
        let span = &rows[0]["childNodes"][0];
        assert_eq!(span["tagName"], "span");
        assert_eq!(span["childNodes"][0]["textContent"], "hello");
    }

    #[test]
    fn diff_none_when_unchanged() {
        let mut e = AvtEmulator::new(80, 10);
        e.feed_str("hi");
        let mut p = Projector::new(80, 10, None, None);
        let _ = p.full_snapshot(&e, 0);
        assert!(p.diff(&e, 1).is_none());
    }

    #[test]
    fn diff_emits_mutation_on_change() {
        let mut e = AvtEmulator::new(80, 10);
        e.feed_str("hi");
        let mut p = Projector::new(80, 10, None, None);
        let _ = p.full_snapshot(&e, 0);
        e.feed_str("!"); // row 0 becomes "hi!"
        let m = p.diff(&e, 2).expect("mutation");
        assert_eq!(m["type"], INCREMENTAL_SNAPSHOT);
        assert_eq!(m["data"]["source"], MUTATION);
        assert!(!m["data"]["removes"].as_array().unwrap().is_empty());
        let adds = m["data"]["adds"].as_array().unwrap();

        // Adds must be flat: every added node has no nested childNodes (the
        // replayer builds with skipChild:true). Text arrives as its own add
        // whose parent is the span added just before it.
        for a in adds {
            let kids = a["node"]["childNodes"].as_array();
            assert!(kids.map(|k| k.is_empty()).unwrap_or(true), "add is not flat: {a}");
        }
        // A span add is followed by its text-node add carrying "hi!".
        let text_add = adds
            .iter()
            .find(|a| a["node"]["type"] == NODE_TEXT && a["node"]["textContent"] == "hi!")
            .expect("text add with new content");
        // its parent is a span added in this same mutation
        let parent = text_add["parentId"].as_i64().unwrap();
        assert!(
            adds.iter().any(|a| a["node"]["id"].as_i64() == Some(parent)
                && a["node"]["tagName"] == "span"),
            "text add's parent span not present"
        );
    }

    #[test]
    fn cursor_overlay_marks_a_cell() {
        let mut e = AvtEmulator::new(80, 3);
        e.feed_str("ab"); // cursor now at col 2, row 0
        let mut p = Projector::new(80, 3, None, None);
        let full = p.full_snapshot(&e, 0);
        let rows = pre_children(&full);
        // row 0 should have the "ab" run plus a reverse-video cursor span at col 2
        let spans = rows[0]["childNodes"].as_array().unwrap();
        let has_bg = spans.iter().any(|s| {
            s["attributes"]["style"]
                .as_str()
                .map(|css| css.contains("background-color"))
                .unwrap_or(false)
        });
        assert!(has_bg, "expected a reverse-video cursor span with a background");
    }

    #[test]
    fn truecolor_run_css() {
        let mut e = AvtEmulator::new(80, 3);
        e.feed_str("\x1b[38;2;10;20;30mX");
        let mut p = Projector::new(80, 3, None, None);
        let full = p.full_snapshot(&e, 0);
        let rows = pre_children(&full);
        let css = rows[0]["childNodes"][0]["attributes"]["style"]
            .as_str()
            .unwrap();
        assert!(css.contains("color:#0a141e"), "got: {css}");
    }

    #[test]
    fn queried_cell_size_sizes_the_viewport() {
        let cell = Some(CellSize {
            width: 9,
            height: 18,
        });
        let meta = Projector::new(80, 24, None, cell).meta(0);
        assert_eq!(meta["data"]["width"], 80 * 9);
        assert_eq!(meta["data"]["height"], 24 * 18);
    }

    #[test]
    fn default_cell_size_matches_legacy_metrics() {
        let meta = Projector::new(80, 24, None, None).meta(0);
        assert_eq!(meta["data"]["width"], 80 * 8);
        assert_eq!(meta["data"]["height"], 24 * 16);
    }

    #[test]
    fn stylesheet_fills_line_box_to_avoid_gaps() {
        let mut e = AvtEmulator::new(80, 3);
        e.feed_str("x");
        let mut p = Projector::new(
            80,
            3,
            None,
            Some(CellSize {
                width: 9,
                height: 18,
            }),
        );
        let full = p.full_snapshot(&e, 0);
        // document -> html -> head -> style -> text
        let html = &full["data"]["node"]["childNodes"][0];
        let style_text = html["childNodes"][0]["childNodes"][0]["childNodes"][0]["textContent"]
            .as_str()
            .unwrap();
        assert!(
            style_text.contains("pre span{display:inline-block;height:18px;line-height:18px"),
            "stylesheet missing inline-block cell rule: {style_text}"
        );
    }
}
