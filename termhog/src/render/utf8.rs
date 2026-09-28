//! Decoding output bytes to text across chunk boundaries.

/// Decodes a UTF-8 byte stream incrementally, holding a trailing incomplete
/// sequence for the next call so each returned `String` ends on a character
/// boundary. Invalid bytes become U+FFFD, as a terminal shows them.
#[derive(Default)]
pub struct Utf8Decoder {
    /// An incomplete trailing sequence, held for the next call.
    pending: Vec<u8>,
}

impl Utf8Decoder {
    /// Decode `bytes`, after any held tail, up to the last complete character.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        let joined;
        let bytes = if self.pending.is_empty() {
            bytes
        } else {
            self.pending.extend_from_slice(bytes);
            joined = std::mem::take(&mut self.pending);
            &joined
        };
        let mut out = String::with_capacity(bytes.len());
        let mut chunks = bytes.utf8_chunks().peekable();
        while let Some(chunk) = chunks.next() {
            out.push_str(chunk.valid());
            let invalid = chunk.invalid();
            // A sequence cut off at the very end may be finished by the next
            // read.
            let cut_off = chunks.peek().is_none()
                && std::str::from_utf8(invalid).is_err_and(|e| e.error_len().is_none());
            if cut_off {
                self.pending = invalid.to_vec();
            } else if !invalid.is_empty() {
                out.push('\u{FFFD}');
            }
        }
        out
    }

    /// Decode the held tail, if any, at the end of the stream.
    pub fn flush(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()
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
        // Invalid bytes are replaced, and decoding goes on after them.
        assert_eq!(d.push(&[b'a', 0xFF, b'b']), "a\u{FFFD}b");
    }
}
