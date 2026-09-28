//! Streams rrweb events to PostHog's session-replay endpoint.
//!
//! Two threads keep the network from ever stalling the terminal or the
//! projection. The intake thread compresses each event and appends it to a
//! [`Spool`] on disk. The upload thread sends what's pending as `$snapshot`
//! batches every [`FLUSH_INTERVAL`], or sooner once a full batch is waiting,
//! retrying with backoff while the network is down. It also sends
//! `term_start`/`term_end` analytics events so the recording is
//! person-linked and shows in the replay list.
//!
//! When the recording ends before everything is sent, what's left can be
//! handed off ([`Msg::HandOff`]) and finished elsewhere with [`drain`].

mod posthog;
pub mod spool;

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ureq::Agent;

use crate::render::rrweb::Event;
use crate::util::spawn_recorder;
use crate::{Diagnostic, Reporter};
pub use posthog::{replay_url, replay_url_at};
use spool::{Batch, Spool};

/// Upload at least this often during a live session.
const FLUSH_INTERVAL: Duration = Duration::from_millis(1500);
/// Keep each upload under this, below PostHog's ~1 MB per-event limit on the
/// `$snapshot` event a batch becomes.
const MAX_BATCH_BYTES: usize = 800 * 1024;
/// Retry backoff after a failed upload, doubling up to the maximum.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// Who the recording belongs to and where it goes.
#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub ingest_host: String,
    pub api_key: String,
    pub distinct_id: String,
    /// Whether PostHog should keep a person profile for `distinct_id`: only
    /// for a caller-given person, as posthog-js does for identified users.
    /// Otherwise every anonymous run would create a new person.
    pub person_profile: bool,
    pub session_id: String,
    pub command: String,
    /// When the recording started, in epoch milliseconds.
    pub started_at: u64,
    /// Where non-fatal upload problems are reported, if set. Nobody would
    /// see the problems of an upload finished elsewhere, so it isn't saved.
    #[serde(skip)]
    pub reporter: Option<Arc<dyn Reporter>>,
}

impl Config {
    fn report(&self, diagnostic: Diagnostic) {
        if let Some(reporter) = &self.reporter {
            reporter.report(&diagnostic);
        }
    }

    /// Reports a recorder thread's panic (see [`spawn_recorder`]).
    pub fn on_panic(&self) -> impl FnOnce(String) + Send + 'static {
        let reporter = self.reporter.clone();
        move |message| {
            if let Some(reporter) = reporter {
                reporter.report(&Diagnostic::Recorder(message));
            }
        }
    }
}

/// How the recorded command ended, for `term_end`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Ending {
    pub exit_code: i32,
    pub duration_ms: u64,
    /// When it ended, in epoch milliseconds.
    pub at: u64,
}

pub enum Msg {
    /// One rrweb event to stream.
    Event(Event),
    /// The recording ended: send everything, then `term_end`.
    End(Ending),
    /// Stop uploading, and send back what's still unsent.
    HandOff(Sender<Leftovers>),
}

/// What's still to be uploaded.
pub struct Leftovers {
    /// Events not yet uploaded, oldest first.
    pub spool: Spool,
    /// Whether `term_start` went out.
    pub started: bool,
    /// How the command ended, once known. `term_end` is still to be sent.
    pub ending: Option<Ending>,
}

impl Leftovers {
    /// The events in `spool`, with nothing sent yet.
    pub fn new(spool: Spool) -> Leftovers {
        Leftovers {
            spool,
            started: false,
            ending: None,
        }
    }
}

/// State shared by the intake and upload threads.
struct Shared {
    state: Mutex<State>,
    /// Signalled when a full batch is waiting, the recording ends, or
    /// uploading stops.
    wake: Condvar,
}

struct State {
    /// What's still to upload. `None` once uploading stopped: everything
    /// was sent, or the rest was handed off.
    work: Option<Leftovers>,
    /// No more events are coming: finish once the spool is empty.
    ended: bool,
}

