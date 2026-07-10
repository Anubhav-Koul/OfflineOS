//! Server-sent events: decoding, and staying connected.
//!
//! Two things make this more than `lines().filter(…)`:
//!
//! **Chunk boundaries.** Bytes arrive in whatever sizes the network chose. A
//! line, a field, or a UTF-8 code point can straddle two chunks, and `\r\n` can
//! be split down the middle. The decoder buffers bytes (never `String`), splits
//! on `\n`/`\r` — which cannot appear inside a multi-byte UTF-8 sequence, since
//! continuation bytes are all `>= 0x80` — and holds back a trailing `\r` until
//! it knows whether an `\n` follows.
//!
//! **The gateway closes every stream after five minutes.** `SSE_MAX_LIFETIME`
//! (`handlers.rs:218`) is not an error; it is the contract. The client must
//! reconnect with `Last-Event-ID` and carry on. Since each frame's SSE `id:` is
//! the projection cursor, resumption is exact and no events are lost.
//!
//! A third constraint shapes the API: the gateway allows only **3 concurrent
//! streams per caller** (SSE and WS share the budget). Exceeding it is a `429`,
//! which this module surfaces immediately rather than retrying — a retry loop
//! against a concurrency cap never converges.

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::Duration;

use futures_util::{Stream, StreamExt as _};

use super::events::{GatewayEvent, parse_event};
use super::ids::ThreadId;
use crate::error::{Error, Result};

/// Backoff before the first reconnect; doubles up to [`MAX_RECONNECT_BACKOFF`].
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(250);

/// Ceiling on the reconnect delay.
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(10);

/// Consecutive failed reconnects before the stream gives up. Reset by any
/// successfully decoded frame.
const MAX_CONSECUTIVE_RECONNECTS: u32 = 8;

/// One dispatched SSE frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SseFrame {
    /// The `event:` field. Absent means the default, `message`.
    pub event: Option<String>,
    /// The accumulated `data:` lines, joined with `\n`.
    pub data: String,
    /// The `id:` field, if this frame set one.
    pub id: Option<String>,
}

/// Incremental SSE decoder. Feed it bytes, take frames.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: String,
    id: Option<String>,
}

impl SseDecoder {
    /// Feed a chunk; returns every frame it completed.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();

