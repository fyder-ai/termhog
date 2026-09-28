#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

#[cfg(not(unix))]
compile_error!("termhog doesn't support this platform yet");

mod command;
mod handoff;
mod render;
mod session;
mod tty;
mod upload;
mod util;

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::resource::{Resource, setrlimit};
use nix::sys::signal::Signal;

pub use command::{HogCommand, HogStdio};
pub use session::Recording;

const US_INGEST_HOST: &str = "https://us.i.posthog.com";
const EU_INGEST_HOST: &str = "https://eu.i.posthog.com";

/// Set TermHog up. Call it as the very first thing in `main`, before
/// recording anything: [`TermHog::spawn`] panics otherwise.
///
/// Recordings finish in the background, so a slow network never holds up
/// your program's exit: whatever isn't uploaded shortly after a command
/// exits is saved to the user's cache folder (`termhog/` in it), and your
/// program's own executable is started again, detached, to upload it. In
/// that process, `init` does the uploading and exits, so the rest of `main`
/// never runs there. That's why it has to come first: anything before it
/// runs in the uploader too.
///
/// Otherwise `init` returns at once, after starting the uploader if earlier
/// runs left anything behind (say, the machine shut down mid-upload).
///
/// In CI, where leftover processes don't outlive the job, no uploader is
/// started: recordings get longer to finish in-process instead, and what's
/// left waits for a later run on the same machine.
///
/// ```no_run
/// // The first line of `main`:
/// termhog::init();
/// ```
pub fn init() {
    handoff::init();
    tty::install_panic_hook();
}

/// Errors that stop a recording from starting or finishing.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The project token is empty.
    #[error("no PostHog project token configured")]
    MissingApiKey,
    /// Another recording in this process already has the terminal.
    #[error("another recording is already attached to this terminal")]
    TerminalBusy,
    /// The temporary files the recording keeps its data in couldn't be
    /// created (for example, the temp folder isn't writable).
    #[error("couldn't create a temporary file for the recording: {0}")]
    TempFile(#[source] std::io::Error),
    /// The command couldn't be started, like [`std::process::Command::spawn`]
    /// failing (for example, it doesn't exist or isn't executable).
    #[error("{}: {source}", program.to_string_lossy())]
    Spawn {
        /// The program that was to run.
        program: std::ffi::OsString,
        /// Why it couldn't start.
        source: std::io::Error,
    },
    /// Setting up or waiting on the recording failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A `Result` with TermHog's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// A non-fatal problem while recording. Recording must never stall the child,
/// so these go to a [`Reporter`] as they happen.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Diagnostic {
    /// Uploading part of the replay failed. It's retried unless the server
    /// rejected it outright.
    #[error("replay upload failed: {message}")]
    Upload {
        /// The HTTP status, when the server answered with an error.
        status: Option<u16>,
        /// What went wrong.
        message: String,
    },
    /// Replay data couldn't be queued for upload.
    #[error("couldn't queue replay data for upload: {0}")]
    Queue(String),
    /// A `term_start`/`term_end` analytics event couldn't be sent.
    #[error("analytics event '{event}' failed: {reason}")]
    Analytics {
        /// The event's name.
        event: String,
        /// What went wrong.
        reason: String,
    },
    /// Replay events the server rejected outright, which won't be retried.
    #[error("dropped {count} replay event(s) that couldn't be uploaded")]
    Dropped {
        /// How many events.
        count: u32,
    },
    /// An internal error stopped the recording. The child and its output
    /// are unaffected.
    #[error("recording stopped by an internal error: {0}")]
    Recorder(String),
}

/// Receives [`Diagnostic`]s from a running recording. Printing them while the
/// child owns the terminal would corrupt its output, so implementations usually
/// buffer them until [`Recording::wait`] returns.
///
/// Any `Fn(&Diagnostic)` closure is a reporter.
pub trait Reporter: Send + Sync {
    /// Handle one diagnostic. Called from a background thread.
    fn report(&self, diagnostic: &Diagnostic);
}

impl<F: Fn(&Diagnostic) + Send + Sync> Reporter for F {
    fn report(&self, diagnostic: &Diagnostic) {
        self(diagnostic)
    }
}

