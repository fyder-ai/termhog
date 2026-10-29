//! Session orchestration: spawns the child exactly as std would, interposes on
//! the streams the caller left inherited, and wires the recorder threads.

mod finish;
mod pump;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::fs::File;
use std::io;
use std::process::{ChildStderr, ChildStdin, ChildStdout};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use uuid::Uuid;

use crate::render::Remainder;
use crate::tty::pty::Child;
use crate::tty::signals;
use crate::upload::spool::Spool;
use crate::util::{self, epoch_ms};
use crate::{Error, HogCommand, Outcome, Result, TermHog, handoff, render, upload};
use pump::{Attached, Attachment, PtyOutput, Source};
#[cfg(unix)]
use unix::start_child;
#[cfg(windows)]
use windows::start_child;

/// A running, recorded child. Mirrors [`std::process::Child`].
///
/// Dropping it without calling [`Recording::wait`] restores the terminal
/// but doesn't wait for or kill the child, like std. The recording still
/// finishes, in the background, once the child exits (if the app is still
/// running then).
pub struct Recording {
    /// The child's stdin, if the command set it to [`HogStdio::piped`].
    pub stdin: Option<ChildStdin>,
    /// The child's stdout, if the command set it to [`HogStdio::piped`].
    pub stdout: Option<ChildStdout>,
    /// The child's stderr, if the command set it to [`HogStdio::piped`].
    pub stderr: Option<ChildStderr>,
    pid: u32,
    replay_url: String,
    /// The child and the recording machinery, until the child is waited on.
    active: Option<Active>,
    outcome: Option<Outcome>,
}

/// The child, and everything that runs while it's alive.
struct Active {
    child: Child,
    start: Instant,
    /// What the upload thread was given, for handing off what it doesn't
    /// finish.
    config: upload::Config,
    /// How long the recording gets to finish before being handed off.
    finish_timeout: Duration,
    feed: render::Feed,
    render_handle: JoinHandle<Option<Option<Remainder>>>,
    upload_tx: Sender<upload::Msg>,
    upload_handle: JoinHandle<Option<()>>,
    /// The thread reading the pty, which ends at EOF.
    pty_reader: Option<JoinHandle<()>>,
    /// Threads teeing the child's output pipes, which end at EOF.
    pipe_readers: Vec<JoinHandle<()>>,
    attached: Option<Attached>,
    /// Keeps SIGTERM/SIGHUP from killing us mid-recording and forwards
    /// signals to the child (see `signals`). Dropped last, once the
    /// recording has flushed.
    signals: signals::Guard,
}

impl Active {
    /// Wait for the child to exit, without collecting its status (see
    /// [`Active::reap`]). With `nohang`, only checks. Returns whether it has
    /// exited.
    fn wait_exited(&self, nohang: bool) -> io::Result<bool> {
        // On Unix by process ID, which stays the child's until it's reaped.
        #[cfg(unix)]
        return util::wait_exited(self.child.id(), nohang);
        // On Windows through the handle the child holds.
        #[cfg(windows)]
        return util::wait_exited(&self.child, nohang);
    }

    /// Collect the exited child's status. On Unix that goes through the
    /// signal guard, so no signal is ever sent to its reused process ID.
    fn reap(&mut self) -> io::Result<std::process::ExitStatus> {
        #[cfg(unix)]
        return self.signals.reap(&mut self.child);
        #[cfg(windows)]
        return self.child.wait();
    }
}

/// A child the platform started (see `unix.rs` and `windows.rs`), and how
/// its output reaches us.
struct Started {
    child: Child,
    start: Instant,
    /// Our ends of the streams the command set to [`crate::HogStdio::piped`].
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    /// The terminal, when the child runs on a pty that takes it over.
    attachment: Option<Attachment>,
    /// The pty's output, mirrored to stdout. The pty merges every stream
    /// attached to it, and only exists when stdout is the terminal.
    pty_output: Option<PtyOutput>,
    /// Output pipes to tee, each with whether it goes to stderr (else stdout).
    tees: Vec<(File, bool)>,
    size: (u16, u16),
}

