//! The checkpoint format: everything needed to finish a recording elsewhere,
//! as JSON Lines, one [`Line`] per line, each with a `type`.
//!
//! - `checkpoint`, first: the format version, who the recording belongs to,
//!   whether `term_start` went out, and how the command ended.
//! - `event`: an event waiting to upload, oldest first.
//! - Then, when rendering is unfinished, its saved state (see
//!   [`render::Saved`]).
//!
//! A reader skips lines of types it doesn't know, so later versions can add
//! them freely. Only a change older readers would get wrong bumps
//! [`FORMAT`], and a checkpoint of a newer format is left for the newer
//! version to finish.

use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::render::{self, Remainder, Resumer};
use crate::upload::spool::Spool;
use crate::upload::{self, Config, Ending, Leftovers};
use crate::util::write_json_line;

/// The format version this code writes and reads.
const FORMAT: u64 = 1;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Line {
    Checkpoint(Header),
    Event {
        event: Value,
    },
    #[serde(untagged)]
    Render(render::Saved),
}

#[derive(Serialize, Deserialize)]
struct Header {
    format: u64,
    config: Config,
    started: bool,
    ending: Option<Ending>,
}

/// Write a checkpoint of `leftovers`, and of `render` if it's unfinished.
pub fn write(
    out: &mut impl Write,
    config: &Config,
    leftovers: &mut Leftovers,
    render: Option<Remainder>,
) -> io::Result<()> {
    let header = Header {
        format: FORMAT,
        config: config.clone(),
        started: leftovers.started,
        ending: leftovers.ending,
    };
    write_json_line(out, &Line::Checkpoint(header))?;
    // Each event is spliced in as spooled, since parsing them all again
    // would slow down the exit this runs in.
    leftovers.spool.for_each(|event| {
        out.write_all(br#"{"type":"event","event":"#)?;
        out.write_all(event)?;
        out.write_all(b"}\n")
    })?;
    if let Some(render) = render {
        render.write_lines(out)?;
    }
    out.flush()
}

/// A checkpoint read back, with any unfinished rendering done.
pub struct Loaded {
    pub config: Config,
    /// Every event to upload, including the ones just rendered.
    pub leftovers: Leftovers,
    /// Rendering was finished here, so the checkpoint should be saved again
    /// before uploading, to never render twice.
    pub rendered: bool,
}

/// Read a checkpoint, finishing its rendering along the way. Lines that are
/// damaged, or of types this version doesn't know, are skipped. `None` if
/// the checkpoint is damaged, or of a newer format.
pub fn load(input: impl BufRead) -> Option<Loaded> {
    let mut lines = input.lines();
    let Line::Checkpoint(header) = serde_json::from_str(&lines.next()?.ok()?).ok()? else {
        return None;
    };
    if header.format > FORMAT {
        return None;
    }
    // Rendered events go after the saved ones, into the same spool.
    let spool = Arc::new(Mutex::new(Spool::new().ok()?));
    let mut resumer = Resumer::new(spool_sink(&spool));
    for line in lines {
        match serde_json::from_str(&line.ok()?) {
            Ok(Line::Event { event }) => lock(&spool).push(&event).ok()?,
            Ok(Line::Render(line)) => resumer.apply(line),
            _ => {}
        }
    }
    let rendered = resumer.finish();
    // The resumer, and with it the sink's handle, is gone.
    let spool = Arc::into_inner(spool)?
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    let leftovers = Leftovers {
        spool,
        started: header.started,
        ending: header.ending,
    };
    Some(Loaded {
        config: header.config,
        leftovers,
        rendered,
    })
}

/// A sink adding rendered events to `spool`.
fn spool_sink(spool: &Arc<Mutex<Spool>>) -> render::Sink {
    let spool = Arc::clone(spool);
    Box::new(move |event| {
        let _ = upload::spool_event(&mut lock(&spool), event);
    })
}

fn lock(spool: &Mutex<Spool>) -> MutexGuard<'_, Spool> {
    spool.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn round_trips_and_skips_what_it_does_not_know() {
        let config = Config {
            ingest_host: "https://example.com".into(),
            api_key: "phc_x".into(),
            distinct_id: "person".into(),
            person_profile: false,
            session_id: "sid".into(),
            command: "true".into(),
            started_at: 1,
            reporter: None,
        };
        let mut spool = Spool::new().unwrap();
        spool.push(&json!({ "type": 4, "timestamp": 7 })).unwrap();
        let mut leftovers = Leftovers::new(spool);
        leftovers.ending = Some(Ending {
            exit_code: 3,
            duration_ms: 5,
            at: 9,
        });
        let mut out = Vec::new();
        write(&mut out, &config, &mut leftovers, None).unwrap();
        // A line from a newer version, and a render line without its screen.
        out.extend_from_slice(b"{\"type\":\"future\",\"x\":1}\n");
        out.extend_from_slice(b"{\"type\":\"input\",\"at\":8}\n");

        let loaded = load(&out[..]).unwrap();
        assert_eq!(loaded.config.session_id, "sid");
        assert!(!loaded.rendered);
        assert_eq!(loaded.leftovers.ending.unwrap().exit_code, 3);
        let mut spool = loaded.leftovers.spool;
        let batch = spool.peek(usize::MAX).unwrap().unwrap();
        assert_eq!(batch.events, [json!({ "type": 4, "timestamp": 7 })]);

        // A newer format is left alone.
        let newer = String::from_utf8(out)
            .unwrap()
            .replacen("\"format\":1", "\"format\":2", 1);
        assert!(load(newer.as_bytes()).is_none());
    }
}
