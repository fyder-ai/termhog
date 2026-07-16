//! Streams rrweb events to PostHog's session-replay endpoint.
//!
//! Runs on its own thread and blocks on the network so the terminal and
//! projection are never stalled. Events are batched and flushed on a timer or
//! when a batch approaches the per-request size limit, then gzipped and POSTed
//! to `/s/`. It also sends `term_start`/`term_end` analytics events so the
//! recording is person-linked and shows in the replay list.
//!
//! Wire format is the one confirmed against PostHog's capture service and a live
//! upload: a batch array whose single `$snapshot` event carries the rrweb events
//! under `properties.$snapshot_data`, gzipped whole, `Content-Type: text/plain`.

use std::io::Write;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::util::epoch_ms;

/// Identifies this client to PostHog (`$lib`); metadata only, lets you filter
/// replays by `$lib = ph-capture`.
const LIB_NAME: &str = "ph-capture";
/// Flush at least this often during a live session.
const FLUSH_INTERVAL: Duration = Duration::from_millis(1500);
/// Flush before a batch crosses this, staying under PostHog's ~1 MB per-request
/// `RECORDING_MAX_EVENT_SIZE`.
const MAX_BATCH_BYTES: usize = 800 * 1024;
/// If the network is down, keep buffering up to this before dropping the oldest
/// events (bounded memory; the replay self-heals via the next keyframe).
const HARD_BUFFER_CAP: usize = 8 * 1024 * 1024;

pub struct Config {
    pub ingest_host: String,
    pub api_key: String,
    pub distinct_id: String,
    pub session_id: String,
    pub command: String,
    pub lib_version: String,
    /// Where non-fatal upload problems are reported, if set.
    pub reporter: Option<std::sync::Arc<dyn crate::Reporter>>,
}

impl Config {
    fn report(&self, diagnostic: crate::Diagnostic) {
        if let Some(reporter) = &self.reporter {
            reporter.report(&diagnostic);
        }
    }
}

pub enum Msg {
    /// One rrweb event to stream.
    Event(Value),
    /// End of session; flush and send `term_end`.
    Terminate { exit_code: i32, duration_ms: u64 },
}

/// Drain the channel, batching + shipping until it closes or a Terminate arrives.
pub fn run(config: Config, rx: Receiver<Msg>) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            config.report(crate::Diagnostic::ClientInit(e.to_string()));
            return;
        }
    };

    // Analytics event so the recording shows in the replay list + links a person.
    send_analytics(&client, &config, "term_start", json!({ "command": config.command }));

    let mut buf: Vec<Value> = Vec::new();
    let mut buf_bytes = 0usize;

    loop {
        match rx.recv_timeout(FLUSH_INTERVAL) {
            Ok(Msg::Event(ev)) => {
                let size = ev.to_string().len();
                if !buf.is_empty() && buf_bytes + size > MAX_BATCH_BYTES {
                    flush(&client, &config, &mut buf, &mut buf_bytes);
                }
                buf.push(ev);
                buf_bytes += size;
            }
            Ok(Msg::Terminate {
                exit_code,
                duration_ms,
            }) => {
                flush(&client, &config, &mut buf, &mut buf_bytes);
                send_analytics(
                    &client,
                    &config,
                    "term_end",
                    json!({
                        "command": config.command,
                        "exit_code": exit_code,
                        "duration_ms": duration_ms,
                    }),
                );
                break;
            }
            Err(RecvTimeoutError::Timeout) => flush(&client, &config, &mut buf, &mut buf_bytes),
            Err(RecvTimeoutError::Disconnected) => {
                flush(&client, &config, &mut buf, &mut buf_bytes);
                break;
            }
        }
    }
}

/// POST the buffered events as one `$snapshot` batch. On failure the buffer is
/// kept for retry (bounded by `HARD_BUFFER_CAP`), never silently dropped — rrweb
/// timestamps are set at capture time, so a late upload lands correctly.
fn flush(client: &reqwest::blocking::Client, config: &Config, buf: &mut Vec<Value>, buf_bytes: &mut usize) {
    if buf.is_empty() {
        return;
    }
    let url = format!(
        "{}/s/?compression=gzip-js&ip=1&ver={}/{}",
        config.ingest_host, LIB_NAME, config.lib_version
    );
    let body = snapshot_body(config, buf);
    match post(client, &url, &body) {
        Ok(()) => {
            buf.clear();
            *buf_bytes = 0;
        }
        Err(e) => {
            config.report(crate::Diagnostic::Upload(e));
            let mut dropped = 0u32;
            while *buf_bytes > HARD_BUFFER_CAP && !buf.is_empty() {
                let ev = buf.remove(0);
                *buf_bytes = buf_bytes.saturating_sub(ev.to_string().len());
                dropped += 1;
            }
            if dropped > 0 {
                config.report(crate::Diagnostic::Dropped { count: dropped });
            }
        }
    }
}