/// The result of a finished recording.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Outcome {
    /// The child's exit status.
    pub status: ExitStatus,
    /// Deep link to the replay in PostHog. It can take a few minutes to process.
    pub replay_url: String,
}

impl Outcome {
    /// The exit status as a shell reports it (`$?`): the child's exit code, or
    /// `128 + signal` when a signal killed it.
    pub fn exit_code(&self) -> i32 {
        util::exit_code(self.status)
    }

    /// End this process the way the child ended, for programs that wrap a
    /// command (like the `termhog` CLI), so their callers see the same result:
    /// exit with the child's code, or die from the signal that killed it.
    ///
    /// Dying from the signal matters beyond `$?`: a shell stops a running
    /// script when a command is killed by Ctrl+C, but not when one merely
    /// exits with 130. Core dumps are disabled first, so a crashed child
    /// (SIGSEGV, SIGABRT) doesn't also produce a core of this process.
    pub fn exit(&self) -> ! {
        if let Some(signal) = self.status.signal().and_then(|s| Signal::try_from(s).ok()) {
            let _ = setrlimit(Resource::RLIMIT_CORE, 0, 0);
            // Crash handlers like systemd-coredump ignore the core limit.
            #[cfg(target_os = "linux")]
            let _ = nix::sys::prctl::set_dumpable(false);
            util::die_from(signal);
        }
        // Not killed by a signal (or it didn't end us): exit with the code.
        std::process::exit(self.exit_code())
    }
}

/// PostHog settings for one recording.
///
/// Settings are explicit. The environment is only consulted the way programs
/// usually do: uploads honor the standard proxy variables (`HTTPS_PROXY`,
/// `NO_PROXY`), common CI variables (`CI`, `GITHUB_ACTIONS` and the like)
/// switch to finishing uploads in-process, and the terminal isn't probed for
/// its colors under GNU screen or `TERM=dumb`.
///
/// Replay data waiting to be processed or uploaded is kept in unnamed
/// temporary files, which disappear with the process. What's handed to the
/// background uploader (see [`init`]) is saved in the user's cache folder
/// until it's uploaded, a week passes, or the folder passes 100 MB.
#[derive(Clone)]
pub struct TermHog {
    pub(crate) api_key: String,
    pub(crate) ingest_host: String,
    pub(crate) ui_host: Option<String>,
    pub(crate) distinct_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) reporter: Option<Arc<dyn Reporter>>,
    pub(crate) fallback_size: (u16, u16),
    pub(crate) finish_timeout: Option<Duration>,
}

impl TermHog {
    /// Settings for the project with this token (the public `phc_` token).
    pub fn new(api_key: impl Into<String>) -> TermHog {
        TermHog {
            api_key: api_key.into(),
            ingest_host: US_INGEST_HOST.to_string(),
            ui_host: None,
            distinct_id: None,
            session_id: None,
            reporter: None,
            fallback_size: (80, 24),
            finish_timeout: None,
        }
    }

    /// Where events are sent. Defaults to PostHog US Cloud.
    pub fn ingest_host(mut self, host: impl Into<String>) -> TermHog {
        self.ingest_host = trim_host(host.into());
        self
    }

    /// Where replay links point, when it differs from the ingest host (for
    /// example behind a proxy). The PostHog Cloud ingest hosts map to their app
    /// host automatically.
    pub fn ui_host(mut self, host: impl Into<String>) -> TermHog {
        self.ui_host = Some(trim_host(host.into()));
        self
    }

    /// The PostHog person. Without it, the recording is anonymous, and
    /// PostHog creates no person profile for it.
    pub fn distinct_id(mut self, id: impl Into<String>) -> TermHog {
        self.distinct_id = Some(id.into());
        self
    }

    /// The replay's session ID, so other events can share it. Defaults to a
    /// fresh UUIDv7.
    pub fn session_id(mut self, id: impl Into<String>) -> TermHog {
        self.session_id = Some(id.into());
        self
    }

