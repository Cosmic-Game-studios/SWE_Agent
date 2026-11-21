//! HTTP plumbing shared by the adapters: client construction, error mapping,
//! SSE response reading and the channel-backed event stream.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::sse::{SseDecoder, SseEvent};
use crate::{EventStream, LlmError, StreamEvent};

/// Longest silence tolerated on an open response before it counts as dead.
/// Reasoning models can pause for minutes between bytes, so this is generous.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) fn client() -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(IDLE_TIMEOUT)
        .user_agent(concat!("swe-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| LlmError::Config(format!("cannot build HTTP client: {e}")))
}

pub(crate) fn network_error(err: reqwest::Error) -> LlmError {
    // The Display of reqwest errors omits the cause chain, which is where the
    // useful part ("connection refused", "dns error", ...) lives.
    let mut message = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    LlmError::Network(message)
}

/// A non-success HTTP response, kept raw so adapters can refine the mapping.
pub(crate) struct ApiFailure {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub should_retry: Option<bool>,
    pub body: String,
    pub json: Option<Value>,
}

impl ApiFailure {
    /// Best human-readable message found in the body.
    pub fn message(&self) -> String {
        let from_json = self.json.as_ref().and_then(|json| {
            let error = json.get("error").unwrap_or(json);
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
                .or_else(|| json.get("message").and_then(Value::as_str))
                .map(str::to_string)
        });
        from_json.unwrap_or_else(|| {
            let trimmed = self.body.trim();
            if trimmed.is_empty() {
                format!("HTTP {}", self.status)
            } else {
                truncate(trimmed, 600)
            }
        })
    }

    pub fn into_error(self, provider: &str) -> LlmError {
        let message = self.message();
        let lower = message.to_ascii_lowercase();
        if matches!(self.status, 400 | 413 | 422) && is_context_overflow(&lower) {
            return LlmError::ContextOverflow(message);
        }
        if self.status == 413 {
            return LlmError::ContextOverflow(message);
        }
        if matches!(self.status, 401 | 403) {
            return LlmError::Auth(format!("{provider}: {message}"));
        }
        let retryable_status = matches!(self.status, 408 | 409 | 425 | 429) || self.status >= 500;
        LlmError::Api {
            provider: provider.to_string(),
            status: self.status,
            message,
            retryable: self.should_retry.unwrap_or(retryable_status),
            retry_after: self.retry_after,
        }
    }
}

fn is_context_overflow(lower: &str) -> bool {
    const MARKERS: &[&str] = &[
        "prompt is too long",
        "context_length_exceeded",
        "maximum context length",
        "context window",
        "exceeds the maximum number of tokens",
        "input token count",
        "too many tokens",
        "request too large",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(ms) = header("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok()) {
        return Some(Duration::from_millis(ms.max(0.0) as u64));
    }
    header("retry-after")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|secs| Duration::from_millis((secs.max(0.0) * 1000.0) as u64))
}

/// Send a request and turn any non-2xx answer into an [`ApiFailure`].
pub(crate) async fn send(
    request: reqwest::RequestBuilder,
) -> Result<Result<reqwest::Response, ApiFailure>, LlmError> {
    let response = request.send().await.map_err(network_error)?;
    let status = response.status();
    if status.is_success() {
        return Ok(Ok(response));
    }
    let retry_after = parse_retry_after(response.headers());
    let should_retry = response
        .headers()
        .get("x-should-retry")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| match v {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        });
    let body = response.text().await.unwrap_or_default();
    let json = serde_json::from_str(&body).ok();
    Ok(Err(ApiFailure {
        status: status.as_u16(),
        retry_after,
        should_retry,
        body,
        json,
    }))
}

/// Reads an SSE body event by event.
pub(crate) struct SseReader {
    body: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    decoder: SseDecoder,
    queue: VecDeque<SseEvent>,
    finished: bool,
}

impl SseReader {
    pub fn new(response: reqwest::Response) -> Self {
        SseReader {
            body: response.bytes_stream().boxed(),
            decoder: SseDecoder::new(),
            queue: VecDeque::new(),
            finished: false,
        }
    }

    /// Next event, or `None` once the body is exhausted.
    pub async fn next(&mut self) -> Result<Option<SseEvent>, LlmError> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Ok(Some(event));
            }
            if self.finished {
                return Ok(None);
            }
            match self.body.next().await {
                Some(Ok(chunk)) => self.queue.extend(self.decoder.push(&chunk)),
                Some(Err(err)) => {
                    return Err(LlmError::Stream(match network_error(err) {
                        LlmError::Network(message) => message,
                        other => other.to_string(),
                    }));
                }
                None => {
                    self.finished = true;
                    self.queue.extend(self.decoder.finish());
                }
            }
        }
    }
}

/// Sending half handed to an adapter's stream-reading task.
pub(crate) struct Emitter {
    tx: mpsc::Sender<Result<StreamEvent, LlmError>>,
}

/// The consumer dropped the stream; stop reading.
pub(crate) struct Cancelled;

