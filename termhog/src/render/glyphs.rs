//! Box-drawing and block characters drawn as CSS shapes instead of font glyphs.
//!
//! Terminals draw these characters to fill the whole cell, so borders and bars
//! join seamlessly across rows and columns. A font glyph is shorter than the
//! line box and its width varies by font, leaving gaps between rows and
//! shifting bars sideways. Describing them as solid rectangles sized to the
//! cell lets the row painter draw them exactly, the way xterm.js does it.

/// How a drawn character fills its cell.
#[derive(Clone, Copy)]
enum Shape {
    /// Lines from the center to the edges (up, right, down, left), each 0 for
    /// none, 1 for light or 2 for heavy (twice as thick).
    Lines([u32; 4]),
    /// A solid block covering this fraction (in eighths) of the cell, from the
    /// given edge.
    Block { from: Edge, eighths: u32 },
}

#[derive(Clone, Copy)]
enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

fn shape(ch: char) -> Option<Shape> {
    const N: u32 = 0;
    const L: u32 = 1;
    const H: u32 = 2;
    let lines = |w| Some(Shape::Lines(w));
    match ch {
        '─' => lines([N, L, N, L]),
        '━' => lines([N, H, N, H]),
        '│' => lines([L, N, L, N]),
        '┃' => lines([H, N, H, N]),
        '┌' | '╭' => lines([N, L, L, N]),
        '┏' => lines([N, H, H, N]),
        '┐' | '╮' => lines([N, N, L, L]),
        '┓' => lines([N, N, H, H]),
        '└' | '╰' => lines([L, L, N, N]),
        '┗' => lines([H, H, N, N]),
        '┘' | '╯' => lines([L, N, N, L]),
        '┛' => lines([H, N, N, H]),
        '├' => lines([L, L, L, N]),
        '┣' => lines([H, H, H, N]),
        '┤' => lines([L, N, L, L]),
        '┫' => lines([H, N, H, H]),
        '┬' => lines([N, L, L, L]),
        '┳' => lines([N, H, H, H]),
        '┴' => lines([L, L, N, L]),
        '┻' => lines([H, H, N, H]),
        '┼' => lines([L, L, L, L]),
        '╋' => lines([H, H, H, H]),
        '╴' => lines([N, N, N, L]),
        '╵' => lines([L, N, N, N]),
        '╶' => lines([N, L, N, N]),
        '╷' => lines([N, N, L, N]),
        '╸' => lines([N, N, N, H]),
        '╹' => lines([H, N, N, N]),
        '╺' => lines([N, H, N, N]),
        '╻' => lines([N, N, H, N]),
        '▀' => block(Edge::Top, 4),
        '▔' => block(Edge::Top, 1),
        // Lower eighths: ▁ ▂ ▃ ▄ ▅ ▆ ▇ █
        '\u{2581}'..='\u{2588}' => block(Edge::Bottom, ch as u32 - 0x2580),
        // Left eighths, widest first: ▉ ▊ ▋ ▌ ▍ ▎ ▏
        '\u{2589}'..='\u{258F}' => block(Edge::Left, 0x2590 - ch as u32),
        '▐' => block(Edge::Right, 4),
        '▕' => block(Edge::Right, 1),
        _ => None,
    }
}

fn block(from: Edge, eighths: u32) -> Option<Shape> {
    Some(Shape::Block { from, eighths })
}

/// Whether `ch` is drawn with CSS rather than a font glyph.
pub fn is_drawn(ch: char) -> bool {
    shape(ch).is_some()
}

/// One solid rectangle within a cell, in pixels from the cell's top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// The solid rectangles that draw `c` in a `cw` x `ch` cell. Empty if `c`
/// isn't a drawn character.
pub fn cell_rects(c: char, cw: u32, ch: u32) -> Vec<Rect> {
    match shape(c) {
        None => Vec::new(),
        Some(Shape::Block { from, eighths }) => {
            // At least a pixel, so a thin block still shows in a small cell.
            let part = |size: u32| (size * eighths / 8).max(1);
            let (x, y, w, h) = match from {
                Edge::Top => (0, 0, cw, part(ch)),
                Edge::Bottom => (0, ch - part(ch), cw, part(ch)),
                Edge::Left => (0, 0, part(cw), ch),
                Edge::Right => (cw - part(cw), 0, part(cw), ch),
            };
            vec![Rect { x, y, w, h }]
        }
        Some(Shape::Lines(weights)) => {
            // Heavy lines are twice as thick, and must still fit the cell.
            let light = ((cw as f32 / 8.0).round() as u32).clamp(1, (cw.min(ch) / 2).max(1));
            let [up, right, down, left] = weights.map(|w| w * light);
            // Arms meet in a center square as wide as the thickest line, so
            // corners and junctions join without notches.
            let (vt, ht) = (up.max(down), left.max(right));
            let (cx, cy) = (cw / 2, ch / 2);
            let (down_y, right_x) = (cy - ht / 2, cx - vt / 2);
            #[rustfmt::skip]
            let arms = [
                (up, Rect { x: cx - up / 2, y: 0, w: up, h: cy + ht.div_ceil(2) }),
                (down, Rect { x: cx - down / 2, y: down_y, w: down, h: ch - down_y }),
                (left, Rect { x: 0, y: cy - left / 2, w: cx + vt.div_ceil(2), h: left }),
                (right, Rect { x: right_x, y: cy - right / 2, w: cw - right_x, h: right }),
            ];
            arms.into_iter()
                .filter(|&(thickness, _)| thickness > 0)
                .map(|(_, rect)| rect)
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_bar_spans_full_cell_height() {
        let rects = cell_rects('┃', 8, 16);
        let top = rects.iter().map(|r| r.y).min().unwrap();
        let bottom = rects.iter().map(|r| r.y + r.h).max().unwrap();
        assert_eq!((top, bottom), (0, 16));
        assert!(rects.iter().all(|r| r.w == 2), "heavy is twice light");
    }

    #[test]
    fn blocks_cover_their_fraction() {
        let r = cell_rects('█', 8, 16)[0];
        assert_eq!((r.x, r.y, r.w, r.h), (0, 0, 8, 16));
        let r = cell_rects('▄', 8, 16)[0];
        assert_eq!((r.y, r.h), (8, 8));
        let r = cell_rects('▏', 8, 16)[0];
        assert_eq!((r.x, r.w), (0, 1));
        let r = cell_rects('▐', 8, 16)[0];
        assert_eq!((r.x, r.w), (4, 4));
    }
}
