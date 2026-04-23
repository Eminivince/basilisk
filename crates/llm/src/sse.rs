//! Minimal SSE (server-sent-events) decoder, tailored to Anthropic.
//!
//! SSE is simple enough to hand-parse and doing so keeps the dependency
//! footprint small. The decoder is a pure byte→frame state machine —
//! feed it chunks from a streaming HTTP body, get back whatever frames
//! finished inside those chunks, leave the remainder in the buffer for
//! the next call.
//!
//! Format (per the SSE spec + Anthropic's usage):
//!
//! ```text
//! event: message_start
//! data: {"type":"message_start",...}
//!
//! event: content_block_delta
//! data: {"type":"content_block_delta","index":0,...}
//! ```
//!
//! Each frame is delimited by a blank line (`\n\n`). Lines starting with
//! `:` are comments and ignored. We collect `event:` and `data:` values
//! per frame; fields we don't recognise are dropped.

use crate::error::LlmError;

/// One decoded SSE frame before JSON parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Streaming SSE decoder. Not thread-safe; owned by a single reader.
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append raw bytes and return every complete frame the buffer now
    /// contains. Incomplete trailing content remains buffered for the
    /// next call.
    ///
    /// Invalid UTF-8 in a frame body produces [`LlmError::ParseError`].
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, LlmError> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();

        loop {
            let Some(end) = find_delimiter(&self.buf) else {
                break;
            };
            let (head, tail) = self.buf.split_at(end.frame_end);
            let frame_bytes = head.to_vec();
            let rest = tail[end.delim_len..].to_vec();
            self.buf = rest;

            let text = std::str::from_utf8(&frame_bytes)
                .map_err(|e| LlmError::ParseError(format!("non-utf8 in SSE frame: {e}")))?;
            if let Some(frame) = parse_frame(text) {
                out.push(frame);
            }
        }
        Ok(out)
    }

    /// Bytes left in the buffer — useful only for tests and diagnostics.
    #[cfg(test)]
    pub fn remaining(&self) -> &[u8] {
        &self.buf
    }
}

/// Location of the blank-line delimiter that closes one frame.
///
/// SSE accepts `\n\n` and `\r\n\r\n`; we normalize both to a single
/// "end of frame" index plus the delimiter length so callers can slice.
struct DelimHit {
    frame_end: usize,
    delim_len: usize,
}

fn find_delimiter(buf: &[u8]) -> Option<DelimHit> {
    // Scan for the earliest of `\n\n` or `\r\n\r\n`.
    for i in 0..buf.len() {
        if buf[i] == b'\n' {
            if i + 1 < buf.len() && buf[i + 1] == b'\n' {
                return Some(DelimHit {
                    frame_end: i,
                    delim_len: 2,
                });
            }
            if i >= 1
                && buf[i - 1] == b'\r'
                && i + 2 < buf.len()
                && buf[i + 1] == b'\r'
                && buf[i + 2] == b'\n'
            {
                return Some(DelimHit {
                    frame_end: i - 1,
                    delim_len: 4,
                });
            }
        }
    }
    None
}

/// Parse one fully-buffered frame body. Returns `None` for frames that
/// carry no `data:` field (e.g. heartbeat comment-only blocks).
fn parse_frame(text: &str) -> Option<SseFrame> {
    let mut event: Option<String> = None;
    let mut data_parts: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            // SSE spec: strip a single leading space if present.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            data_parts.push(rest);
        }
        // Unknown fields (id:, retry:) are silently ignored.
    }
    if data_parts.is_empty() {
        return None;
    }
    Some(SseFrame {
        event,
        data: data_parts.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_single_frame() {
        let mut d = SseDecoder::new();
        let frames = d.push_bytes(b"event: hello\ndata: {\"k\":1}\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("hello"));
        assert_eq!(frames[0].data, "{\"k\":1}");
        assert!(d.remaining().is_empty());
    }

    #[test]
    fn decodes_two_frames_in_one_chunk() {
        let mut d = SseDecoder::new();
        let frames = d
            .push_bytes(b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        assert_eq!(frames[1].event.as_deref(), Some("b"));
    }

    #[test]
    fn buffers_partial_frame_across_chunks() {
        let mut d = SseDecoder::new();
        let empty = d.push_bytes(b"event: hi\ndata: one").unwrap();
        assert!(empty.is_empty());
        let frames = d.push_bytes(b"\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "one");
    }

    #[test]
    fn supports_crlf_delimiters() {
        let mut d = SseDecoder::new();
        let frames = d.push_bytes(b"event: hi\r\ndata: one\r\n\r\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "one");
    }

    #[test]
    fn ignores_comment_lines() {
        let mut d = SseDecoder::new();
        let frames = d
            .push_bytes(b": this is a comment\nevent: e\ndata: v\n\n")
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("e"));
    }

    #[test]
    fn concatenates_multiline_data() {
        let mut d = SseDecoder::new();
        let frames = d
            .push_bytes(b"event: e\ndata: line1\ndata: line2\n\n")
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "line1\nline2");
    }

    #[test]
    fn data_without_event_still_yields_frame() {
        let mut d = SseDecoder::new();
        let frames = d.push_bytes(b"data: bare\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].event.is_none());
        assert_eq!(frames[0].data, "bare");
    }

    #[test]
    fn frame_with_no_data_is_dropped() {
        // Comment-only / heartbeat frames.
        let mut d = SseDecoder::new();
        let frames = d.push_bytes(b": just a ping\n\n").unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn strips_single_leading_space_after_colon() {
        let mut d = SseDecoder::new();
        let frames = d.push_bytes(b"data: value\n\n").unwrap();
        assert_eq!(frames[0].data, "value");
        let mut d = SseDecoder::new();
        let frames = d.push_bytes(b"data:value\n\n").unwrap();
        assert_eq!(frames[0].data, "value");
        let mut d = SseDecoder::new();
        let frames = d.push_bytes(b"data:  two-space\n\n").unwrap();
        // Only one leading space is stripped per the SSE spec.
        assert_eq!(frames[0].data, " two-space");
    }

    #[test]
    fn byte_by_byte_feed_assembles_frame() {
        let mut d = SseDecoder::new();
        let raw = b"event: e\ndata: {\"k\":1}\n\n";
        let mut total = Vec::new();
        for byte in raw {
            let frames = d.push_bytes(&[*byte]).unwrap();
            total.extend(frames);
        }
        assert_eq!(total.len(), 1);
        assert_eq!(total[0].data, "{\"k\":1}");
    }
}