        while let Some(position) = self
            .buffer
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
        {
            // A trailing `\r` might be the first half of a `\r\n` that has not
            // arrived. Wait rather than dispatching an empty line and ending the
            // frame early.
            if self.buffer[position] == b'\r' && position + 1 == self.buffer.len() {
                break;
            }
            let terminator_len =
                if self.buffer[position] == b'\r' && self.buffer[position + 1] == b'\n' {
                    2
                } else {
                    1
                };

            let line = String::from_utf8_lossy(&self.buffer[..position]).into_owned();
            self.buffer.drain(..position + terminator_len);

            if let Some(frame) = self.push_line(&line) {
                frames.push(frame);
            }
        }
        frames
    }

    /// Process one complete line. Returns a frame when the line was the blank
    /// dispatch line.
    fn push_line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            return self.dispatch();
        }
        // A leading colon is a comment. The gateway's 15-second keep-alive
        // arrives this way.
        if line.starts_with(':') {
            return None;
        }

        let (field, value) = match line.split_once(':') {
            // "One space after the colon is ignored" — SSE spec.
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            // A line with no colon is a field with an empty value.
            None => (line, ""),
        };

        match field {
            "event" => self.event = Some(value.to_string()),
            // Each `data` field appends its value *and* a newline; the trailing
            // one is removed at dispatch. Joining with `\n` instead would lose
            // the distinction between `data: a` and `data: a` + `data:`.
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            // The spec says to ignore an id containing a NUL.
            "id" if !value.contains('\0') => self.id = Some(value.to_string()),
            // `retry` is advisory reconnect timing; we run our own backoff.
            _ => {}
        }
        None
    }

    /// A blank line dispatches whatever has accumulated.
    ///
    /// The spec's order matters here: emptiness is tested *before* the trailing
    /// newline is stripped. So a lone `data` line (buffer `"\n"`) dispatches a
    /// frame with empty data, while an `event:` line with no `data` at all
    /// dispatches nothing and resets the event type.
    fn dispatch(&mut self) -> Option<SseFrame> {
        let event = self.event.take();
        let id = self.id.clone(); // `id` persists across frames, per spec.
        let mut data = std::mem::take(&mut self.data);
        if data.is_empty() {
            return None;
        }
        if data.ends_with('\n') {
            data.pop();
        }
        Some(SseFrame { event, data, id })
    }

    /// The last `id:` seen, which is what `Last-Event-ID` must carry.
    pub(crate) fn last_id(&self) -> Option<&str> {
        self.id.as_deref()
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

/// A live, self-healing event stream for one thread.
///
/// Reconnects transparently when the gateway closes the stream at its
/// five-minute lifetime, resuming from the last cursor. Stops for good on a
/// non-retryable gateway error, on the stream-concurrency cap, or after
/// [`MAX_CONSECUTIVE_RECONNECTS`] failures in a row.
pub struct EventStream {
    client: reqwest::Client,
    url: String,
    token: String,
    thread_id: ThreadId,
    inner: Option<ByteStream>,
    decoder: SseDecoder,
    pending: VecDeque<GatewayEvent>,
    last_event_id: Option<String>,
    consecutive_failures: u32,
    finished: bool,
}

impl EventStream {
    pub(crate) fn new(
        client: reqwest::Client,
        base_url: &str,
        token: &str,
        thread_id: ThreadId,
    ) -> Self {
        Self {
            url: format!("{base_url}{}/threads/{thread_id}/events", super::API_PREFIX),
            client,
            token: token.to_string(),
            thread_id,
            inner: None,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            last_event_id: None,
            consecutive_failures: 0,
            finished: false,
        }
    }

    /// Resume from a cursor obtained earlier (the SSE `id:` of the last event
    /// the caller processed).
    pub fn resume_from(mut self, cursor: impl Into<String>) -> Self {
        self.last_event_id = Some(cursor.into());
        self
    }

    /// The thread being streamed.
    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    /// The cursor to resume from if this stream is dropped and recreated.
    pub fn cursor(&self) -> Option<&str> {
        self.last_event_id.as_deref()
    }

    /// The next event, or `None` once the stream is finished for good.
    ///
    /// Cancel-safe in the sense that dropping the future loses at most the
    /// in-flight chunk; the cursor is only advanced once a frame is decoded.
    pub async fn next(&mut self) -> Option<Result<GatewayEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(Ok(event));
            }
            if self.finished {
                return None;
            }
            if self.inner.is_none()
                && let Err(error) = self.reconnect().await
            {
                self.finished = true;
                return Some(Err(error));
            }

            match self.read_chunk().await {
                Ok(Some(frames)) => self.enqueue(frames),
                // End of stream. The gateway closes every stream at five
                // minutes; that is expected, so reconnect rather than report.
                Ok(None) => {
                    tracing::debug!(thread_id = %self.thread_id, "event stream closed; reconnecting");
                    self.inner = None;
                }
                Err(error) => {
                    tracing::warn!(thread_id = %self.thread_id, %error, "event stream read failed");
                    self.inner = None;
                }
            }
        }
    }

    /// Decode one chunk. `Ok(None)` means the underlying body ended.
    async fn read_chunk(&mut self) -> Result<Option<Vec<GatewayEvent>>> {
        let Some(stream) = self.inner.as_mut() else {
            return Ok(None);
        };
        let Some(chunk) = stream.next().await else {
            return Ok(None);
        };
        let chunk = chunk.map_err(|source| Error::Http {
            url: self.url.clone(),
            source,
        })?;

        let frames = self.decoder.push(&chunk);
        if let Some(id) = self.decoder.last_id() {
            self.last_event_id = Some(id.to_string());
        }

        let mut events = Vec::with_capacity(frames.len());
        for frame in frames {
            // The default event name per the SSE spec; the gateway always sets
            // one, so this is defensive.
            let name = frame.event.as_deref().unwrap_or("message");
            match parse_event(name, &frame.data) {
                Ok(event) => events.push(event),
                // A frame we cannot parse is a protocol drift signal, not a
                // reason to tear down a working stream.
                Err(error) => {
                    tracing::warn!(
                        thread_id = %self.thread_id,
                        event = name,
                        %error,
                        "discarding an SSE frame this client could not decode"
                    );
                }
            }
        }
        Ok(Some(events))
    }

    /// Queue decoded events, and notice a terminal `error` frame.
    fn enqueue(&mut self, events: Vec<GatewayEvent>) {
        if !events.is_empty() {
            self.consecutive_failures = 0;
        }
        for event in events {
            // The gateway emits one `error` event and then closes. Reconnecting
            // into a non-retryable error would spin.
            if let GatewayEvent::Error(error) = &event
                && !error.retryable
            {
                self.finished = true;
            }
            self.pending.push_back(event);
        }
    }

    /// Open (or reopen) the underlying HTTP response.
    async fn reconnect(&mut self) -> Result<()> {
        loop {
            // `open` is a free function, not a method. Awaiting `self.open()`
            // would hold a `&EventStream` across the await, and `&T: Send`
            // requires `T: Sync` — which this type can never be, because it owns
            // a boxed response body. That would make the whole stream
            // undrivable from `tokio::spawn`, which is the only way it is used.
            match open(
                &self.client,
                &self.url,
                &self.token,
                self.last_event_id.as_deref(),
            )
            .await
            {
                Ok(stream) => {
                    self.inner = Some(stream);
                    self.decoder = SseDecoder::default();
                    return Ok(());
                }
                Err(error) => {
                    // The concurrency cap is a state of the world, not a blip.
                    // Retrying it just burns the caller's rate limit.
                    if error.is_stream_cap() {
                        return Err(error);
                    }
                    self.consecutive_failures += 1;
                    if self.consecutive_failures >= MAX_CONSECUTIVE_RECONNECTS {
                        return Err(Error::EventStream {
                            thread_id: self.thread_id.to_string(),
                            reason: format!(
                                "gave up after {} consecutive reconnect failures: {error}",
                                self.consecutive_failures
                            ),
                        });
                    }
                    let backoff = backoff_for(self.consecutive_failures);
                    tracing::warn!(
                        thread_id = %self.thread_id,
                        attempt = self.consecutive_failures,
                        ?backoff,
                        %error,
                        "reconnecting to the event stream"
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
}

/// Open the SSE response. See [`EventStream::reconnect`] for why this is not a
/// method.
async fn open(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    cursor: Option<&str>,
) -> Result<ByteStream> {
    let mut request = client
        .get(url)
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, "text/event-stream");
    if let Some(cursor) = cursor {
        // The gateway prefers `Last-Event-ID` over `?after_cursor=`
        // (handlers.rs:158), and the header round-trips the cursor verbatim.
        request = request.header("Last-Event-ID", cursor);
    }

    let response = request.send().await.map_err(|source| Error::Http {
        url: url.to_string(),
        source,
    })?;
    if !response.status().is_success() {
        return Err(super::gateway_error("GET", url, response).await);
    }
    Ok(Box::pin(response.bytes_stream()))
}

fn backoff_for(attempt: u32) -> Duration {
    let doubling = 2u32.saturating_pow(attempt.saturating_sub(1));
    INITIAL_RECONNECT_BACKOFF
        .checked_mul(doubling)
        .unwrap_or(MAX_RECONNECT_BACKOFF)
        .min(MAX_RECONNECT_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(chunks: &[&str]) -> Vec<SseFrame> {
        let mut decoder = SseDecoder::default();
        let mut all = Vec::new();
        for chunk in chunks {
            all.extend(decoder.push(chunk.as_bytes()));
        }
        all
    }

    #[test]
    fn a_whole_frame_in_one_chunk_decodes() {
        let decoded = frames(&["event: gate\nid: 7\ndata: {\"a\":1}\n\n"]);
        assert_eq!(
            decoded,
            vec![SseFrame {
                event: Some("gate".into()),
                data: "{\"a\":1}".into(),
                id: Some("7".into()),
            }]
        );
    }

    #[test]
    fn a_frame_split_across_chunks_decodes() {
        // Split mid-field, mid-value, and mid-terminator.
        let decoded = frames(&["eve", "nt: ga", "te\nda", "ta: {\"a\":1}\n", "\n"]);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].event.as_deref(), Some("gate"));
        assert_eq!(decoded[0].data, "{\"a\":1}");
    }

    #[test]
    fn a_crlf_split_across_chunks_is_not_read_as_a_blank_line() {
        // The `\r` ends one chunk and the `\n` opens the next. Dispatching on
        // the lone `\r` would emit a frame with no data and then a second
        // spurious blank line.
        let decoded = frames(&["data: hello\r", "\ndata: world\r\n\r\n"]);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].data, "hello\nworld");
    }

    #[test]
    fn bare_cr_and_bare_lf_both_terminate_lines() {
        assert_eq!(frames(&["data: a\rdata: b\n\n"])[0].data, "a\nb");
    }

    #[test]
    fn multi_byte_utf8_split_across_chunks_survives() {
        // "é" is 0xC3 0xA9; split it. Splitting on \n/\r bytes is safe precisely
        // because continuation bytes are >= 0x80.
        let mut decoder = SseDecoder::default();
        let mut all = decoder.push(b"data: caf\xc3");
        all.extend(decoder.push(b"\xa9\n\n"));
        assert_eq!(all[0].data, "café");
    }

    #[test]
    fn multiple_data_lines_join_with_newlines() {
        assert_eq!(
            frames(&["data: one\ndata: two\ndata: three\n\n"])[0].data,
            "one\ntwo\nthree"
        );
    }

    #[test]
    fn comments_are_ignored_which_is_how_keep_alive_arrives() {
        // The gateway's 15-second heartbeat is an SSE comment.
        let decoded = frames(&[": keep-alive\n\ndata: real\n\n"]);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].data, "real");
    }

    #[test]
    fn one_space_after_the_colon_is_stripped_but_only_one() {
        assert_eq!(frames(&["data:  two spaces\n\n"])[0].data, " two spaces");
        assert_eq!(frames(&["data:no space\n\n"])[0].data, "no space");
    }

    #[test]
    fn a_field_with_no_colon_has_an_empty_value() {
        let decoded = frames(&["data\n\n"]);
        // Per the spec, emptiness is tested before the trailing newline is
        // stripped, so the buffer "\n" is non-empty and this dispatches.
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].data, "");
    }

    #[test]
    fn an_event_line_with_no_data_dispatches_nothing() {
        // The buffer is genuinely empty, so the spec says return without
        // dispatching — and the event type must be reset, or it would leak into
        // the next frame.
        let decoded = frames(&["event: gate\n\ndata: x\n\n"]);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].event, None, "the stale event type leaked");
        assert_eq!(decoded[0].data, "x");
    }

    #[test]
    fn a_trailing_empty_data_line_is_preserved_as_a_blank_line() {
        // `data: a` then `data:` means "a\n", not "a". Joining data lines with
        // `\n` instead of appending one per line would collapse the two.
        assert_eq!(frames(&["data: a\ndata:\n\n"])[0].data, "a\n");
        assert_eq!(frames(&["data\ndata\n\n"])[0].data, "\n");
    }

    #[test]
    fn the_id_persists_across_frames_per_the_spec() {
        let decoded = frames(&["id: 5\ndata: a\n\ndata: b\n\n"]);
        assert_eq!(decoded[0].id.as_deref(), Some("5"));
        // The second frame sets no id, so `Last-Event-ID` remains 5 — losing it
        // would resume the stream from the beginning of the thread.
        assert_eq!(decoded[1].id.as_deref(), Some("5"));
    }

    #[test]
    fn an_id_containing_a_nul_is_ignored_per_the_spec() {
        let decoded = frames(&["id: 5\n\nid: bad\0id\ndata: x\n\n"]);
        assert_eq!(decoded.last().expect("a frame").id.as_deref(), Some("5"));
    }

    #[test]
    fn a_partial_frame_yields_nothing_until_its_blank_line_arrives() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"event: gate\ndata: {}\n").is_empty());
        assert_eq!(decoder.push(b"\n").len(), 1);
    }

    #[test]
    fn back_to_back_frames_in_one_chunk_both_decode() {
        let decoded = frames(&["data: a\n\ndata: b\n\n"]);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].data, "a");
        assert_eq!(decoded[1].data, "b");
    }

    /// Compile-time regression. The only way this stream is ever used is from a
    /// spawned task, and `tokio::spawn` requires the future to be `Send`.
    ///
    /// An earlier version awaited `self.open()` inside `reconnect(&mut self)`,
    /// which holds a `&EventStream` across the await. `&T: Send` requires
    /// `T: Sync`, and `EventStream` owns a boxed response body that is `Send`
    /// but never `Sync` — so the pump would not compile. The closure below is
    /// never called; it exists to make the bound part of the test suite.
    #[test]
    fn a_pump_task_over_the_event_stream_is_send() {
        fn assert_send<F: std::future::Future + Send>(_: F) {}

        async fn pump(mut stream: EventStream) {
            while let Some(event) = stream.next().await {
                let _ = event;
            }
        }

        // Never called. The `Send` bound is checked when this compiles.
        let _bound_check = |stream: EventStream| assert_send(pump(stream));
    }

    #[test]
    fn reconnect_backoff_doubles_and_is_capped() {
        assert_eq!(backoff_for(1), Duration::from_millis(250));
        assert_eq!(backoff_for(2), Duration::from_millis(500));
        assert_eq!(backoff_for(3), Duration::from_secs(1));
        assert_eq!(backoff_for(64), MAX_RECONNECT_BACKOFF);
        assert_eq!(backoff_for(u32::MAX), MAX_RECONNECT_BACKOFF);
    }
}
