//! Ending a recording once its child has exited.
//!
//! The rest of the output is drained, the terminal given back, and the
//! recording gets [`Active::finish_timeout`] to render and upload what's
//! left. Whatever isn't done by then is handed off to the background
//! uploader (see [`crate::handoff`]), so the caller is never held up by a
//! slow network or a backlog of output still being rendered.

use std::process::ExitStatus;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};

use super::Active;
use crate::handoff;
use crate::upload::spool::Spool;
use crate::upload::{self, Ending, Leftovers};
use crate::util::{self, epoch_ms};

/// How long the pty reader gets to drain after the child exits.
const READER_GRACE: Duration = Duration::from_millis(250);

impl Active {
    /// Finish the recording of a child that exited with `status`. Returns
    /// how long it ran.
    pub(super) fn finish(mut self, status: ExitStatus) -> Duration {
        let elapsed = self.start.elapsed();
        let ending = Ending {
            exit_code: util::exit_code(status),
            duration_ms: elapsed.as_millis() as u64,
            at: epoch_ms(),
        };
        self.drain_output();
        let deadline = Instant::now() + self.finish_timeout;

        // Render what's left, or stop and keep the rest for the background
        // uploader.
        self.feed.stop();
        if !finished_by(&self.render_handle, deadline) {
            self.feed.hand_off();
        }
        let remainder = self.render_handle.join().ok().flatten().flatten();

        match remainder {
            // Rendering is done: upload what's left, or hand that off.
            None => {
                let _ = self.upload_tx.send(upload::Msg::End(ending));
                if !finished_by(&self.upload_handle, deadline) {
                    if let Some(leftovers) = take_leftovers(&self.upload_tx) {
                        handoff::hand_off(&self.config, leftovers, None);
                    }
                }
            }
            // Rendering is unfinished, so uploading is too. The ending goes
            // with the rest, for `term_end` once everything's sent.
            Some(remainder) => {
                let mut leftovers = take_leftovers(&self.upload_tx)
                    .unwrap_or_else(|| Leftovers::new(Spool::in_memory()));
                leftovers.ending = Some(ending);
                handoff::hand_off(&self.config, leftovers, Some(remainder));
            }
        }
        // Done or handed off: a pending SIGTERM/SIGHUP may now end the process.
        drop(self.signals);
        elapsed
    }

    /// Let the child's output finish flowing, then give the terminal back.
    fn drain_output(&mut self) {
        // A pipe stays open while anything the child started (`server &`)
        // still writes to it, and natively the reader at the other end (like
        // `cmd | cat`) keeps going until it closes. So do the same, keeping
        // that output flowing and recorded.
        for reader in self.pipe_readers.drain(..) {
            let _ = reader.join();
        }
        // On a terminal, such a process natively writes straight to the
        // screen without delaying anything. So give the pty reader a moment
        // to drain what the child left buffered, then leave it mirroring any
        // background output (unrecorded) and finish now.
        if let Some(reader) = &self.pty_reader {
            finished_by(reader, Instant::now() + READER_GRACE);
        }
        if let Some(attached) = self.attached.take() {
            attached.detach();
        }
    }
}

/// Stop the upload thread and take what it hasn't sent. `None` if it already
/// finished.
fn take_leftovers(upload_tx: &Sender<upload::Msg>) -> Option<Leftovers> {
    let (reply_tx, reply_rx) = bounded(1);
    upload_tx.send(upload::Msg::HandOff(reply_tx)).ok()?;
    reply_rx.recv().ok()
}

/// Whether `thread` finishes by `deadline`, waiting until then.
fn finished_by<T>(thread: &JoinHandle<T>, deadline: Instant) -> bool {
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}
