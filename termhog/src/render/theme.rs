//! The colors a screen is drawn in: the program's own (set with escape
//! sequences) over the host terminal's (as far as it reported them) over
//! xterm's defaults.

use serde::{Deserialize, Serialize};

use crate::tty::osc::{Colors, css_hex};

const DEFAULT_FOREGROUND: &str = "#d0d0d0";
const DEFAULT_BACKGROUND: &str = "#000000";
const DEFAULT_PALETTE: [&str; 16] = [
    "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
    "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
];

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Theme {
    /// The host terminal's colors.
    pub host: Colors,
    /// Colors the program set, which win over the host's until it resets
    /// them.
    pub program: Colors,
}

impl Theme {
    pub fn foreground(&self) -> String {
        let color = self.program.foreground.as_deref();
        let color = color.or(self.host.foreground.as_deref());
        color.unwrap_or(DEFAULT_FOREGROUND).to_string()
    }

    pub fn background(&self) -> String {
        let color = self.program.background.as_deref();
        let color = color.or(self.host.background.as_deref());
        color.unwrap_or(DEFAULT_BACKGROUND).to_string()
    }

    /// A palette entry: 16 named colors, a 6x6x6 color cube, then 24 grays.
    pub fn indexed(&self, i: u8) -> String {
        if let Some(color) = self.program.palette(i).or(self.host.palette(i)) {
            return color.to_string();
        }
        match i {
            0..=15 => DEFAULT_PALETTE[i as usize].to_string(),
            16..=231 => {
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
                css_hex([level(i / 36), level(i / 6 % 6), level(i % 6)])
            }
            232..=255 => {
                let level = (i - 232) * 10 + 8;
                css_hex([level; 3])
            }
        }
    }
}
