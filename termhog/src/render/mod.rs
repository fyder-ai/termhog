//! Turns the child's output into rrweb events: a VT emulator tracks the
//! screen, and the projection turns it into DOM snapshots and mutations.

mod backlog;
mod emulator;
mod glyphs;
mod paint;
mod projection;
mod remainder;
pub mod rrweb;
mod screen;
mod theme;
mod utf8;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, select, tick, unbounded};
use serde::{Deserialize, Serialize};

use crate::tty::probe::TerminalInfo;
use crate::util::{epoch_ms, spawn_recorder};
pub use backlog::Backlog;
use backlog::Chunk;
pub use remainder::{Remainder, Resumer, Saved};
use screen::Screen;
pub use screen::Sink;

/// Frame length in milliseconds: changes within one are sent together,
/// coalescing bursty output to a human-perceptible rate.
const FRAME_MS: u64 = 33;
/// How often a changing screen gets a fresh keyframe, in milliseconds, so the
/// player can seek without replaying every change from the start. Five
/// minutes, as posthog-js takes full snapshots.
const KEYFRAME_MS: u64 = 5 * 60 * 1000;
/// How long the first frame waits for the host terminal's colors, so the
/// replay starts in them. Terminals answer in milliseconds.
const COLORS_WAIT: Duration = Duration::from_millis(200);

/// A message to the render thread.
enum Msg {
    /// Output the child wrote at `at`, held in the backlog until rendered.
    Output {
        chunk: Chunk,
        at: u64,
    },
    Update(Update),
    /// The recording is over. Flush a final frame and exit, even though
    /// readers mirroring a background process may still hold feeds.
    Stop,
}

/// A change to what the recording shows, other than output. `at` is when it
/// happened (epoch milliseconds), which is where it lands in the replay
/// however late it's rendered.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Update {
    Resize {
        at: u64,
        cols: u16,
        rows: u16,
    },
    /// The user typed. Marks the timeline active, without any content.
    Input {
        at: u64,
    },
    /// More of the host terminal's colors, or its cell size, became known.
    Terminal {
        at: u64,
        info: TerminalInfo,
    },
    /// Saved by a newer version, and skipped.
    #[serde(other)]
    Unknown,
}

/// Where the session's threads report what the recording shows. Cheap to
/// clone, one per thread.
#[derive(Clone)]
pub struct Feed {
    tx: Sender<Msg>,
    backlog: Backlog,
    hand_off: Arc<AtomicBool>,
    /// The host terminal's first report, which the first frame waits for.
    first_report: Sender<TerminalInfo>,
}

impl Feed {
    /// Output the child just wrote.
    pub fn output(&self, bytes: Vec<u8>) {
        let at = epoch_ms();
        let chunk = self.backlog.hold(bytes);
        let _ = self.tx.send(Msg::Output { chunk, at });
    }

    /// The terminal was resized.
    pub fn resize(&self, cols: u16, rows: u16) {
        let at = epoch_ms();
        self.update(Update::Resize { at, cols, rows });
    }

    /// The user typed.
    pub fn input(&self) {
        self.update(Update::Input { at: epoch_ms() });
    }

    /// Some of the host terminal's colors, or its cell size, became known.
    pub fn terminal(&self, info: TerminalInfo) {
        // Only the first fits, and only while the first frame waits for it.
        let _ = self.first_report.try_send(info.clone());
        self.update(Update::Terminal {
            at: epoch_ms(),
            info,
        });
    }

    fn update(&self, update: Update) {
        let _ = self.tx.send(Msg::Update(update));
    }

    /// End the recording once everything sent so far is rendered.
    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
    }

    /// Stop rendering as soon as possible, leaving the rest in a
    /// [`Remainder`].
    pub fn hand_off(&self) {
        self.hand_off.store(true, Ordering::SeqCst);
    }
}

/// Start the render thread, which holds output in `backlog` until it's
/// rendered and sends its events to `sink`. Returns the feed for everything
/// it should show. With `probed`, the host terminal was asked for its
/// colors, and the first frame waits briefly for them.
///
/// The thread gives a [`Remainder`] when it's handed off before finishing,
/// all wrapped in `None` if it panicked.
pub fn start(
    (cols, rows): (u16, u16),
    probed: bool,
    backlog: Backlog,
    sink: Sink,
    on_panic: impl FnOnce(String) + Send + 'static,
) -> (Feed, JoinHandle<Option<Option<Remainder>>>) {
    let (tx, rx) = unbounded();
    let (first_report, first_rx) = bounded(1);
    let feed = Feed {
        tx,
        backlog,
        hand_off: Arc::new(AtomicBool::new(false)),
        first_report,
    };
    let (backlog, hand_off) = (feed.backlog.clone(), Arc::clone(&feed.hand_off));
    let run = move || {
        let info = if probed {
            first_rx.recv_timeout(COLORS_WAIT).unwrap_or_default()
        } else {
            TerminalInfo::default()
        };
        drop(first_rx);
        let screen = Screen::new(cols, rows, info, sink);
        run(screen, rx, backlog, &hand_off)
    };
    (feed, spawn_recorder(run, on_panic))
}

/// Render everything fed in, until the recording stops or is handed off.
fn run(
    mut screen: Screen,
    rx: Receiver<Msg>,
    backlog: Backlog,
    hand_off: &AtomicBool,
) -> Option<Remainder> {
    let ticker = tick(Duration::from_millis(FRAME_MS));
    loop {
        if hand_off.load(Ordering::SeqCst) {
            return Some(Remainder {
                screen,
                rx,
                backlog,
            });
        }
        select! {
            recv(rx) -> msg => match msg {
                Ok(Msg::Output { chunk, at }) => match backlog.take(chunk) {
                    Ok(bytes) => screen.output(&bytes, at),
                    // The backlog file failed, so this chunk is gone. The
                    // next keyframe shows the screen as it is again.
                    Err(_) => screen.force_keyframe(),
                },
                Ok(Msg::Update(update)) => screen.update(update),
                Ok(Msg::Stop) | Err(_) => break,
            },
            recv(ticker) -> _ => screen.tick(),
        }
    }
    screen.finish();
    None
}
