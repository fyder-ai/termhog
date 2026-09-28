//! Session orchestration: spawns the child exactly as std would, interposes on
//! the streams the caller left inherited, and wires the recorder threads.

mod finish;
mod pump;

use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rustix::termios;
use uuid::Uuid;

use crate::render::Remainder;
use crate::tty::{self, pty, signals};
use crate::upload::spool::Spool;
use crate::util::{self, epoch_ms};
use crate::{Error, HogCommand, HogStdio, Outcome, Result, TermHog, handoff, render, upload};
use pump::{Attached, Attachment, Source};

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

/// Spawn and record `command`. See [`TermHog::spawn`].
pub fn spawn(settings: TermHog, command: &mut HogCommand) -> Result<Recording> {
    let out_tty = command.stdout.is_inherit() && termios::isatty(io::stdout());
    let err_tty = command.stderr.is_inherit() && termios::isatty(io::stderr());
    let in_tty = command.stdin.is_inherit() && termios::isatty(io::stdin());

    // Where the recording's data waits to be rendered and uploaded.
    let backlog = render::Backlog::new().map_err(Error::TempFile)?;
    let spool = Spool::new().map_err(Error::TempFile)?;

    // Keeps SIGTERM/SIGHUP from killing us with the terminal left raw. It's
    // in place before raw mode, and gets the child once there is one.
    let signals = signals::watch();

    // Only take over the terminal when stdout is on it. When just stderr is
    // (e.g. `termhog -- make | less`), the terminal belongs to the next
    // program in the pipeline, so stderr is recorded through a pipe instead.
    let host = if out_tty {
        Attachment::open(settings.fallback_size)?
    } else {
        None
    };
    // When stdout is a terminal we can't take over (we're a background job,
    // or it isn't our controlling terminal), the child still gets a terminal
    // for its output, with the same settings and size, while its input stays
    // exactly as it is natively. If that can't be set up, it gets a pipe.
    let (attachment, pty, (cols, rows)) = match host {
        Some((attachment, pty)) => {
            let size = attachment.size;
            (Some(attachment), Some(pty), size)
        }
        None if out_tty => {
            let size = tty::winsize(io::stdout()).unwrap_or(settings.fallback_size);
            let termios = termios::tcgetattr(io::stdout()).ok();
            (None, termios.and_then(|t| pty::open(size, &t).ok()), size)
        }
        None => (None, None, settings.fallback_size),
    };

    // Streams the caller redirected go to std untouched. Inherited ones on
    // stdout's terminal are swapped for the pty (stdin only when input is
    // forwarded). Other inherited output goes through a pipe we tee, except
    // stderr on a different terminal than a pty-backed stdout, which keeps
    // its terminal.
    let slave = pty.as_ref().map(|p| &p.slave);
    let on_stdout_terminal = |fd: BorrowedFd| tty::same_terminal(fd, io::stdout());
    let in_pty = attachment.is_some() && in_tty && on_stdout_terminal(io::stdin().as_fd());
    let err_pty = slave.is_some() && err_tty && on_stdout_terminal(io::stderr().as_fd());
    let tee_stdout = slave.is_none() && command.stdout.is_inherit();
    let tee_stderr = command.stderr.is_inherit() && !(slave.is_some() && err_tty);
    // Output that all goes to one place (like `2>&1`) arrives there in the
    // order it's written, so it gets one pipe, read in order, not one each.
    let merged = if tee_stdout && tee_stderr && util::same_file(io::stdout(), io::stderr()) {
        Some(util::pipe()?)
    } else {
        None
    };
    let stdio = |on_pty: bool, tee: bool, spec: &HogStdio| -> io::Result<_> {
        Ok(match (slave, &merged) {
            (Some(slave), _) if on_pty => slave.try_clone()?.into(),
            (_, Some((_, write))) if tee => write.try_clone()?.into(),
            _ if tee => std::process::Stdio::piped(),
            _ => spec.to_std()?,
        })
    };
    let mut std_cmd = command.to_std();
    std_cmd.stdin(stdio(in_pty, false, &command.stdin)?);
    std_cmd.stdout(stdio(true, tee_stdout, &command.stdout)?);
    std_cmd.stderr(stdio(err_pty, tee_stderr, &command.stderr)?);
    // Only a child whose terminal we fully manage gets it as its controlling
    // terminal. Otherwise it keeps ours, with native job control.
    if let (Some(slave), Some(_)) = (slave, &attachment) {
        pty::make_controlling(&mut std_cmd, slave);
    }

    // Everything that can fail happened before the child exists, so an error
    // never leaves it running unattended.
    let mut child = std_cmd.spawn().map_err(|source| Error::Spawn {
        program: command.program.clone(),
        source,
    })?;
    let start = Instant::now();
    // Drop every slave and pipe write end we hold, so readers see EOF once
    // the child exits.
    drop(std_cmd);
    let master = pty.map(|pty| pty.master);
    let merged = merged.map(|(read, _write)| read);
    // A child with its own controlling terminal leads its own process group,
    // so signals go to the whole group.
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

    // Output readers. The pty merges every stream attached to it, mirrored to
    // stdout (a pty only exists when stdout is the terminal).
    let pty_reader = master.map(|master| {
        let feed = feed.clone();
        thread::spawn(move || pump::read_loop(master, io::stdout(), Source::Pty, feed))
    });
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    let mut pipe_readers = Vec::new();
    if let Some(pipe) = merged {
        pipe_readers.push(pump::tee(pipe, io::stdout(), &feed));
    } else {
        if tee_stdout {
            let pipe = stdout.take().expect("piped stdout");
            pipe_readers.push(pump::tee(pipe, io::stdout(), &feed));
        }
        if tee_stderr {
            let pipe = stderr.take().expect("piped stderr");
            pipe_readers.push(pump::tee(pipe, io::stderr(), &feed));
        }
    }
    let attached = attachment.map(|a| a.start(&feed));

    Ok(Recording {
        stdin: child.stdin.take(),
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
        if !util::wait_exited(self.pid, nohang)? {
            return Ok(None);
        }
        let mut active = self.active.take().expect("active until the outcome");
        let status = match active.signals.reap(&mut active.child) {
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
        let pid = self.pid;
        thread::spawn(move || {
            if util::wait_exited(pid, false).is_ok() {
                if let Ok(status) = active.signals.reap(&mut active.child) {
                    active.finish(status);
                }
            }
        });
    }
}
