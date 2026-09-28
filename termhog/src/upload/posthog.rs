//! PostHog's wire format: the `/s/` snapshot batch, the `/i/v0/e/` analytics
//! event, and the replay deep link.
//!
//! A snapshot upload is a batch array whose single `$snapshot` event carries
//! the rrweb events under `properties.$snapshot_data`, gzipped whole and sent
//! as `Content-Type: text/plain`. FullSnapshot and mutation payloads are also
//! compressed one by one, as posthog-js does (`cv: "2024-10"`), which
//! PostHog's player decompresses.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Serialize;
use serde_json::{Value, json};
use ureq::Agent;
use uuid::Uuid;

use super::Config;
use super::spool::Batch;
use crate::render::rrweb::{Event, FULL_SNAPSHOT, INCREMENTAL_SNAPSHOT, MUTATION};

/// Sent as `$lib`, so replays can be filtered by `$lib = termhog`.
const LIB_NAME: &str = "termhog";
const LIB_VERSION: &str = env!("CARGO_PKG_VERSION");
/// posthog-js's compressed-event format version.
const COMPRESSION_VERSION: &str = "2024-10";

/// Why a POST failed.
pub struct PostError {
    pub message: String,
    /// The HTTP status, when the server answered with an error.
    pub status: Option<u16>,
}

impl PostError {
    /// The server rejected the request itself (a 4xx other than timeout or
    /// rate limiting), so retrying it won't help.
    pub fn permanent(&self) -> bool {
        self.status
            .is_some_and(|code| (400..500).contains(&code) && code != 408 && code != 429)
    }
}

/// Upload one batch of spooled events as a `$snapshot` event.
pub fn send_snapshots(client: &Agent, config: &Config, batch: &Batch) -> Result<(), PostError> {
    post(
        client,
        &endpoint(config, "/s/"),
        snapshot_body(config, batch),
    )
}

/// Send one analytics event that happened at `at` (epoch milliseconds),
/// tagged with the session ID so it links the recording to a person and the
/// replay list.
pub fn send_analytics(
    client: &Agent,
    config: &Config,
    event: &str,
    at: u64,
    mut properties: Value,
) -> Result<(), PostError> {
    if let Some(obj) = properties.as_object_mut() {
        obj.insert("$session_id".into(), json!(config.session_id));
        obj.insert("$lib".into(), json!(LIB_NAME));
        obj.insert("$lib_version".into(), json!(LIB_VERSION));
        obj.insert(
            "$process_person_profile".into(),
            json!(config.person_profile),
        );
    }
    let payload = json!({
        "api_key": config.api_key,
        "event": event,
        "distinct_id": config.distinct_id,
        "timestamp": at.to_string(),
        "properties": properties,
    });
    let body = gzip(&serde_json::to_vec(&payload).unwrap_or_default());
    post(client, &endpoint(config, "/i/v0/e/"), body)
}

/// The replay deep link, built as posthog-js does: the public token sits
/// where the numeric project ID would.
pub fn replay_url(ui_host: &str, token: &str, session_id: &str) -> String {
    format!("{ui_host}/project/{token}/replay/{session_id}")
}

/// A replay link that starts playback `secs` in.
pub fn replay_url_at(url: &str, secs: u64) -> String {
    format!("{url}?t={secs}")
}

/// A capture endpoint on the ingest host, with the query PostHog's SDKs send.
fn endpoint(config: &Config, path: &str) -> String {
    format!(
        "{}{path}?compression=gzip-js&ip=1&ver={LIB_NAME}/{LIB_VERSION}",
        config.ingest_host
    )
}

/// POST a body. ureq reports non-2xx statuses as errors by default.
fn post(client: &Agent, url: &str, body: Vec<u8>) -> Result<(), PostError> {
    client
        .post(url)
        .header("Content-Type", "text/plain")
        .send(&body[..])
        .map(|_| ())
        .map_err(|e| PostError {
            status: match e {
                ureq::Error::StatusCode(code) => Some(code),
                _ => None,
            },
            message: e.to_string(),
        })
}

