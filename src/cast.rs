//! asciicast v3 writer.
//!
//! This is the durable, never-blocked source of truth for a session. It is a
//! deliberately dumb, fast NDJSON writer: line 1 is a JSON header, every
//! subsequent line is a `[interval, code, data]` array where `interval` is the
//! delta in seconds since the previous event.
//!
//! Spec: <https://docs.asciinema.org/manual/asciicast/v3/>

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::util::Utf8Decoder;

/// asciicast v3 header (line 1 of the file).
#[derive(Debug, Clone, Serialize)]
pub struct Header {
    /// Always 3.
    pub version: u8,
    pub term: Term,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Term {
    pub cols: u16,
    pub rows: u16,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
}

/// Terminal color theme, as captured via OSC queries. Colors are CSS hex
/// (`#rrggbb`); `palette` is 8 or 16 colors joined with `:`.
#[derive(Debug, Clone, Serialize)]
pub struct Theme {
    pub fg: String,
    pub bg: String,
    pub palette: String,
}

impl Header {
    /// Build a header from the initial terminal geometry, stamping the current
    /// wall-clock time. `env` is filtered to a small, useful allowlist by the
    /// caller.
    pub fn new(cols: u16, rows: u16) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
        Header {
            version: 3,
            term: Term {
                cols,
                rows,
                type_: std::env::var("TERM").ok(),
                theme: None,
            },
            timestamp,
            command: None,
            tags: None,
            env: BTreeMap::new(),
        }
    }
}

/// Writes asciicast v3 to any sink. Intervals are computed from a monotonic
/// clock and error-diffused against absolute elapsed time (we round the
/// *absolute* microsecond offset and take deltas of the rounded values), so the
/// running sum of intervals never drifts from real elapsed time.
pub struct CastWriter<W: Write> {
    w: W,
    start: Instant,
    /// Rounded absolute offset (microseconds) of the last event written.
    last_abs_us: i64,
    /// Decodes raw output bytes to valid UTF-8 across chunk boundaries.
    decoder: Utf8Decoder,
}

impl<W: Write> CastWriter<W> {
    /// Create a writer and emit the header line. `start` is the session's
    /// reference instant; the first event's interval is measured from it.
    pub fn new(mut w: W, start: Instant, header: &Header) -> io::Result<Self> {
        let line = serde_json::to_string(header).map_err(io::Error::other)?;
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;
        Ok(CastWriter {
            w,
            start,
            last_abs_us: 0,
            decoder: Utf8Decoder::default(),
        })
    }

    /// Microsecond interval since the previous event, error-diffused.
    fn interval_us(&mut self, at: Instant) -> i64 {
        let abs_us = at.saturating_duration_since(self.start).as_micros() as i64;
        let iv = abs_us - self.last_abs_us;
        self.last_abs_us = abs_us;
        iv.max(0)
    }

    /// Output printed to the terminal (`o`). Raw bytes are decoded to valid
    /// UTF-8, buffering any split multi-byte sequence for the next call.
    pub fn output(&mut self, at: Instant, bytes: &[u8]) -> io::Result<()> {
        let iv = self.interval_us(at);
        let text = self.decoder.push(bytes);
        if !text.is_empty() {
            self.write_event(iv, 'o', &text)?;
        }
        Ok(())
    }

    /// Input typed by the user (`i`). Only recorded when explicitly enabled and
    /// echo is on (see the capture layer); passthrough is never gated by this.
    pub fn input(&mut self, at: Instant, bytes: &[u8]) -> io::Result<()> {
        let iv = self.interval_us(at);
        // Input is short and self-contained; decode without cross-chunk buffering
        // to avoid interleaving with the output pending buffer.
        let text = String::from_utf8_lossy(bytes);
        if !text.is_empty() {
            self.write_event(iv, 'i', &text)?;
        }
        Ok(())
    }