    /// Where non-fatal recording problems go (see [`Diagnostic`]). Defaults
    /// to nowhere. A closure works: `Arc::new(|d: &Diagnostic| ...)`.
    pub fn reporter(mut self, reporter: Arc<dyn Reporter>) -> TermHog {
        self.reporter = Some(reporter);
        self
    }

    /// The recorded screen size when there's no terminal to take it from:
    /// output going to a pipe or file, or a terminal that reports no size.
    /// Defaults to 80x24.
    pub fn fallback_size(mut self, cols: u16, rows: u16) -> TermHog {
        self.fallback_size = (cols.max(1), rows.max(1));
        self
    }

    /// How long a recording gets to finish after its command exits, before
    /// the rest is handed to the background uploader (see [`init`]).
    /// Defaults to 0.3 seconds, or 20 seconds in CI.
    pub fn finish_timeout(mut self, timeout: Duration) -> TermHog {
        self.finish_timeout = Some(timeout);
        self
    }

    /// Start `command` and record it, like [`std::process::Command::spawn`].
    ///
    /// # Signals
    ///
    /// While a recording runs, signals that would kill the process by
    /// default are handled so the child sees them as it would natively:
    ///
    /// - SIGTERM and SIGHUP are passed to the child. The recording then gets
    ///   a moment to flush and the terminal is restored before the process
    ///   dies from the signal.
    /// - SIGINT and SIGQUIT typed on the terminal (Ctrl+C, Ctrl+\\) reach
    ///   only the child, as they would natively. Ones sent to this process by
    ///   another program (`kill -INT`) are passed to the child.
    ///
    /// Signals the app handles or ignores itself are left alone.
    ///
    /// One case can't be told apart: when the child doesn't get its own
    /// controlling terminal (output isn't a terminal, or this process is a
    /// background job), it shares this process's process group. A signal
    /// sent to the whole group (`kill -INT -<pgid>`) then reaches the child
    /// directly and is also passed on, so the child gets it twice. Signals
    /// sent to this process alone, and keys typed on the terminal, arrive
    /// exactly once.
    ///
    /// # Panics
    ///
    /// If [`init`] wasn't called first.
    pub fn spawn(self, command: &mut HogCommand) -> Result<Recording> {
        if self.api_key.is_empty() {
            return Err(Error::MissingApiKey);
        }
        handoff::ensure_initialized();
        session::spawn(self, command)
    }

    /// Run `command` to completion and record it, like
    /// [`std::process::Command::status`]. Blocks until the child exits.
    pub fn status(self, command: &mut HogCommand) -> Result<Outcome> {
        self.spawn(command)?.wait()
    }

    /// The replay-UI host: explicit, else the app host for a known Cloud
    /// ingest host, else the ingest host itself.
    pub(crate) fn resolved_ui_host(&self) -> String {
        if let Some(ui) = &self.ui_host {
            return ui.clone();
        }
        match self.ingest_host.as_str() {
            US_INGEST_HOST => "https://us.posthog.com".to_string(),
            EU_INGEST_HOST => "https://eu.posthog.com".to_string(),
            other => other.to_string(),
        }
    }
}

impl std::fmt::Debug for TermHog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermHog")
            .field("ingest_host", &self.ingest_host)
            .field("ui_host", &self.ui_host)
            .field("distinct_id", &self.distinct_id)
            .field("session_id", &self.session_id)
            .field("fallback_size", &self.fallback_size)
            .finish_non_exhaustive()
    }
}

/// Trailing slashes are trimmed so host matching and URL joining stay exact.
fn trim_host(host: String) -> String {
    host.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_host_resolution() {
        assert_eq!(
            TermHog::new("t").resolved_ui_host(),
            "https://us.posthog.com"
        );
        let eu = TermHog::new("t").ingest_host("https://eu.i.posthog.com/");
        assert_eq!(eu.resolved_ui_host(), "https://eu.posthog.com");
        let proxied = TermHog::new("t").ingest_host("https://ph.example.com");
        assert_eq!(proxied.resolved_ui_host(), "https://ph.example.com");
        let explicit = proxied.ui_host("https://app.example.com/");
        assert_eq!(explicit.resolved_ui_host(), "https://app.example.com");
    }
}
