//! The emulated screen, and the frames of it that have been sent.

use super::emulator::Emulator;
use super::projection::{self, Projector};
use super::rrweb::Event;
use super::theme::Theme;
use super::utf8::Utf8Decoder;
use super::{FRAME_MS, KEYFRAME_MS, Update};
use crate::tty::osc::Scanner;
use crate::tty::probe::TerminalInfo;
use crate::util::epoch_ms;

/// Where a screen's rrweb events go.
pub type Sink = Box<dyn FnMut(Event) + Send>;

/// Changes are grouped into frames of at most [`FRAME_MS`] of the output's
/// own time, each stamped when its last change happened. So when output
/// arrives faster than it renders, the replay still shows each frame at the
/// moment it appeared, not when the render thread got to it.
pub struct Screen {
    pub(super) emu: Emulator,
    pub(super) proj: Projector,
    pub(super) decoder: Utf8Decoder,
    /// Picks out colors the program sets.
    colors: Scanner,
    sink: Sink,
    /// When the latest change happened.
    pub(super) now: u64,
    /// When the frame being built started, if anything changed since the last.
    frame_start: Option<u64>,
    /// The next frame must be a keyframe (the geometry changed, or output
    /// was lost).
    keyframe_pending: bool,
    last_keyframe: u64,
    /// A diff went out since the last keyframe, so a new one isn't redundant.
    changed: bool,
    /// When the user first typed since the last frame.
    input_at: Option<u64>,
}

impl Screen {
    /// A blank screen in the host terminal's colors, as far as they're known,
    /// sent as the first keyframe.
    pub fn new(cols: u16, rows: u16, info: TerminalInfo, sink: Sink) -> Screen {
        let theme = Theme {
            host: info.colors,
            ..Theme::default()
        };
        let proj = Projector::new(cols, rows, theme, info.cell);
        let mut screen = Screen::resumed(Emulator::new(cols, rows), proj, epoch_ms(), sink);
        (screen.sink)(projection::hide_mouse(screen.now));
        screen
    }

    /// A screen continuing from `emu`'s state as of `now`, sent as a keyframe.
    pub(super) fn resumed(emu: Emulator, proj: Projector, now: u64, sink: Sink) -> Screen {
        let mut screen = Screen {
            emu,
            proj,
            decoder: Utf8Decoder::default(),
            colors: Scanner::default(),
            sink,
            now,
            frame_start: None,
            keyframe_pending: false,
            last_keyframe: now,
            changed: false,
            input_at: None,
        };
        screen.keyframe();
        screen
    }

    pub fn output(&mut self, bytes: &[u8], at: u64) {
        let text = self.decoder.push(bytes);
        if text.is_empty() {
            return;
        }
        self.change(at);
        // A color the program sets recolors everything already drawn in it,
        // as it would on a terminal.
        for change in self.colors.scan(text.as_bytes()) {
            self.proj.theme.program.apply(&change);
            self.proj.repaint();
        }
        self.emu.feed_str(&text);
    }

    pub fn update(&mut self, update: Update) {
        match update {
            Update::Resize { at, cols, rows } => self.resize(cols, rows, at),
            Update::Input { at } => self.input(at),
            Update::Terminal { at, info } => self.terminal(info, at),
            Update::Unknown => {}
        }
    }

    fn resize(&mut self, cols: u16, rows: u16, at: u64) {
        // A resize to the same size (a SIGWINCH without a change) shows nothing.
        if (cols, rows) == self.proj.size() {
            return;
        }
        self.change(at);
        self.emu.resize(cols, rows);
        self.proj.resize(cols, rows);
        // The grid changed, so never diff across it.
        self.keyframe_pending = true;
    }

    /// Learn more of the host terminal's colors or its cell size.
    fn terminal(&mut self, info: TerminalInfo, at: u64) {
        let mut host = self.proj.theme.host.clone();
        host.merge(&info.colors);
        let cell = info.cell.unwrap_or(self.proj.cell);
        if host == self.proj.theme.host && cell == self.proj.cell {
            return;
        }
        self.change(at);
        self.proj.theme.host = host;
        self.proj.repaint();
        // A new cell size changes the viewport, which only a keyframe can.
        if cell != self.proj.cell {
            self.proj.cell = cell;
            self.keyframe_pending = true;
        }
    }

    /// Typing is marked once per frame, like other changes.
    fn input(&mut self, at: u64) {
        if self
            .input_at
            .is_some_and(|first| at.saturating_sub(first) >= FRAME_MS)
        {
            self.mark_input();
        }
        self.input_at.get_or_insert(at);
    }

    /// Resend the whole screen with the next frame.
    pub fn force_keyframe(&mut self) {
        self.change(self.now);
        self.keyframe_pending = true;
    }

    /// Send what's pending, on the frame clock.
    pub fn tick(&mut self) {
        self.mark_input();
        if self.frame_start.is_some() {
            self.frame();
        } else if self.changed && epoch_ms().saturating_sub(self.last_keyframe) >= KEYFRAME_MS {
            // Caught up and idle: a fresh keyframe of the current screen, so
            // seeking needn't replay every diff since the last one.
            self.now = epoch_ms();
            self.keyframe();
        }
    }

    /// Mark the typing since the last frame as activity.
    fn mark_input(&mut self) {
        if let Some(at) = self.input_at.take() {
            (self.sink)(projection::activity(at));
        }
    }

    /// Render whatever's left and send the final frame.
    pub fn finish(mut self) {
        let leftover = self.decoder.flush();
        if !leftover.is_empty() {
            self.change(self.now);
            self.emu.feed_str(&leftover);
        }
        self.mark_input();
        if self.frame_start.is_some() {
            self.frame();
        }
    }

    /// Note a change at `at`, first sending the frame in progress if `at`
    /// falls after it.
    fn change(&mut self, at: u64) {
        if self
            .frame_start
            .is_some_and(|start| at.saturating_sub(start) >= FRAME_MS)
        {
            self.frame();
        }
        self.now = self.now.max(at);
        self.frame_start.get_or_insert(self.now);
    }

    /// Send the changes since the last frame, stamped with the latest one.
    fn frame(&mut self) {
        self.frame_start = None;
        if self.keyframe_pending || self.now.saturating_sub(self.last_keyframe) >= KEYFRAME_MS {
            self.keyframe();
        } else {
            let mutations = self.proj.diff(&self.emu, self.now);
            self.changed |= !mutations.is_empty();
            self.emit(mutations);
        }
    }

    fn keyframe(&mut self) {
        let events = self.proj.keyframe(&self.emu, self.now);
        self.emit(events);
        self.last_keyframe = self.now;
        self.keyframe_pending = false;
        self.changed = false;
    }

    fn emit(&mut self, events: Vec<Event>) {
        for event in events {
            (self.sink)(event);
        }
    }
}