/// Spawn and record `command`. See [`TermHog::spawn`].
pub fn spawn(settings: TermHog, command: &mut HogCommand) -> Result<Recording> {
    // Where the recording's data waits to be rendered and uploaded.
    let backlog = render::Backlog::new().map_err(Error::TempFile)?;
    let spool = Spool::new().map_err(Error::TempFile)?;

    // Keeps SIGTERM/SIGHUP from killing us with the terminal left raw. It's
    // in place before raw mode, and gets the child once there is one.
    let signals = signals::watch();

    let Started {
        child,
        start,
        stdin,
        stdout,
        stderr,
        attachment,
        pty_output,
        tees,
        size: (cols, rows),
    } = start_child(&settings, command)?;
    // A child with its own controlling terminal leads its own process group,
    // so signals go to the whole group.
    #[cfg(unix)]
    signals.set_child(child.id(), attachment.is_some());

    let session_id = settings
        .session_id
        .clone()
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let replay_url =
        upload::replay_url(&settings.resolved_ui_host(), &settings.api_key, &session_id);
    let config = upload_config(&settings, command, session_id);
    let (upload_tx, upload_handle) = upload::start(config.clone(), spool);
    let sink = {
        let upload_tx = upload_tx.clone();
        Box::new(move |event| {
            let _ = upload_tx.send(upload::Msg::Event(event));
        })
    };
    let probed = attachment.as_ref().is_some_and(|a| a.probed);
    let on_panic = config.on_panic();
    let (feed, render_handle) = render::start((cols, rows), probed, backlog, sink, on_panic);

    // Output readers.
    let pty_reader = pty_output.map(|output| {
        let feed = feed.clone();
        thread::spawn(move || pump::read_loop(output, io::stdout(), Source::Pty, feed))
    });
    let pipe_readers = tees
        .into_iter()
        .map(|(pipe, to_stderr)| {
            if to_stderr {
                pump::tee(pipe, io::stderr(), &feed)
            } else {
                pump::tee(pipe, io::stdout(), &feed)
            }
        })
        .collect();
    let attached = attachment.map(|a| a.start(&feed));

    Ok(Recording {
        stdin,
        stdout,
        stderr,
        pid: child.id(),
        replay_url,
        active: Some(Active {
            child,
            start,
            config,
            finish_timeout: settings
                .finish_timeout
                .unwrap_or_else(handoff::default_finish_timeout),
            feed,
            render_handle,
            upload_tx,
            upload_handle,
            pty_reader,
            pipe_readers,
            attached,
            signals,
        }),
        outcome: None,
    })
}

/// Who the recording belongs to, from `settings`.
fn upload_config(settings: &TermHog, command: &HogCommand, session_id: String) -> upload::Config {
    upload::Config {
        ingest_host: settings.ingest_host.clone(),
        api_key: settings.api_key.clone(),
        distinct_id: settings
            .distinct_id
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string()),
        person_profile: settings.distinct_id.is_some(),
        session_id,
        command: command.display(),
        started_at: epoch_ms(),
        reporter: settings.reporter.clone(),
    }
}

impl Recording {
    /// The child's OS process ID.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Kill the child, like [`std::process::Child::kill`]. Does nothing once
    /// it has been waited on.
    pub fn kill(&mut self) -> io::Result<()> {
        self.active.as_mut().map_or(Ok(()), |a| a.child.kill())
    }

    /// Deep link to this recording's replay in PostHog.
    pub fn replay_url(&self) -> &str {
        &self.replay_url
    }

    /// Wait for the child to exit, then finish the recording. Like
    /// [`std::process::Child::wait`], stdin is closed first so a child reading
    /// it can finish, and repeated calls return the same outcome.
    ///
    /// Returns shortly after the child exits, even on a slow network (see
    /// [`TermHog::finish_timeout`]).
    pub fn wait(&mut self) -> Result<Outcome> {
        if self.outcome.is_none() {
            drop(self.stdin.take());
        }
        Ok(self.poll(false)?.expect("waited for the child to exit"))
    }

    /// If the child has exited, finish the recording and return the outcome,
    /// like [`std::process::Child::try_wait`]. `None` while it's running.
    pub fn try_wait(&mut self) -> Result<Option<Outcome>> {
        self.poll(true)
    }

    /// The outcome, once the child has exited, finishing the recording then.
    /// With `nohang`, only checks.
    fn poll(&mut self, nohang: bool) -> Result<Option<Outcome>> {
        if let Some(outcome) = &self.outcome {
            return Ok(Some(outcome.clone()));
        }
        let active = self.active.as_ref().expect("active until the outcome");
        if !active.wait_exited(nohang)? {
            return Ok(None);
        }
        let mut active = self.active.take().expect("active until the outcome");
        let status = match active.reap() {
            Ok(status) => status,
            Err(e) => {
                // Left in place to try again, as std's `wait` does.
                self.active = Some(active);
                return Err(e.into());
            }
        };
        let elapsed = active.finish(status);

        // Seek near the end of a failed run, where the interesting part is.
        let replay_url = if status.success() {
            self.replay_url.clone()
        } else {
            let seek = elapsed.as_secs().saturating_sub(10);
            upload::replay_url_at(&self.replay_url, seek)
        };
        let outcome = Outcome { status, replay_url };
        self.outcome = Some(outcome.clone());
        Ok(Some(outcome))
    }
}

impl std::fmt::Debug for Recording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recording")
            .field("pid", &self.pid)
            .field("replay_url", &self.replay_url)
            .finish_non_exhaustive()
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        // Give the terminal back now.
        if let Some(attached) = active.attached.take() {
            attached.detach();
        }
        // Finish the recording once the child exits, without waiting here.
        thread::spawn(move || {
            if active.wait_exited(false).is_ok() {
                if let Ok(status) = active.reap() {
                    active.finish(status);
                }
            }
        });
    }
}
