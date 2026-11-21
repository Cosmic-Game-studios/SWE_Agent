//! Streaming LLM provider adapters.
//!
//! Every adapter implements [`Provider`]: it turns a provider-neutral
//! [`ChatRequest`] into the provider's wire format, streams the response and
//! reports it as a sequence of [`StreamEvent`]s that always ends with
//! [`StreamEvent::Completed`] carrying the fully assembled assistant message.
//!
//! Four wire protocols are implemented, which together cover essentially every
//! hosted and local model:
//!
//! | Protocol            | Used for                                             |
//! |---------------------|------------------------------------------------------|
//! | `anthropic`         | Claude via the Messages API                          |
//! | `openai-responses`  | OpenAI via the Responses API                         |
//! | `openai-chat`       | OpenRouter and any OpenAI-compatible endpoint        |
//! | `gemini`            | Google Gemini via `streamGenerateContent`            |

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use swe_protocol::{Effort, Message, StopReason, ToolSpec, Usage};

pub mod anthropic;
pub mod gemini;
mod http;
pub mod mock;
pub mod openai_chat;
pub mod openai_responses;
mod sse;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Anthropic,
    OpenaiResponses,
    OpenaiChat,
    Gemini,
    Mock,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Anthropic => "anthropic",
            Protocol::OpenaiResponses => "openai-responses",
            Protocol::OpenaiChat => "openai-chat",
            Protocol::Gemini => "gemini",
            Protocol::Mock => "mock",
        }
    }
}

impl std::str::FromStr for Protocol {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic" => Ok(Protocol::Anthropic),
            "openai-responses" | "openai" => Ok(Protocol::OpenaiResponses),
            "openai-chat" | "openai-compatible" => Ok(Protocol::OpenaiChat),
            "gemini" | "google" => Ok(Protocol::Gemini),
            "mock" => Ok(Protocol::Mock),
            other => Err(format!(
                "unknown protocol `{other}` (expected anthropic|openai-responses|openai-chat|gemini)"
            )),
        }
    }
}

/// Everything needed to talk to one provider endpoint.
#[derive(Clone, Debug)]
pub struct ProviderSettings {
    /// Id the rest of the system uses for this provider (`anthropic`, `openrouter`, `ollama`, ...).
    pub id: String,
    pub protocol: Protocol,
    /// Base URL without a trailing slash, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    /// Extra headers sent with every request.
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    /// System prompt. Kept byte-stable for a conversation so provider prompt
    /// caches (and Anthropic's preserved-thinking check) keep working.
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// Upper bound for this response. `None` lets the adapter choose.
    pub max_output_tokens: Option<u32>,
    pub effort: Effort,
    /// Stable per-conversation key for providers that route prompt caches by key.
    pub cache_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// A tool call started streaming. `index` identifies it within this response.
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    /// More argument text arrived for the tool call at `index`.
    ToolCallDelta {
        index: usize,
        fragment: String,
    },
    /// Final event of every successful stream.
    Completed(Completion),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    /// The assembled assistant message, including replayable provider metadata.
    pub message: Message,
    pub usage: Usage,
    pub stop_reason: StopReason,
    /// Model that actually served the response, when the provider reports it.
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum LlmError {
    /// The provider rejected the credentials.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The provider answered with an error status.
    #[error("{provider} returned HTTP {status}: {message}")]
    Api {
        provider: String,
        status: u16,
        message: String,
        retryable: bool,
        retry_after: Option<Duration>,
    },
    /// The prompt no longer fits the model's context window.
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
    /// Replayed reasoning was rejected; the history must be sent without it.
    #[error("replayed reasoning was rejected: {0}")]
    ReasoningRejected(String),
    /// Connection-level failure before or during the stream.
    #[error("network error: {0}")]
    Network(String),
    /// The stream ended or broke before the response was complete.
    #[error("stream interrupted: {0}")]
    Stream(String),
    /// The provider sent something we could not interpret.
    #[error("unexpected response: {0}")]
    Protocol(String),
    #[error("{0}")]
    Config(String),
}

impl LlmError {
    /// Whether sending the same request again can succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::Api { retryable, .. } => *retryable,
            LlmError::Network(_) | LlmError::Stream(_) => true,
            _ => false,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            LlmError::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Configured id of this provider.
    fn id(&self) -> &str;

    fn protocol(&self) -> Protocol;

    /// Start a streaming completion.
    ///
    /// Errors that happen before the first byte of the response (bad status,
    /// connection failure) are returned directly; later ones arrive as an
    /// `Err` item on the stream. Dropping the stream cancels the request.
    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError>;

    /// Models the configured credentials can use.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError>;
}

/// Build the adapter for `settings.protocol`.
pub fn build_provider(settings: ProviderSettings) -> Result<Arc<dyn Provider>, LlmError> {
    let client = http::client()?;
    Ok(match settings.protocol {
        Protocol::Anthropic => Arc::new(anthropic::Anthropic::new(settings, client)),
        Protocol::OpenaiResponses => Arc::new(openai_responses::OpenAiResponses::new(settings, client)),
        Protocol::OpenaiChat => Arc::new(openai_chat::OpenAiChat::new(settings, client)),
        Protocol::Gemini => Arc::new(gemini::Gemini::new(settings, client)),
        Protocol::Mock => Arc::new(mock::MockProvider::demo(settings.id)),
    })
}

/// Drain a stream to its final [`Completion`], ignoring the deltas.
pub async fn collect(mut stream: EventStream) -> Result<Completion, LlmError> {
    use futures::StreamExt;
    while let Some(event) = stream.next().await {
        if let StreamEvent::Completed(done) = event? {
            return Ok(done);
        }
    }
    Err(LlmError::Stream("stream ended without a completion".into()))
}
