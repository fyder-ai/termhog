//! Paints a row's cell backgrounds and drawn glyphs as seamless layers.
//!
//! PostHog's player scales the replay with a fractional CSS transform, so cell
//! edges land between device pixels. Wherever two separately painted boxes
//! meet (adjacent spans, repeated tiles, stacked rows), each edge is partially
//! covered and the page background shows through as a hairline seam.
//!
//! So a row is painted by its own `<div>`, cut into horizontal bands at every
//! glyph edge. Each band is one full-row gradient that is opaque everywhere
//! (glyph color where a glyph is, the cell background elsewhere). A gradient
//! is evaluated per pixel, so neighboring cells meet at a hard color stop with
//! nothing in between. Vertically, the same rule applies at every level: each
//! band extends [`OVERLAP_PX`] into the band below, and each row into the row
//! below, and the lower one paints over it. A fractionally placed edge then
//! only ever blends the two colors that really meet there.

use super::emulator::{Run, Style};
use super::glyphs;

/// How far each row's paint extends into the row below.
pub const OVERLAP_PX: u32 = 1;

/// One run's paint: its span of the row, colors, and glyph shape (repeated in
/// every cell) if it's a drawn character.
struct Block {
    x0: u32,
    cells: u32,
    fg: String,
    bg: String,
    /// Glyph rectangles within one cell, left to right, with bottoms reaching
    /// the cell's bottom stretched into the overlap.
    rects: Vec<glyphs::Rect>,
}

/// Solid colors left to right across a band, each running from where the
/// last one ended, as `(color, x0, x1)` in pixels.
#[derive(Default)]
struct Stripes(Vec<(String, u32, u32)>);

impl Stripes {
    /// Continue in `color` up to `x1`, merging with the last stripe when it's
    /// the same color. Does nothing if `x1` isn't past the end.
    fn push(&mut self, color: &str, x1: u32) {
        let x0 = self.0.last().map_or(0, |s| s.2);
        match self.0.last_mut() {
            _ if x1 <= x0 => {}
            Some(last) if last.0 == color => last.2 = x1,
            _ => self.0.push((color.to_string(), x0, x1)),
        }
    }

    /// A left-to-right gradient with hard stops.
    fn gradient(&self) -> String {
        let mut stops: Vec<String> = self
            .0
            .iter()
            .map(|(c, x0, x1)| format!("{c} {x0}px,{c} {x1}px"))
            .collect();
        stops.push(format!(
            "transparent {}px",
            self.0.last().map_or(0, |s| s.2)
        ));
        format!("linear-gradient(to right,{})", stops.join(","))
    }
}

/// The pixel geometry rows are painted on.
#[derive(Clone, Copy)]
pub struct Grid {
    pub cell_w: u32,
    pub cell_h: u32,
    /// A full row's width. Every band spans it, so rows stay opaque.
    pub row_width: u32,
}

