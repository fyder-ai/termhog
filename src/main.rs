//! `ph-capture` records a terminal session and streams it to PostHog as a
//! session replay. It wraps a command under a pty, mirrors its output to the
//! terminal, and projects the screen into rrweb events in the background.

mod emulator;
mod identity;
mod projection;
mod pty;
mod session;
mod shipper;
mod terminal;
mod util;

use std::sync::{Arc, Mutex};

use clap::Parser;

use session::{Config, SessionOutcome};

/// Errors that abort a session run.
#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("no command given to run")]
    NoCommand,
    #[error("no PostHog project token configured (set POSTHOG_API_KEY)")]
    MissingApiKey,
    #[error(transparent)]
    Session(#[from] anyhow::Error),
}

type Result<T> = std::result::Result<T, Error>;

/// A non-fatal runtime problem. The shipper runs on a background thread and must
/// never stall the terminal, so these can't be returned from `run` — they're
/// delivered to a [`Reporter`] as they happen.
#[derive(Debug, thiserror::Error)]
enum Diagnostic {
    #[error("HTTP client init failed, streaming disabled: {0}")]
    ClientInit(String),
    #[error("snapshot upload failed: {0}")]
    Upload(String),
    #[error("analytics event '{event}' failed: {reason}")]
    Analytics { event: String, reason: String },
    #[error("upload backlog over cap: dropped {count} buffered event(s)")]
    Dropped { count: u32 },
}

/// Receives non-fatal [`Diagnostic`]s from the shipper thread. The CLI buffers
/// them and prints them to stderr once the terminal is restored.
trait Reporter: Send + Sync {
    fn report(&self, diagnostic: &Diagnostic);
}

/// Run a session with a fully-specified [`Config`]. Blocks until the child exits
/// and returns its mirrored exit status plus the replay deep link.
fn run(config: Config) -> Result<SessionOutcome> {
    if config.command.is_empty() {
        return Err(Error::NoCommand);
    }
    if config.api_key.is_empty() {
        return Err(Error::MissingApiKey);
    }
    session::run(config).map_err(Error::Session)
}

/// A set, non-empty environment variable (an empty value reads as unset).
fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Resolve the `(ingest, ui)` PostHog hosts from the environment. `POSTHOG_HOST`
/// sets the ingest host (default US Cloud). `POSTHOG_UI_HOST` overrides the
/// replay-UI host for proxied ingest. The known Cloud ingest hosts map to their
/// app host so replay deep links resolve — the api_host/ui_host split posthog-js
/// uses. Trailing slashes are trimmed so host matching and URL joining stay exact.
fn resolve_hosts() -> (String, String) {
    let host = |key| env(key).map(|h| h.trim_end_matches('/').to_string());
    let ingest = host("POSTHOG_HOST").unwrap_or_else(|| "https://us.i.posthog.com".to_string());
    let ui = host("POSTHOG_UI_HOST").unwrap_or_else(|| match ingest.as_str() {
        "https://us.i.posthog.com" => "https://us.posthog.com".to_string(),
        "https://eu.i.posthog.com" => "https://eu.posthog.com".to_string(),
        _ => ingest.clone(),
    });
    (ingest, ui)
}

/// Collects diagnostics during the session and prints them at the very end.
/// Printing mid-session would splat into the child's live terminal output, so we
/// buffer and flush after teardown (once raw mode is restored).
#[derive(Default)]
struct CollectingReporter {
    messages: Mutex<Vec<String>>,
}
impl Reporter for CollectingReporter {
    fn report(&self, diagnostic: &Diagnostic) {
        if let Ok(mut messages) = self.messages.lock() {
            messages.push(diagnostic.to_string());
        }
    }
}

/// Record a terminal session and stream it to PostHog as a session replay.
#[derive(Parser)]
#[command(name = "ph-capture", version, about)]
struct Cli {
    /// The command to run, e.g. `ph-capture -- nvim`.
    #[arg(trailing_var_arg = true, required = true, num_args = 1.., value_name = "COMMAND")]
    command: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    // Buffer diagnostics, flush after run() returns (terminal restored).
    let reporter = Arc::new(CollectingReporter::default());

    // Resolve everything from CLI args + environment here, then hand the session
    // a fully-built config. `POSTHOG_API_KEY` is the token; `PH_CAPTURE_*` pin the
    // person/session so external events can share them.
    let (ingest_host, ui_host) = resolve_hosts();
    let config = Config {
        command: cli.command,
        api_key: env("POSTHOG_API_KEY").unwrap_or_default(),
        ingest_host,
        ui_host,
        distinct_id: env("PH_CAPTURE_DISTINCT_ID"),
        session_id: env("PH_CAPTURE_SESSION_ID"),
        reporter: Some(reporter.clone()),
    };

    let outcome = run(config);

    // Everything below prints only after the session has fully torn down.
    if let Ok(messages) = reporter.messages.lock() {
        for message in messages.iter() {
            eprintln!("ph-capture: {message}");
        }
    }

    match outcome {
        Ok(outcome) => {
            if let Some(url) = &outcome.session_url {
                eprintln!("ph-capture: replay → {url}");
            }
            std::process::exit(outcome.exit_status);
        }
        Err(e) => {
            eprintln!("ph-capture: {e:#}");
            std::process::exit(1);
        }
    }
}
