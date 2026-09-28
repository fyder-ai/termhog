//! The `termhog` command records a terminal session and streams it to PostHog
//! as a session replay. It's a thin wrapper over the TermHog library that takes
//! its settings from flags or the matching `POSTHOG_*` environment variables.

use std::ffi::OsString;
use std::io::{ErrorKind, Write};
use std::sync::{Arc, Mutex};

use clap::Parser;
use termhog::{Command, Diagnostic, Error, Reporter, TermHog};

/// Diagnostics collected during the session, for `main` to print once the
/// terminal is restored. Printed mid-session, they'd land in the middle of
/// the child's output.
type Messages = Arc<Mutex<Vec<String>>>;

/// Record a terminal session and stream it to PostHog as a session replay.
#[derive(Parser)]
#[command(name = "termhog", version)]
struct Cli {
    /// PostHog project token (the public `phc_` token).
    #[arg(long, env = "POSTHOG_API_KEY", hide_env_values = true)]
    api_key: String,

    /// Where events are sent. Defaults to PostHog US Cloud.
    #[arg(long, env = "POSTHOG_HOST", hide_env_values = true)]
    host: Option<String>,

    /// Where replay links point, for ingestion behind a proxy. The PostHog
    /// Cloud hosts map to their app host automatically.
    #[arg(long, env = "POSTHOG_UI_HOST", hide_env_values = true)]
    ui_host: Option<String>,

    /// The PostHog person. Defaults to an anonymous one.
    #[arg(long, env = "POSTHOG_DISTINCT_ID", hide_env_values = true)]
    distinct_id: Option<String>,

    /// The session ID, so other events can share it. Defaults to a fresh
    /// UUIDv7.
    #[arg(long, env = "POSTHOG_SESSION_ID", hide_env_values = true)]
    session_id: Option<String>,

    /// The command to run, e.g. `termhog -- nvim`.
    #[arg(trailing_var_arg = true, required = true, num_args = 1.., value_name = "COMMAND")]
    command: Vec<OsString>,
}

impl Cli {
    /// The library settings these arguments describe. An empty value (like
    /// `POSTHOG_HOST=`) counts as unset.
    fn settings(&self, reporter: Arc<dyn Reporter>) -> TermHog {
        let set = |value: &Option<String>| value.clone().filter(|v| !v.is_empty());
        let mut settings = TermHog::new(&self.api_key).reporter(reporter);
        if let Some(host) = set(&self.host) {
            settings = settings.ingest_host(host);
        }
        if let Some(host) = set(&self.ui_host) {
            settings = settings.ui_host(host);
        }
        if let Some(id) = set(&self.distinct_id) {
            settings = settings.distinct_id(id);
        }
        if let Some(id) = set(&self.session_id) {
            settings = settings.session_id(id);
        }
        // Without a terminal (CI logs, pipes), record the size the shell says
        // it has. Malformed values are ignored rather than failing the run.
        let dim = |key| std::env::var(key).ok()?.parse().ok();
        if let (Some(cols), Some(rows)) = (dim("COLUMNS"), dim("LINES")) {
            settings = settings.fallback_size(cols, rows);
        }
        settings
    }
}

/// Print a message on stderr, ignoring failure. stderr may be a closed pipe
/// (`termhog -- cmd 2>&1 | head`), where `eprintln!` would panic.
fn note(message: std::fmt::Arguments) {
    let _ = writeln!(std::io::stderr(), "termhog: {message}");
}

fn main() {
    // Must come first: in the background uploader, this uploads and exits.
    termhog::init();
    let cli = Cli::parse();
    let messages = Messages::default();
    let reporter = {
        let messages = Arc::clone(&messages);
        move |diagnostic: &Diagnostic| {
            if let Ok(mut messages) = messages.lock() {
                messages.push(diagnostic.to_string());
            }
        }
    };

    let mut command = Command::new(&cli.command[0]);
    command.args(&cli.command[1..]);

    let outcome = cli.settings(Arc::new(reporter)).status(&mut command);

    // Everything below prints only after the terminal is restored.
    if let Ok(messages) = messages.lock() {
        for message in messages.iter() {
            note(format_args!("{message}"));
        }
    }

    match outcome {
        Ok(outcome) => {
            note(format_args!("replay → {}", outcome.replay_url));
            // End the way the child did, so callers see the same result.
            outcome.exit();
        }
        Err(e) => {
            note(format_args!("{e}"));
            // Like `env`, `timeout` and shells: 127 when the command wasn't
            // found, 126 when it couldn't be run, so callers can tell these
            // apart from the command itself failing.
            let code = match &e {
                Error::Spawn { source, .. } if source.kind() == ErrorKind::NotFound => 127,
                Error::Spawn { .. } => 126,
                _ => 1,
            };
            std::process::exit(code);
        }
    }
}
