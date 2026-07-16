use std::sync::{Arc, Mutex};

use clap::Parser;
use ph_capture::{Diagnostic, Reporter, config_from_env, run};

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
    /// Record user keystrokes as input events (echo-gated).
    #[arg(long)]
    record_input: bool,

    /// The command to run, e.g. `ph-capture -- nvim`.
    #[arg(trailing_var_arg = true, required = true, num_args = 1.., value_name = "COMMAND")]
    command: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    let mut config = match config_from_env(cli.command) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("ph-capture: {e}");
            std::process::exit(2);
        }
    };
    config.record_input |= cli.record_input;

    // Buffer diagnostics; flush after run() returns (terminal restored).
    let reporter = Arc::new(CollectingReporter::default());
    config.reporter = Some(reporter.clone());

    let outcome = run(config);

    // Everything below prints only after the session has fully torn down.
    if let Ok(messages) = reporter.messages.lock() {
        for message in messages.iter() {
            eprintln!("ph-capture: {message}");
        }
    }

    match outcome {
        Ok(outcome) => {
            if let Some(path) = &outcome.cast_path {
                eprintln!("ph-capture: debug cast written to {}", path.display());
            }
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