/// Build the gzipped `/s/` request body for a batch of rrweb events.
fn snapshot_body(config: &Config, events: &[Value]) -> Vec<u8> {
    let timestamp = events
        .first()
        .and_then(|e| e.get("timestamp"))
        .and_then(Value::as_u64)
        .unwrap_or_else(epoch_ms)
        .to_string();
    let snapshot_bytes = serde_json::to_vec(events).map(|v| v.len()).unwrap_or(0);

    let payload = json!([{
        "api_key": config.api_key,
        "event": "$snapshot",
        "uuid": Uuid::new_v4().to_string(),
        "timestamp": timestamp,
        "distinct_id": config.distinct_id,
        "properties": {
            "$snapshot_data": events,
            "$snapshot_bytes": snapshot_bytes,
            "$session_id": config.session_id,
            "$snapshot_source": "web",
            "$lib": LIB_NAME,
            "$lib_version": config.lib_version,
            "distinct_id": config.distinct_id,
        }
    }]);
    gzip(&serde_json::to_vec(&payload).unwrap_or_default())
}

/// Send one analytics event to `/i/v0/e/`, tagged with the session id so it
/// links the recording to a person and the replay list.
fn send_analytics(client: &reqwest::blocking::Client, config: &Config, event: &str, mut properties: Value) {
    if let Some(obj) = properties.as_object_mut() {
        obj.insert("$session_id".into(), json!(config.session_id));
        obj.insert("$lib".into(), json!(LIB_NAME));
        obj.insert("$lib_version".into(), json!(config.lib_version));
    }
    let payload = json!({
        "api_key": config.api_key,
        "event": event,
        "distinct_id": config.distinct_id,
        "timestamp": epoch_ms().to_string(),
        "properties": properties,
    });
    let url = format!(
        "{}/i/v0/e/?compression=gzip-js&ip=1&ver={}/{}",
        config.ingest_host, LIB_NAME, config.lib_version
    );
    let body = gzip(&serde_json::to_vec(&payload).unwrap_or_default());
    if let Err(e) = post(client, &url, &body) {
        config.report(crate::Diagnostic::Analytics {
            event: event.to_string(),
            reason: e,
        });
    }
}

fn post(client: &reqwest::blocking::Client, url: &str, body: &[u8]) -> Result<(), String> {
    let resp = client
        .post(url)
        .header("Content-Type", "text/plain")
        .body(body.to_vec())
        .send()
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

/// Construct the replay deep link, exactly as posthog-js does: the write-only
/// token sits in the URL path where the numeric project id would. `seek_secs`
/// adds a `?t=` to land near a point of interest (e.g. just before a failure).
pub fn replay_url(ui_host: &str, token: &str, session_id: &str, seek_secs: Option<u64>) -> String {
    let mut url = format!("{ui_host}/project/{token}/replay/{session_id}");
    if let Some(t) = seek_secs {
        url.push_str(&format!("?t={t}"));
    }
    url
}

/// Derive the UI/app host from the ingestion host. PostHog Cloud ingests on
/// `*.i.posthog.com` but serves the app on `*.posthog.com`; self-hosted uses one
/// host for both.
pub fn derive_ui_host(ingest_host: &str) -> String {
    ingest_host
        .replace("://us.i.posthog.com", "://us.posthog.com")
        .replace("://eu.i.posthog.com", "://eu.posthog.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_host_maps_cloud_ingest_to_app() {
        assert_eq!(derive_ui_host("https://us.i.posthog.com"), "https://us.posthog.com");
        assert_eq!(derive_ui_host("https://eu.i.posthog.com"), "https://eu.posthog.com");
        // self-hosted unchanged
        assert_eq!(derive_ui_host("https://ph.example.com"), "https://ph.example.com");
    }

    #[test]
    fn replay_url_shape() {
        assert_eq!(
            replay_url("https://us.posthog.com", "phc_x", "sid", None),
            "https://us.posthog.com/project/phc_x/replay/sid"
        );
        assert_eq!(
            replay_url("https://us.posthog.com", "phc_x", "sid", Some(42)),
            "https://us.posthog.com/project/phc_x/replay/sid?t=42"
        );
    }
}