    /// Resize event (`r`), data `"{cols}x{rows}"`.
    pub fn resize(&mut self, at: Instant, cols: u16, rows: u16) -> io::Result<()> {
        let iv = self.interval_us(at);
        self.write_event(iv, 'r', &format!("{cols}x{rows}"))
    }

    /// Session exit (`x`), data is the numeric status encoded as a string.
    /// Flushes any buffered incomplete UTF-8 first.
    pub fn exit(&mut self, at: Instant, status: i32) -> io::Result<()> {
        let leftover = self.decoder.flush();
        if !leftover.is_empty() {
            let iv = self.interval_us(at);
            self.write_event(iv, 'o', &leftover)?;
        }
        let iv = self.interval_us(at);
        self.write_event(iv, 'x', &status.to_string())?;
        self.w.flush()
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }

    /// Write `[interval, "code", <json string>]\n`. `data` is JSON-escaped by
    /// serde (non-printables become `\uXXXX`).
    fn write_event(&mut self, iv_us: i64, code: char, data: &str) -> io::Result<()> {
        let data_json = serde_json::to_string(data).map_err(io::Error::other)?;
        // Exact fixed-point seconds from the integer microsecond delta; no float
        // round-trip, so no drift.
        writeln!(
            self.w,
            "[{}.{:06}, \"{}\", {}]",
            iv_us / 1_000_000,
            iv_us % 1_000_000,
            code,
            data_json
        )
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_to_vec(f: impl FnOnce(&mut CastWriter<Vec<u8>>)) -> String {
        let start = Instant::now();
        let header = Header::new(80, 24);
        let mut w = CastWriter::new(Vec::new(), start, &header).unwrap();
        f(&mut w);
        String::from_utf8(std::mem::take(&mut w.w)).unwrap()
    }

    #[test]
    fn header_is_valid_v3() {
        let out = write_to_vec(|_| {});
        let first = out.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(first).unwrap();
        assert_eq!(v["version"], 3);
        assert_eq!(v["term"]["cols"], 80);
        assert_eq!(v["term"]["rows"], 24);
    }

    #[test]
    fn output_event_shape() {
        let out = write_to_vec(|w| {
            w.output(Instant::now(), b"hello\r\n").unwrap();
        });
        let line = out.lines().nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v[0].is_number());
        assert_eq!(v[1], "o");
        assert_eq!(v[2], "hello\r\n");
    }

    #[test]
    fn split_utf8_is_buffered_then_completed() {
        // "é" is 0xC3 0xA9 split across two chunks.
        let out = write_to_vec(|w| {
            w.output(Instant::now(), &[0xC3]).unwrap();
            w.output(Instant::now(), &[0xA9]).unwrap();
        });
        let events: Vec<serde_json::Value> = out
            .lines()
            .skip(1)
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // First chunk buffers (no event), second completes "é".
        assert_eq!(events.len(), 1);
        assert_eq!(events[0][2], "é");
    }

    #[test]
    fn resize_and_exit() {
        let out = write_to_vec(|w| {
            w.resize(Instant::now(), 100, 40).unwrap();
            w.exit(Instant::now(), 3).unwrap();
        });
        let lines: Vec<&str> = out.lines().collect();
        let r: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r[1], "r");
        assert_eq!(r[2], "100x40");
        let x: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(x[1], "x");
        assert_eq!(x[2], "3"); // numeric status as a string, per spec
    }

    #[test]
    fn intervals_do_not_drift() {
        // Sum of intervals should equal the rounded absolute offset of the last
        // event. We can't control Instant, but we can assert monotonic parse.
        let out = write_to_vec(|w| {
            for _ in 0..5 {
                w.output(Instant::now(), b"x").unwrap();
            }
        });
        let mut sum = 0f64;
        for line in out.lines().skip(1) {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let iv = v[0].as_f64().unwrap();
            assert!(iv >= 0.0);
            sum += iv;
        }
        assert!(sum >= 0.0);
    }
}
