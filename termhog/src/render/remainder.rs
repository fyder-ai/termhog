//! Saving a render stopped part-way, so another process can finish it.
//!
//! It's saved as [`Saved`] lines: first the screen's state (with the
//! emulator's own dump, the escape sequences that redraw it), then each
//! message not yet rendered, with the time it happened. Output is saved as
//! text, decoded exactly as rendering would decode it. Resuming redraws the
//! screen, sends it as a keyframe, then renders the rest exactly as the
//! render thread would have.

use std::io::{self, Write};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};

use super::backlog::Backlog;
use super::emulator::Emulator;
use super::projection::Projector;
use super::screen::{Screen, Sink};
use super::theme::Theme;
use super::{Msg, Update};
use crate::tty::probe::CellSize;
use crate::util::write_json_line;

/// A render stopped part-way: the screen so far, and what's left to render.
pub struct Remainder {
    pub(super) screen: Screen,
    pub(super) rx: Receiver<Msg>,
    pub(super) backlog: Backlog,
}

/// One line of a saved render.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Saved {
    /// First, the screen as rendering left it.
    Screen(State),
    /// Output not yet rendered.
    Output { at: u64, text: String },
    #[serde(untagged)]
    Update(Update),
}

/// A screen's state, enough to redraw it.
#[derive(Serialize, Deserialize)]
pub struct State {
    cols: u16,
    rows: u16,
    /// When its latest change happened.
    now: u64,
    dump: String,
    theme: Theme,
    cell: CellSize,
}

impl Remainder {
    /// Write the screen's state, then every message still to render.
    pub fn write_lines(self, out: &mut impl Write) -> io::Result<()> {
        let Remainder {
            mut screen,
            rx,
            backlog,
        } = self;
        let (cols, rows) = screen.proj.size();
        let state = State {
            cols,
            rows,
            now: screen.now,
            dump: screen.emu.dump(),
            theme: screen.proj.theme.clone(),
            cell: screen.proj.cell,
        };
        write_json_line(out, &Saved::Screen(state))?;

        // Output continues from the screen's undecoded tail, if any.
        let mut decoder = std::mem::take(&mut screen.decoder);
        let mut last_output = screen.now;
        // Stop marks the end. A reader still mirroring a background process
        // may add more after it, which isn't part of the recording.
        for msg in rx.try_iter() {
            let line = match msg {
                Msg::Output { chunk, at } => {
                    let text = decoder.push(&backlog.take(chunk)?);
                    last_output = at;
                    if text.is_empty() {
                        continue;
                    }
                    Saved::Output { at, text }
                }
                Msg::Update(update) => Saved::Update(update),
                Msg::Stop => break,
            };
            write_json_line(out, &line)?;
        }
        // An incomplete character at the very end, shown as a replacement
        // character, as rendering it would.
        let text = decoder.flush();
        if !text.is_empty() {
            let at = last_output;
            write_json_line(out, &Saved::Output { at, text })?;
        }
        Ok(())
    }
}

/// Finishes a render saved by [`Remainder::write_lines`], one line at a time,
/// sending its events to a sink.
pub struct Resumer {
    /// The sink, until the screen line arrives and takes it.
    sink: Option<Sink>,
    screen: Option<Screen>,
}

impl Resumer {
    pub fn new(sink: Sink) -> Resumer {
        Resumer {
            sink: Some(sink),
            screen: None,
        }
    }

    /// Render one saved line.
    pub fn apply(&mut self, line: Saved) {
        match (line, &mut self.screen) {
            (Saved::Screen(state), None) => {
                self.screen = self.sink.take().map(|sink| state.restore(sink));
            }
            (Saved::Output { at, text }, Some(screen)) => screen.output(text.as_bytes(), at),
            (Saved::Update(update), Some(screen)) => screen.update(update),
            // Lines before the screen, or a second screen: damaged.
            _ => {}
        }
    }

    /// Send the final frame. Returns whether there was a render to finish.
    pub fn finish(self) -> bool {
        self.screen.map(Screen::finish).is_some()
    }
}

impl State {
    /// The screen this state was saved from, sent as a keyframe.
    fn restore(self, sink: Sink) -> Screen {
        let proj = Projector::new(self.cols, self.rows, self.theme, Some(self.cell));
        let emu = Emulator::restore(self.cols, self.rows, &self.dump);
        Screen::resumed(emu, proj, self.now, sink)
    }
}