/// CSS background properties painting `runs`. `colors` gives a style's
/// foreground and, when it isn't the default, its background. `default_bg`
/// fills everything else. Empty when the row shows only the default
/// background (the stylesheet paints that).
pub fn row_css(
    runs: &[Run],
    grid: Grid,
    default_bg: &str,
    colors: impl Fn(&Style) -> (String, Option<String>),
) -> String {
    let Grid {
        cell_w,
        cell_h,
        row_width,
    } = grid;
    let height = cell_h + OVERLAP_PX;
    let mut blocks = Vec::new();
    let mut custom_bg = false;
    let mut x = 0;
    for run in runs {
        let (fg, bg) = colors(&run.style);
        custom_bg |= bg.is_some();
        let mut rects = run
            .text
            .chars()
            .next()
            .map(|ch| glyphs::cell_rects(ch, cell_w, cell_h))
            .unwrap_or_default();
        for rect in &mut rects {
            if rect.y + rect.h == cell_h {
                rect.h += OVERLAP_PX;
            }
        }
        rects.sort_unstable_by_key(|r| (r.x, r.x + r.w));
        blocks.push(Block {
            x0: x,
            cells: run.width() as u32,
            fg,
            bg: bg.unwrap_or_else(|| default_bg.to_string()),
            rects,
        });
        x += run.width() as u32 * cell_w;
    }
    if !custom_bg && blocks.iter().all(|b| b.rects.is_empty()) {
        return String::new();
    }

    // Cut the row into bands at every glyph edge.
    let mut edges = vec![0, height];
    for block in &blocks {
        edges.extend(block.rects.iter().flat_map(|r| [r.y, r.y + r.h]));
    }
    edges.sort_unstable();
    edges.dedup();

    // One gradient per band, top down, merging neighbors that look the same
    // (like the two arms of a vertical line).
    let mut bands: Vec<(String, u32, u32)> = Vec::new();
    for band in edges.windows(2) {
        let (y0, y1) = (band[0], band[1]);
        // Each cell shows its glyph where a glyph rectangle covers the band,
        // and its background everywhere else.
        let mut stripes = Stripes::default();
        for block in &blocks {
            let covering = block.rects.iter().filter(|r| r.y <= y0 && r.y + r.h >= y1);
            for cell in 0..block.cells {
                let left = block.x0 + cell * cell_w;
                for r in covering.clone() {
                    stripes.push(&block.bg, left + r.x);
                    stripes.push(&block.fg, left + r.x + r.w);
                }
                stripes.push(&block.bg, left + cell_w);
            }
        }
        stripes.push(default_bg, row_width);
        let paint = stripes.gradient();
        match bands.last_mut() {
            Some(last) if last.0 == paint => last.2 = y1,
            _ => bands.push((paint, y0, y1)),
        }
    }

    // Each band overlaps the next by OVERLAP_PX, so the lower band must paint
    // on top. The first layer listed paints on top, hence bottom band first.
    let (mut images, mut sizes, mut positions) = (Vec::new(), Vec::new(), Vec::new());
    for (paint, y0, y1) in bands.into_iter().rev() {
        let h = if y1 < height {
            y1 - y0 + OVERLAP_PX
        } else {
            y1 - y0
        };
        images.push(paint);
        sizes.push(format!("{row_width}px {h}px"));
        positions.push(format!("0 {y0}px"));
    }
    format!(
        "background-image:{};background-size:{};background-position:{};background-repeat:no-repeat;",
        images.join(","),
        sizes.join(","),
        positions.join(","),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of single-width characters.
    fn run(text: &str, style: Style) -> Run {
        Run {
            text: text.to_string(),
            style,
            widths: vec![1; text.chars().count()],
        }
    }

    /// Foreground "F", and background "B" only for styles that set one.
    fn colors(style: &Style) -> (String, Option<String>) {
        ("F".to_string(), style.bg.map(|_| "B".to_string()))
    }

    /// Paint on 8x16 cells, in a row exactly as wide as the runs.
    fn css(runs: &[Run]) -> String {
        let row_width = runs.iter().map(|r| r.width() as u32 * 8).sum();
        let grid = Grid {
            cell_w: 8,
            cell_h: 16,
            row_width,
        };
        row_css(runs, grid, "D", colors)
    }

    /// The (gradient, size, position) of each layer, top-painting first.
    fn layers(css: &str) -> Vec<(String, String, String)> {
        let prop = |name: &str| {
            let rest = css.split(&format!("{name}:")).nth(1).unwrap();
            rest.split(';').next().unwrap().to_string()
        };
        let images: Vec<String> = prop("background-image")
            .split("linear-gradient")
            .skip(1)
            .map(|g| g.trim_end_matches(',').to_string())
            .collect();
        let sizes = prop("background-size");
        let positions = prop("background-position");
        images
            .into_iter()
            .zip(sizes.split(',').map(String::from))
            .zip(positions.split(',').map(String::from))
            .map(|((g, s), p)| (g, s, p))
            .collect()
    }

    #[test]
    fn adjacent_blocks_form_one_unbroken_segment() {
        // "██" then "█" in one color merge into a single stop pair, so no edge
        // exists between the cells for a seam to appear on. The rest of the
        // row is filled too, so it stays opaque.
        let runs = [run("██", Style::default()), run("█", Style::default())];
        let grid = Grid {
            cell_w: 8,
            cell_h: 16,
            row_width: 80,
        };
        let layers = layers(&row_css(&runs, grid, "D", colors));
        assert_eq!(layers.len(), 1, "{layers:?}");
        assert!(layers[0].0.contains("F 0px,F 24px,D 24px,D 80px"));
        // Full-height glyphs run into the overlap with the next row.
        assert_eq!(layers[0].1, "80px 17px");
    }

    #[test]
    fn a_vertical_bar_is_one_unbroken_layer() {
        // Its up and down arms meet mid-cell. They must not become two bands.
        let layers = layers(&css(&[run("┃", Style::default())]));
        assert_eq!(layers.len(), 1, "{layers:?}");
        assert!(layers[0].0.contains("F 3px,F 5px"), "{layers:?}");
    }

    #[test]
    fn bands_are_opaque_and_overlap_downward() {
        let bg = Style {
            bg: Some(crate::render::emulator::Color::Indexed(4)),
            ..Style::default()
        };
        let layers = layers(&css(&[run("ab", bg), run("▀", Style::default())]));
        // Bottom band paints on top, so it's listed first.
        let (bottom, top) = (&layers[0], &layers[1]);
        assert!(
            bottom.0.contains("B 0px,B 16px,D 16px,D 24px"),
            "{bottom:?}"
        );
        assert_eq!(
            (bottom.1.as_str(), bottom.2.as_str()),
            ("24px 9px", "0 8px")
        );
        assert!(top.0.contains("B 0px,B 16px,F 16px,F 24px"), "{top:?}");
        // The top band reaches 1px under the bottom band.
        assert_eq!((top.1.as_str(), top.2.as_str()), ("24px 9px", "0 0px"));
    }
}