/// Build the gzipped `/s/` request body for a batch of spooled events: a
/// batch array holding one `$snapshot` event that carries them all.
fn snapshot_body(config: &Config, batch: &Batch) -> Vec<u8> {
    let body = json!([{
        "api_key": config.api_key,
        "event": "$snapshot",
        "uuid": Uuid::new_v4().to_string(),
        "timestamp": batch.first_timestamp.to_string(),
        "distinct_id": config.distinct_id,
        "properties": {
            "$snapshot_data": batch.events,
            // The serialized size of `$snapshot_data`: the events, the commas
            // between them, and the brackets around them.
            "$snapshot_bytes": batch.bytes + batch.events.len().saturating_sub(1) + 2,
            "$session_id": config.session_id,
            "$snapshot_source": "web",
            "$lib": LIB_NAME,
            "$lib_version": LIB_VERSION,
            "$process_person_profile": config.person_profile,
            "distinct_id": config.distinct_id,
        },
    }]);
    gzip(&serde_json::to_vec(&body).unwrap_or_default())
}

/// Compress an event's bulky payload the way posthog-js does: a FullSnapshot's
/// `data`, or a mutation's `adds`/`removes`/`texts`/`attributes`, each become
/// gzipped JSON stored as a string of byte-valued characters, and the event
/// is marked with `cv`. Other events are left as they are.
pub fn compress_event(event: Event) -> Value {
    match event {
        Event::Mutation {
            mutation,
            timestamp,
        } => json!({
            "type": INCREMENTAL_SNAPSHOT,
            "data": {
                "source": MUTATION,
                "texts": gzip_chars(&mutation.texts),
                "attributes": gzip_chars(&mutation.attributes),
                "removes": gzip_chars(&mutation.removes),
                "adds": gzip_chars(&mutation.adds),
            },
            "cv": COMPRESSION_VERSION,
            "timestamp": timestamp,
        }),
        Event::Other(mut event) if event["type"] == FULL_SNAPSHOT => {
            let data = event["data"].take();
            event["data"] = Value::String(gzip_chars(&data));
            event["cv"] = json!(COMPRESSION_VERSION);
            event
        }
        Event::Other(event) => event,
    }
}

/// Gzipped JSON as a string with one character per byte (U+0000 to U+00FF),
/// the encoding PostHog's player reads back.
fn gzip_chars(value: &impl Serialize) -> String {
    gzip(&serde_json::to_vec(value).unwrap_or_default())
        .into_iter()
        .map(char::from)
        .collect()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::GzDecoder;

    use super::*;
    use crate::upload::spool::Spool;

    fn gunzip(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        GzDecoder::new(bytes).read_to_end(&mut out).unwrap();
        out
    }

    fn config() -> Config {
        Config {
            ingest_host: "https://example.com".into(),
            api_key: "phc_x".into(),
            distinct_id: "person".into(),
            person_profile: false,
            session_id: "sid".into(),
            command: "true".into(),
            started_at: 0,
            reporter: None,
        }
    }

    #[test]
    fn compressed_events_decode_like_posthog_does() {
        let snapshot = json!({ "type": 2, "data": { "node": { "id": 1 } }, "timestamp": 5 });
        let event = compress_event(Event::Other(snapshot));
        assert_eq!(event["cv"], COMPRESSION_VERSION);
        // PostHog maps each character back to one byte, then gunzips.
        let bytes: Vec<u8> = event["data"]
            .as_str()
            .unwrap()
            .chars()
            .map(|c| c as u8)
            .collect();
        let data: Value = serde_json::from_slice(&gunzip(&bytes)).unwrap();
        assert_eq!(data, json!({ "node": { "id": 1 } }));

        // Events other than snapshots and mutations are left alone.
        let meta = json!({ "type": 4, "data": { "width": 1 }, "timestamp": 5 });
        assert_eq!(compress_event(Event::Other(meta.clone())), meta);
    }

    #[test]
    fn snapshot_body_is_one_valid_snapshot_event() {
        let mut spool = Spool::new();
        spool.push(&json!({ "type": 4, "timestamp": 7 })).unwrap();
        spool.push(&json!({ "type": 3, "timestamp": 8 })).unwrap();
        let batch = spool.peek(usize::MAX).unwrap().unwrap();
        let body: Value =
            serde_json::from_slice(&gunzip(&snapshot_body(&config(), &batch))).unwrap();
        let event = &body[0];
        assert_eq!(event["event"], "$snapshot");
        assert_eq!(event["timestamp"], "7");
        let props = &event["properties"];
        assert_eq!(props["$snapshot_data"].as_array().unwrap().len(), 2);
        assert_eq!(props["$snapshot_data"][1]["type"], 3);
        assert_eq!(props["$session_id"], "sid");
        let data = serde_json::to_vec(&props["$snapshot_data"]).unwrap();
        assert_eq!(props["$snapshot_bytes"], data.len());
    }
}
