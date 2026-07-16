//! Small shared helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time as epoch milliseconds (for rrweb/event timestamps).
pub fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Incrementally decodes a byte stream to UTF-8, buffering a trailing incomplete
/// multi-byte sequence across calls so each returned `String` is valid UTF-8.
/// Genuinely invalid bytes become U+FFFD, mirroring how a terminal renders them.
#[derive(Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    /// Decode as much of `bytes` (plus any buffered tail) as forms complete
    /// characters. An incomplete trailing sequence is held for the next call.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(
                        std::str::from_utf8(&self.pending[..valid]).expect("valid prefix"),
                    );
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid + n);
                        }
                        None => {
                            self.pending.drain(..valid);
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// Emit any leftover incomplete sequence (as replacement chars). Call at end
    /// of stream.
    pub fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let s = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_split_multibyte() {
        let mut d = Utf8Decoder::default();
        assert_eq!(d.push(&[0xC3]), ""); // first half of "é"
        assert_eq!(d.push(&[0xA9]), "é");
    }

    #[test]
    fn invalid_becomes_replacement() {
        let mut d = Utf8Decoder::default();
        assert_eq!(d.push(&[b'a', 0xFF, b'b']), "a\u{FFFD}b");
    }

    #[test]
    fn flush_emits_incomplete_tail() {
        let mut d = Utf8Decoder::default();
        assert_eq!(d.push(&[0xE2, 0x82]), ""); // incomplete "€"
        assert_eq!(d.flush(), "\u{FFFD}");
    }
}
