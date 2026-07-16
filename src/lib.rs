//! `ph-capture` records terminal sessions and streams them to PostHog as
//! session replays.
//!
//! Two ways to use it:
//! - [`builder`]/[`run`] to wrap and record an explicit command;
//! - [`init`] at the top of `main` to make a program record *itself*.

pub mod cast;
mod emulator;
mod identity;
mod projection;
mod pty;
mod session;
mod shipper;
mod terminal;
mod util;

use std::path::PathBuf;

pub use session::{Config, SessionOutcome};

const DEFAULT_HOST: &str = "https://us.i.posthog.com";

/// Errors surfaced by the public API.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no command given to run")]
    NoCommand,
    #[error("no PostHog project token configured (set POSTHOG_API_KEY)")]
    MissingApiKey,
    #[error(transparent)]
    Session(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A non-fatal runtime problem. The shipper runs on a background thread and must
/// never stall the terminal, so these can't be returned from [`run`] — they're
/// delivered to a [`Reporter`] as they happen.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Diagnostic {
    #[error("HTTP client init failed, streaming disabled: {0}")]
    ClientInit(String),
    #[error("snapshot upload failed: {0}")]
    Upload(String),
    #[error("analytics event '{event}' failed: {reason}")]
    Analytics { event: String, reason: String },
    #[error("upload backlog over cap: dropped {count} buffered event(s)")]
    Dropped { count: u32 },
}

/// Receives non-fatal [`Diagnostic`]s from a running session (e.g. failed
/// uploads). Set one via [`Builder::reporter`]; the CLI prints them to stderr,
/// and embedders can route them to their own telemetry. When none is set,
/// diagnostics are simply not emitted.
pub trait Reporter: Send + Sync {
    fn report(&self, diagnostic: &Diagnostic);
}

/// Run a session with a fully-specified [`Config`]. Blocks until the child exits
/// and returns its mirrored exit status plus the replay deep link.
pub fn run(config: Config) -> Result<SessionOutcome> {
    if config.command.is_empty() {
        return Err(Error::NoCommand);
    }
    if config.api_key.is_empty() {
        return Err(Error::MissingApiKey);
    }
    session::run(config).map_err(Error::Session)
}

/// Build a [`Config`] for `command`, filling PostHog settings from the
/// environment (`POSTHOG_API_KEY`/`PH_CAPTURE_API_KEY`, `POSTHOG_HOST`, …).
/// Errors if no token is configured.
pub fn config_from_env(command: Vec<String>) -> Result<Config> {
    Ok(Config {
        command,
        api_key: env_api_key(None).ok_or(Error::MissingApiKey)?,
        posthog_host: env_host(None),
        distinct_id: env_distinct_id(None),
        session_id: env_session_id(None),
        record_input: env_bool("PH_CAPTURE_RECORD_INPUT"),
        cast_path: std::env::var_os("PH_CAPTURE_DEBUG_CAST").map(PathBuf::from),
        rrweb_debug_path: std::env::var_os("PH_CAPTURE_DEBUG_RRWEB").map(PathBuf::from),
        reporter: None,
    })
}

/// Options for [`init`]. Any unset field falls back to the environment. Derives
/// `Deserialize` so a C/FFI shim can parse a JSON config straight into it and
/// call [`init`] — the Rust side stays struct-typed, the shim owns JSON.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(default)]
pub struct InitOptions {
    pub api_key: Option<String>,
    pub posthog_host: Option<String>,
    pub distinct_id: Option<String>,
    pub session_id: Option<String>,
    pub record_input: bool,
}