impl Shared {
    fn new(work: Leftovers, ended: bool) -> Shared {
        Shared {
            state: Mutex::new(State {
                work: Some(work),
                ended,
            }),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stop uploading and take what's unsent, if anything. An upload in
    /// flight may still land, so its events can arrive twice, but none are
    /// lost.
    fn stop(&self) -> Option<Leftovers> {
        let work = self.lock().work.take();
        self.wake.notify_all();
        work
    }
}

/// Start uploading a recording's events. The returned thread ends once
/// everything is uploaded, or once it's handed off.
pub fn start(config: Config) -> (Sender<Msg>, JoinHandle<Option<()>>) {
    let config = Arc::new(config);
    let shared = Arc::new(Shared::new(Leftovers::new(Spool::new()), false));
    let (tx, rx) = unbounded();
    {
        let (config, shared) = (Arc::clone(&config), Arc::clone(&shared));
        let on_panic = config.on_panic();
        spawn_recorder(move || intake(&config, &shared, rx), on_panic);
    }
    let on_panic = config.on_panic();
    let handle = spawn_recorder(
        move || {
            upload_loop(&config, &shared, None);
        },
        on_panic,
    );
    (tx, handle)
}

/// Add an rrweb event to `spool`, compressed and ready to upload.
pub fn spool_event(spool: &mut Spool, event: Event) -> std::io::Result<()> {
    spool.push(&posthog::compress_event(event))
}

/// Upload handed-off events, retrying until `give_up_at`. Returns what's
/// still unsent if it gives up.
pub fn drain(config: &Config, leftovers: Leftovers, give_up_at: Instant) -> Option<Leftovers> {
    let shared = Shared::new(leftovers, true);
    upload_loop(config, &shared, Some(give_up_at));
    shared.stop()
}

/// Compress and spool events as they come, until the recording ends or is
/// handed off.
fn intake(config: &Config, shared: &Shared, rx: Receiver<Msg>) {
    for msg in rx {
        match msg {
            Msg::Event(event) => {
                let event = posthog::compress_event(event);
                let mut state = shared.lock();
                let Some(work) = &mut state.work else {
                    continue;
                };
                if let Err(e) = work.spool.push(&event) {
                    config.report(Diagnostic::Queue(e.to_string()));
                }
                if work.spool.pending() >= MAX_BATCH_BYTES as u64 {
                    shared.wake.notify_all();
                }
            }
            Msg::End(ending) => {
                let mut state = shared.lock();
                if let Some(work) = &mut state.work {
                    work.ending = Some(ending);
                }
                state.ended = true;
                shared.wake.notify_all();
            }
            Msg::HandOff(reply) => {
                if let Some(leftovers) = shared.stop() {
                    let _ = reply.send(leftovers);
                }
                return;
            }
        }
    }
    // Every sender is gone without an ending (the recording was dropped):
    // upload what there is.
    shared.lock().ended = true;
    shared.wake.notify_all();
}

/// Upload spooled events until the recording has ended and everything is
/// sent, or until it's handed off or `give_up_at` passes. Failures are
/// retried with backoff.
fn upload_loop(config: &Config, shared: &Shared, give_up_at: Option<Instant>) {
    let client: Agent = Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let expired = || give_up_at.is_some_and(|t| Instant::now() >= t);
    let mut started = shared.lock().work.as_ref().is_none_or(|w| w.started);

    let mut retry = RETRY_MIN;
    let mut next_attempt = Instant::now();
    let ending = loop {
        let batch = {
            let mut state = shared.lock();
            // Sleep until the next flush, unless a full batch is waiting or
            // the recording ends. A failed upload waits out its backoff here.
            let deadline = next_attempt.max(Instant::now() + FLUSH_INTERVAL);
            loop {
                let Some(work) = &state.work else {
                    return;
                };
                if expired() {
                    return;
                }
                let due = state.ended || work.spool.pending() >= MAX_BATCH_BYTES as u64;
                let until = if due { next_attempt } else { deadline };
                let now = Instant::now();
                if now >= until {
                    break;
                }
                let until = give_up_at.map_or(until, |t| until.min(t));
                state = shared
                    .wake
                    .wait_timeout(state, until.saturating_duration_since(now))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            let peeked = match &mut state.work {
                Some(work) => work.spool.peek(MAX_BATCH_BYTES),
                None => return,
            };
            match peeked {
                Ok(Some(batch)) => batch,
                // Everything's sent. Stopping now leaves a hand-off nothing
                // to take, so `term_end` is sent from here only.
                Ok(None) if state.ended => break state.work.take().and_then(|w| w.ending),
                Ok(None) => continue,
                Err(e) => {
                    config.report(Diagnostic::Queue(e.to_string()));
                    return;
                }
            }
        };

        // Tried with every batch until it goes out.
        started = started || send_start(&client, config, shared);
        match posthog::send_snapshots(&client, config, &batch) {
            Ok(()) => {
                commit(shared, &batch);
                retry = RETRY_MIN;
                next_attempt = Instant::now();
            }
            // A permanent rejection (bad key, malformed) won't succeed later.
            Err(e) if e.permanent() => {
                config.report(upload_failed(e));
                config.report(Diagnostic::Dropped {
                    count: batch.events.len() as u32,
                });
                commit(shared, &batch);
            }
            Err(e) => {
                config.report(upload_failed(e));
                next_attempt = Instant::now() + retry;
                retry = (retry * 2).min(RETRY_MAX);
            }
        }
    };

    if !started {
        send_start(&client, config, shared);
    }
    if let Some(ending) = ending {
        let properties = json!({
            "command": config.command,
            "exit_code": ending.exit_code,
            "duration_ms": ending.duration_ms,
        });
        analytics(&client, config, "term_end", ending.at, properties);
    }
}

/// Send `term_start`, which shows the recording in the replay list and links
/// a person, noting that it went out. Returns whether it did.
fn send_start(client: &Agent, config: &Config, shared: &Shared) -> bool {
    let properties = json!({ "command": config.command });
    let sent = analytics(client, config, "term_start", config.started_at, properties);
    if sent {
        if let Some(work) = &mut shared.lock().work {
            work.started = true;
        }
    }
    sent
}

/// Send an analytics event, reporting a failure. Returns whether it went out.
fn analytics(client: &Agent, config: &Config, event: &str, at: u64, properties: Value) -> bool {
    match posthog::send_analytics(client, config, event, at, properties) {
        Ok(()) => true,
        Err(e) => {
            config.report(Diagnostic::Analytics {
                event: event.to_string(),
                reason: e.message,
            });
            false
        }
    }
}

fn upload_failed(e: posthog::PostError) -> Diagnostic {
    Diagnostic::Upload {
        status: e.status,
        message: e.message,
    }
}

/// Remove an uploaded (or rejected) batch, unless the spool was handed off
/// meanwhile.
fn commit(shared: &Shared, batch: &Batch) {
    if let Some(work) = &mut shared.lock().work {
        work.spool.commit(batch);
    }
}