impl Emitter {
    pub async fn emit(&self, event: StreamEvent) -> Result<(), Cancelled> {
        self.tx.send(Ok(event)).await.map_err(|_| Cancelled)
    }

    pub async fn text(&self, text: &str) -> Result<(), Cancelled> {
        if text.is_empty() {
            return Ok(());
        }
        self.emit(StreamEvent::TextDelta(text.to_string())).await
    }

    pub async fn reasoning(&self, text: &str) -> Result<(), Cancelled> {
        if text.is_empty() {
            return Ok(());
        }
        self.emit(StreamEvent::ReasoningDelta(text.to_string())).await
    }
}

/// Outcome of an adapter's reader task.
pub(crate) enum ReadError {
    Cancelled,
    Failed(LlmError),
}

impl From<Cancelled> for ReadError {
    fn from(_: Cancelled) -> Self {
        ReadError::Cancelled
    }
}

impl From<LlmError> for ReadError {
    fn from(err: LlmError) -> Self {
        ReadError::Failed(err)
    }
}

/// Run `reader` on its own task and expose what it emits as an [`EventStream`].
/// Dropping the returned stream makes the next `emit` fail, which ends the
/// task and with it the underlying HTTP response.
pub(crate) fn spawn_stream<F, Fut>(reader: F) -> EventStream
where
    F: FnOnce(Emitter) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), ReadError>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(128);
    let error_tx = tx.clone();
    tokio::spawn(async move {
        // Racing against `closed()` matters while the reader is waiting for
        // bytes: a model that is silently thinking would otherwise keep the
        // connection (and the billing) alive after the consumer went away.
        tokio::select! {
            result = reader(Emitter { tx }) => {
                if let Err(ReadError::Failed(err)) = result {
                    let _ = error_tx.send(Err(err)).await;
                }
            }
            _ = error_tx.closed() => {}
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

/// Parse streamed tool-call arguments.
///
/// Returns the arguments object and, when the text was not a JSON object, the
/// raw text so the caller can report it back to the model instead of running
/// the tool on guessed input.
pub(crate) fn parse_tool_arguments(raw: &str) -> (Value, Option<String>) {
    if raw.trim().is_empty() {
        return (Value::Object(Map::new()), None);
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => (value, None),
        _ => (Value::Object(Map::new()), Some(raw.to_string())),
    }
}

/// Tool-call ids must match `^[a-zA-Z0-9_-]+$` for some providers; ids minted
/// by another provider may not.
pub(crate) fn sanitize_tool_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if cleaned.is_empty() { "call".to_string() } else { cleaned }
}

pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

pub(crate) fn new_call_id() -> String {
    format!("call_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn failure(status: u16, body: &str) -> ApiFailure {
        ApiFailure {
            status,
            retry_after: None,
            should_retry: None,
            body: body.to_string(),
            json: serde_json::from_str(body).ok(),
        }
    }

    #[test]
    fn maps_statuses_to_error_classes() {
        let overloaded = failure(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
            .into_error("anthropic");
        assert!(overloaded.is_retryable());
        assert!(overloaded.to_string().contains("Overloaded"));

        let auth = failure(401, r#"{"error":{"message":"bad key"}}"#).into_error("openai");
        assert!(matches!(auth, LlmError::Auth(_)));
        assert!(!auth.is_retryable());

        let overflow = failure(400, r#"{"error":{"message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#)
            .into_error("anthropic");
        assert!(matches!(overflow, LlmError::ContextOverflow(_)));

        let bad = failure(400, r#"{"error":{"message":"messages: bad shape"}}"#).into_error("anthropic");
        assert!(!bad.is_retryable());

        let plain = failure(502, "<html>Bad gateway</html>").into_error("proxy");
        assert!(plain.is_retryable());
        assert!(plain.to_string().contains("Bad gateway"));
    }

    #[test]
    fn should_retry_header_overrides_status() {
        let mut spend_cap = failure(429, r#"{"error":{"message":"spend cap"}}"#);
        spend_cap.should_retry = Some(false);
        assert!(!spend_cap.into_error("anthropic").is_retryable());
    }

    #[test]
    fn tool_arguments_must_be_an_object() {
        assert_eq!(parse_tool_arguments(""), (json!({}), None));
        assert_eq!(parse_tool_arguments(r#"{"a":1}"#), (json!({"a": 1}), None));
        assert_eq!(
            parse_tool_arguments(r#"{"a":"#),
            (json!({}), Some(r#"{"a":"#.to_string()))
        );
        assert_eq!(parse_tool_arguments("[1]"), (json!({}), Some("[1]".to_string())));
    }

    #[test]
    fn sanitizes_foreign_tool_ids() {
        assert_eq!(sanitize_tool_id("toolu_01A-b"), "toolu_01A-b");
        assert_eq!(sanitize_tool_id("call:1.2|x"), "call_1_2_x");
        assert_eq!(sanitize_tool_id(""), "call");
    }
}