/// Record the current program itself. Call once, first thing in `main`.
///
/// On a normal launch this re-executes the same binary as a child under a
/// recording pty and streams it to PostHog, exiting with the child's status. In
/// the re-executed child it returns immediately, so the program runs as usual
/// with its output captured. If no token is configured (in `options` or the
/// environment) or recording setup fails, it returns and the program runs
/// normally — it never blocks the host app.
pub fn init(options: InitOptions) {
    // The recorder injects PH_CAPTURE_SESSION_ID into the child; its presence
    // means we are already being recorded — run normally.
    if std::env::var_os("PH_CAPTURE_SESSION_ID").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = vec![exe.to_string_lossy().into_owned()];
    command.extend(std::env::args().skip(1));

    // Options win, environment fills the rest. No token anywhere => run normally.
    let Some(api_key) = env_api_key(options.api_key) else {
        return;
    };
    let config = Config {
        command,
        api_key,
        posthog_host: env_host(options.posthog_host),
        distinct_id: env_distinct_id(options.distinct_id),
        session_id: env_session_id(options.session_id),
        record_input: options.record_input || env_bool("PH_CAPTURE_RECORD_INPUT"),
        cast_path: None,
        rrweb_debug_path: None,
        reporter: None,
    };
    match run(config) {
        Ok(outcome) => std::process::exit(outcome.exit_status),
        Err(e) => log::warn!("ph-capture: recording disabled ({e})"),
    }
}

/// Ergonomic builder for wrapping a command programmatically.
#[derive(Default)]
pub struct Builder {
    command: Vec<String>,
    api_key: Option<String>,
    posthog_host: Option<String>,
    distinct_id: Option<String>,
    session_id: Option<String>,
    record_input: bool,
    reporter: Option<std::sync::Arc<dyn Reporter>>,
}

/// Start a [`Builder`].
pub fn builder() -> Builder {
    Builder::default()
}

impl Builder {
    /// The command and args to run (required).
    pub fn command(mut self, argv: Vec<String>) -> Self {
        self.command = argv;
        self
    }
    /// PostHog project token (required; falls back to env).
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
    /// Ingestion host (default `https://us.i.posthog.com`).
    pub fn posthog_host(mut self, host: impl Into<String>) -> Self {
        self.posthog_host = Some(host.into());
        self
    }
    /// Person id (default: persisted anonymous id).
    pub fn distinct_id(mut self, id: impl Into<String>) -> Self {
        self.distinct_id = Some(id.into());
        self
    }
    /// Session id (default: fresh UUIDv7 per run).
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }
    /// Record user keystrokes as input events (echo-gated). Off by default.
    pub fn record_input(mut self, enabled: bool) -> Self {
        self.record_input = enabled;
        self
    }
    /// Receiver for non-fatal upload diagnostics. Unset => diagnostics dropped.
    pub fn reporter(mut self, reporter: std::sync::Arc<dyn Reporter>) -> Self {
        self.reporter = Some(reporter);
        self
    }

    /// Build the [`Config`], filling unset PostHog settings from the environment.
    pub fn build(self) -> Result<Config> {
        if self.command.is_empty() {
            return Err(Error::NoCommand);
        }
        Ok(Config {
            command: self.command,
            api_key: env_api_key(self.api_key).ok_or(Error::MissingApiKey)?,
            posthog_host: env_host(self.posthog_host),
            distinct_id: env_distinct_id(self.distinct_id),
            session_id: env_session_id(self.session_id),
            record_input: self.record_input,
            cast_path: None,
            rrweb_debug_path: None,
            reporter: self.reporter,
        })
    }

    /// Build and run.
    pub fn run(self) -> Result<SessionOutcome> {
        run(self.build()?)
    }
}

/// First set, non-empty environment variable from `keys`, in order.
fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
}

// Env-var precedence for the PostHog settings, shared by `config_from_env`, the
// `Builder`, and `init`: an explicit value always wins, else the environment, in
// the order below. These are the single source of truth for the variable names.

fn env_api_key(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| env_first(&["PH_CAPTURE_API_KEY", "POSTHOG_API_KEY"]))
}

fn env_host(explicit: Option<String>) -> String {
    explicit
        .or_else(|| env_first(&["PH_CAPTURE_POSTHOG_URL", "POSTHOG_HOST"]))
        .unwrap_or_else(|| DEFAULT_HOST.to_string())
}

fn env_distinct_id(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| env_first(&["PH_CAPTURE_DISTINCT_ID"]))
}

fn env_session_id(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| env_first(&["PH_CAPTURE_SESSION_ID"]))
}

fn env_bool(key: &str) -> bool {
    matches!(
        std::env::var(key).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}
