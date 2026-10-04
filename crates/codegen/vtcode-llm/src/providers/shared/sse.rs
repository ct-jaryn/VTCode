//! Shared SSE byte-stream pump.
//!
//! Every provider stream repeats the same skeleton: accumulate network chunks,
//! split on SSE event boundaries, extract the `data:` payload, and dispatch it.
//! The byte-buffer bookkeeping — boundary search, UTF-8 slicing, offset
//! arithmetic, and compaction — lives here; provider loops keep their own
//! payload handling, including `yield`.

use std::borrow::Cow;

/// Advance past the next complete SSE event in `buf`, starting at `*offset`,
/// and return the raw event text.
///
/// Returns `None` when no complete event remains. The returned `&str` borrows
/// `buf`, so the caller must finish handling it before mutating the buffer —
/// then call [`drain_consumed_sse`] once per chunk to drop the consumed
/// prefix and keep the buffer bounded to the unprocessed tail.
///
/// `*offset` is advanced past the event before it is returned, matching the
/// historical behavior where parse failures and `[DONE]` breaks leave the
/// remaining tail buffered for the next chunk.
///
/// Invalid UTF-8 in the stream surfaces as [`std::str::Utf8Error`]; providers
/// either map it into their own error type or `expect` it away, matching
/// their historical behavior.
pub(crate) fn next_sse_event<'a>(buf: &'a [u8], offset: &mut usize) -> Result<Option<&'a str>, std::str::Utf8Error> {
    let Some((split_idx, delimiter_len)) = find_sse_boundary_bytes(buf, *offset) else {
        return Ok(None);
    };
    let event = std::str::from_utf8(&buf[*offset..split_idx])?;
    *offset = split_idx + delimiter_len;
    Ok(Some(event))
}

/// Drop the consumed prefix of `buf` so it stays bounded to the unprocessed
/// tail. Call once per chunk, after the [`next_sse_event`] loop ends.
pub(crate) fn drain_consumed_sse(buf: &mut Vec<u8>, offset: &mut usize) {
    if *offset > 0 {
        buf.drain(..*offset);
        *offset = 0;
    }
}

#[inline]
pub(crate) fn extract_data_payload<'a>(event: &'a str) -> Option<Cow<'a, str>> {
    // For the common single `data:` line case, return a borrowed slice to
    // avoid allocating a String per SSE event. Multi-line events are joined
    // with `\n` as before, requiring an owned String.
    let mut first: Option<&'a str> = None;
    let mut out = String::new();

    for raw_line in event.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        if let Some(value) = line.strip_prefix("data:") {
            let trimmed = value.trim_start();
            if let Some(first_val) = first {
                // Second+ data: line — join into the owned buffer.
                if out.is_empty() {
                    out.push_str(first_val);
                }
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(trimmed);
            } else {
                first = Some(trimmed);
            }
        }
    }

    if !out.is_empty() {
        return Some(Cow::Owned(out));
    }
    // Single data: line — return the borrowed slice (None if empty, matching
    // the original behaviour where an empty `out` yields `None`).
    first.filter(|s| !s.is_empty()).map(Cow::Borrowed)
}

#[inline]
pub(super) fn find_sse_boundary(buffer: &str) -> Option<(usize, usize)> {
    let newline_boundary = buffer.find("\n\n").map(|idx| (idx, 2));
    let carriage_boundary = buffer.find("\r\n\r\n").map(|idx| (idx, 4));

    match (newline_boundary, carriage_boundary) {
        (Some((n_idx, n_len)), Some((c_idx, c_len))) => {
            if n_idx <= c_idx {
                Some((n_idx, n_len))
            } else {
                Some((c_idx, c_len))
            }
        }
        (Some(boundary), None) => Some(boundary),
        (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

#[inline]
pub(crate) fn find_sse_boundary_bytes(buffer: &[u8], offset: usize) -> Option<(usize, usize)> {
    let data = &buffer[offset..];
    let newline_boundary = data.windows(2).position(|w| w == b"\n\n").map(|idx| (idx, 2));
    let carriage_boundary = data.windows(4).position(|w| w == b"\r\n\r\n").map(|idx| (idx, 4));

    match (newline_boundary, carriage_boundary) {
        (Some((n_idx, n_len)), Some((c_idx, c_len))) => {
            let na = offset + n_idx;
            let ca = offset + c_idx;
            if na <= ca { Some((na, n_len)) } else { Some((ca, c_len)) }
        }
        (Some((idx, len)), None) => Some((offset + idx, len)),
        (None, Some((idx, len))) => Some((offset + idx, len)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{drain_consumed_sse, extract_data_payload, find_sse_boundary, next_sse_event};

    /// Drive one chunk through the pump, collecting raw event text.
    fn collect(buf: &mut Vec<u8>, offset: &mut usize, chunk: &str) -> Vec<String> {
        buf.extend_from_slice(chunk.as_bytes());
        let mut events = Vec::new();
        while let Some(event) = next_sse_event(buf, offset).expect("valid utf-8 stream data") {
            events.push(event.to_string());
        }
        drain_consumed_sse(buf, offset);
        events
    }

    #[test]
    fn pumps_events_across_chunk_boundaries() {
        let mut buf = Vec::new();
        let mut offset = 0usize;
        let events = collect(&mut buf, &mut offset, "data: {\"a\":1}\n\n: keep-alive\n\ndata: {\"b\":2}\n\n");
        assert_eq!(events, ["data: {\"a\":1}", ": keep-alive", "data: {\"b\":2}"]);
        assert_eq!(offset, 0);
        assert!(buf.is_empty());
    }

    #[test]
    fn splits_events_arriving_midway_across_chunks() {
        let mut buf = Vec::new();
        let mut offset = 0usize;
        let first = collect(&mut buf, &mut offset, "data: {\"a\"");
        assert!(first.is_empty());
        let second = collect(&mut buf, &mut offset, ":1}\n\ndata: x\n\n");
        assert_eq!(second, ["data: {\"a\":1}", "data: x"]);
    }

    #[test]
    fn buffer_stays_bounded_to_unprocessed_tail() {
        let mut buf = Vec::new();
        let mut offset = 0usize;
        let _ = collect(&mut buf, &mut offset, "data: one\n\ndata: two\n\n");
        let _ = collect(&mut buf, &mut offset, "data: thr");
        assert_eq!(buf, b"data: thr");
        assert_eq!(offset, 0);
    }

    #[test]
    fn event_borrow_ends_before_drain_and_body_can_break() {
        let buf = b"data: a\n\ndata: b\n\n".to_vec();
        let mut offset = 0usize;
        let mut seen = Vec::new();
        while let Some(event) = next_sse_event(&buf, &mut offset).expect("valid utf-8 stream data") {
            seen.push(event.to_string());
            if seen.len() == 1 {
                // Historical `[DONE]`-style break: offset already advanced,
                // the second event stays buffered for the next chunk.
                break;
            }
        }
        assert_eq!(seen, ["data: a"]);
        assert_eq!(offset, 9);
    }

    #[test]
    fn extract_data_payload_merges_lines() {
        let event = ": keep-alive\n".to_string() + "data: {\"a\":1}\n" + "data: {\"b\":2}\n";
        let payload = extract_data_payload(&event);
        assert_eq!(payload.as_deref(), Some("{\"a\":1}\n{\"b\":2}"));
    }

    #[test]
    fn find_sse_boundary_prefers_newline() {
        let buffer = "data: foo\n\nrest";
        assert_eq!(find_sse_boundary(buffer), Some((9, 2)));
    }
}
