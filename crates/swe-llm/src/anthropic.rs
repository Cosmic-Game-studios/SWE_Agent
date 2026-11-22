//! Claude via the Anthropic Messages API (`POST /v1/messages`, SSE).
//!
//! Notes on the less obvious parts:
//!
//! * Thinking blocks are replayed byte-for-byte. Current Claude models bind
//!   them to the model *and* to the conversation prefix, so the agent loop
//!   keeps histories append-only and this adapter never rewrites them.
//! * The system prompt and the newest message carry `cache_control`
//!   breakpoints, which makes every loop iteration read the previous one's
//!   prefix from the prompt cache.
//! * Sampling parameters are never sent: API version `2026-10-21` rejects them.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use swe_protocol::{ContentBlock, Effort, Message, ProviderMeta, Role, StopReason, Usage};

use crate::http::{self, ApiFailure, Emitter, ReadError, SseReader};
use crate::{
    ChatRequest, Completion, EventStream, LlmError, ModelInfo, Protocol, Provider, ProviderSettings, StreamEvent,
};

const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const PROTOCOL: &str = "anthropic";
const DEFAULT_MAX_TOKENS: u32 = 64_000;

pub struct Anthropic {
    settings: ProviderSettings,
    client: reqwest::Client,
    /// Cleared the first time the endpoint rejects the corresponding field,
    /// which happens behind gateways that lag the first-party API.
    eager_tool_streaming: AtomicBool,
    refusal_fallback: AtomicBool,
}

/// What a given Claude model accepts.
struct Capabilities {
    adaptive_thinking: bool,
    effort: bool,
    xhigh: bool,
    refusal_fallback: bool,
}

fn capabilities(model: &str) -> Capabilities {
    // Generations that predate adaptive thinking still take `budget_tokens`
    // and have no (or a reduced) `effort` knob; they run without thinking here.
    const LEGACY: &[&str] = &[
        "claude-3",
        "claude-haiku-4-5",
        "claude-sonnet-4-5",
        "claude-opus-4-5",
        "claude-opus-4-1",
        "claude-opus-4-2",
        "claude-sonnet-4-2",
    ];
    let legacy = LEGACY.iter().any(|prefix| model.contains(prefix));
    let no_xhigh = model.contains("claude-opus-4-6") || model.contains("claude-sonnet-4-6");
    const FALLBACK: &[&str] = &[
        "claude-fable-5-2",
        "claude-fable-5-1",
        "claude-opus-5",
        "claude-sonnet-5-5",
    ];
    Capabilities {
        adaptive_thinking: !legacy,
        effort: !legacy,
        xhigh: !legacy && !no_xhigh,
        refusal_fallback: FALLBACK.iter().any(|prefix| model.starts_with(prefix)),
    }
}

impl Anthropic {
    pub fn new(settings: ProviderSettings, client: reqwest::Client) -> Self {
        // Fields newer than the oldest deployments are only sent to the
        // first-party API by default.
        let first_party = settings.base_url.contains("api.anthropic.com");
        Anthropic {
            settings,
            client,
            eager_tool_streaming: AtomicBool::new(first_party),
            refusal_fallback: AtomicBool::new(first_party),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder, LlmError> {
        let key = self
            .settings
            .api_key
            .as_deref()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| LlmError::Auth(format!("no API key configured for provider `{}`", self.settings.id)))?;
        let mut builder = self
            .client
            .request(method, format!("{}{}", self.settings.base_url, path))
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION);
        for (name, value) in &self.settings.headers {
            builder = builder.header(name, value);
        }
        Ok(builder)
    }

    fn build_body(&self, request: &ChatRequest) -> (Value, bool) {
        let caps = capabilities(&request.model);
        let mut body = Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert(
            "max_tokens".into(),
            json!(request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        );
        body.insert("stream".into(), json!(true));
        if !request.system.is_empty() {
            body.insert(
                "system".into(),
                json!([{
                    "type": "text",
                    "text": request.system,
                    "cache_control": {"type": "ephemeral"},
                }]),
            );
        }
        if !request.tools.is_empty() {
            let eager = self.eager_tool_streaming.load(Ordering::Relaxed);
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    let mut spec = json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.input_schema,
                    });
                    if eager {
                        spec["eager_input_streaming"] = json!(true);
                    }
                    spec
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        body.insert("messages".into(), Value::Array(convert_messages(&request.messages)));
        if caps.adaptive_thinking {
            body.insert("thinking".into(), json!({"type": "adaptive", "display": "summarized"}));
        }
        if caps.effort {
            let effort = match request.effort {
                Effort::Low => "low",
                Effort::Medium => "medium",
                Effort::High => "high",
                Effort::Xhigh if caps.xhigh => "xhigh",
                Effort::Xhigh => "max",
            };
            body.insert("output_config".into(), json!({"effort": effort}));
        }
        let fallback = caps.refusal_fallback && self.refusal_fallback.load(Ordering::Relaxed);
        if fallback {
            body.insert("fallbacks".into(), json!("default"));
        }
        (Value::Object(body), fallback)
    }

    /// Turn off an optional request field the endpoint just rejected.
    /// Returns true when something changed and the request is worth resending.
    fn downgrade(&self, failure: &ApiFailure) -> bool {
        if failure.status != 400 {
            return false;
        }
        let message = failure.message();
        if message.contains("eager_input_streaming") {
            return self.eager_tool_streaming.swap(false, Ordering::Relaxed);
        }
        if message.contains("fallbacks") || message.contains("anthropic-beta") {
            return self.refusal_fallback.swap(false, Ordering::Relaxed);
        }
        false
    }
}

fn map_failure(failure: ApiFailure, provider: &str) -> LlmError {
    let message = failure.message();
    if failure.status == 400 && message.contains("`signature` in `thinking` block") {
        return LlmError::ReasoningRejected(message);
    }
    failure.into_error(provider)
}

#[async_trait]
impl Provider for Anthropic {
    fn id(&self) -> &str {
        &self.settings.id
    }

    fn protocol(&self) -> Protocol {
        Protocol::Anthropic
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        // At most two optional fields can be rejected, so three attempts suffice.
        for _ in 0..3 {
            let (body, fallback) = self.build_body(&request);
            let mut builder = self.request(reqwest::Method::POST, "/messages")?.json(&body);
            if fallback {
                builder = builder.header("anthropic-beta", FALLBACK_BETA);
            }
            match http::send(builder).await? {
                Ok(response) => {
                    let model = request.model.clone();
                    return Ok(http::spawn_stream(move |emitter| {
                        read_stream(SseReader::new(response), emitter, model)
                    }));
                }
                Err(failure) => {
                    if self.downgrade(&failure) {
                        tracing::warn!(
                            provider = %self.settings.id,
                            "endpoint rejected an optional field, retrying without it: {}",
                            failure.message()
                        );
                        continue;
                    }
                    return Err(map_failure(failure, &self.settings.id));
                }
            }
        }
        Err(LlmError::Protocol(
            "request kept being rejected after downgrades".into(),
        ))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut builder = self
                .request(reqwest::Method::GET, "/models")?
                .query(&[("limit", "1000")]);
            if let Some(after) = &after {
                builder = builder.query(&[("after_id", after.as_str())]);
            }
            let response = match http::send(builder).await? {
                Ok(response) => response,
                Err(failure) => return Err(failure.into_error(&self.settings.id)),
            };
            let page: Value = response.json().await.map_err(http::network_error)?;
            for entry in page["data"].as_array().into_iter().flatten() {
                let Some(id) = entry["id"].as_str() else { continue };
                models.push(ModelInfo {
                    id: id.to_string(),
                    name: entry["display_name"].as_str().unwrap_or(id).to_string(),
                    context_window: entry["max_input_tokens"].as_u64(),
                    max_output_tokens: entry["max_tokens"].as_u64(),
                });
            }
            match (page["has_more"].as_bool(), page["last_id"].as_str()) {
                (Some(true), Some(last)) => after = Some(last.to_string()),
                _ => break,
            }
        }
        Ok(models)
    }
}

/// Translate the neutral history into Messages API `messages`.
fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<(Role, Vec<Value>)> = Vec::new();
    for message in messages {
        let mut blocks = Vec::new();
        // `tool_result` blocks have to lead a user message.
        if message.role == Role::User {
            for block in &message.content {
                if let ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } = block
                {
                    let mut result = json!({
                        "type": "tool_result",
                        "tool_use_id": http::sanitize_tool_id(tool_call_id),
                        "content": content,
                    });
                    if *is_error {
                        result["is_error"] = json!(true);
                    }
                    blocks.push(result);
                }
            }
        }
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } => {
                    if !text.trim().is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                ContentBlock::Image { media_type, data } => blocks.push(json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data},
                })),
                ContentBlock::Reasoning { meta, .. } => {
                    if let Some(meta) = meta.as_ref().filter(|m| m.protocol == PROTOCOL) {
                        blocks.push(meta.raw.clone());
                    }
                }
                ContentBlock::ToolCall { id, name, input, .. } => {
                    if message.role == Role::Assistant {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": http::sanitize_tool_id(id),
                            "name": name,
                            "input": input,
                        }));
                    }
                }
                ContentBlock::Opaque { meta } => {
                    if meta.protocol == PROTOCOL {
                        blocks.push(meta.raw.clone());
                    }
                }
                ContentBlock::ToolResult { .. } => {}
            }
        }
        if blocks.is_empty() {
            continue;
        }
        // The API wants alternating roles; merge neighbours with the same one.
        match out.last_mut() {
            Some((role, existing)) if *role == message.role => existing.extend(blocks),
            _ => out.push((message.role, blocks)),
        }
    }

    // Cache breakpoint on the newest block so the next request reads this prefix.
    if let Some((_, blocks)) = out.last_mut()
        && let Some(last) = blocks.last_mut()
    {
        let cacheable = matches!(last["type"].as_str(), Some("text" | "tool_result" | "image"));
        if cacheable {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
    }

    out.into_iter()
        .map(|(role, content)| {
            json!({
                "role": match role { Role::User => "user", Role::Assistant => "assistant" },
                "content": content,
            })
        })
        .collect()
}

/// One content block being assembled from stream deltas.
struct PendingBlock {
    /// The `content_block` object from `content_block_start`, updated in place.
    block: Value,
    /// Accumulated `input_json_delta` text for `tool_use` blocks.
    partial_json: String,
}

async fn read_stream(mut sse: SseReader, emitter: Emitter, requested_model: String) -> Result<(), ReadError> {
    let mut blocks: Vec<Option<PendingBlock>> = Vec::new();
    let mut usage = Usage::default();
    let mut uncached_input = 0u64;
    let mut stop_reason = StopReason::EndTurn;
    let mut refusal_detail: Option<String> = None;
    let mut model = requested_model;
    let mut finished = false;

    while let Some(event) = sse.next().await? {
        if event.data.is_empty() {
            continue;
        }
        let data: Value = serde_json::from_str(&event.data)
            .map_err(|e| LlmError::Protocol(format!("invalid JSON in stream event: {e}")))?;
        match data["type"].as_str().unwrap_or(&event.event) {
            "message_start" => {
                let message = &data["message"];
                if let Some(served) = message["model"].as_str() {
                    model = served.to_string();
                }
                apply_usage(&message["usage"], &mut usage, &mut uncached_input);
            }
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                if blocks.len() <= index {
                    blocks.resize_with(index + 1, || None);
                }
                let block = data["content_block"].clone();
                if block["type"] == "tool_use" {
                    emitter
                        .emit(StreamEvent::ToolCallStart {
                            index,
                            id: block["id"].as_str().unwrap_or_default().to_string(),
                            name: block["name"].as_str().unwrap_or_default().to_string(),
                        })
                        .await?;
                }
                if let Some(text) = block["text"].as_str() {
                    emitter.text(text).await?;
                }
                blocks[index] = Some(PendingBlock {
                    block,
                    partial_json: String::new(),
                });
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let Some(Some(pending)) = blocks.get_mut(index) else {
                    continue;
                };
                let delta = &data["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        append_str(&mut pending.block, "text", text);
                        emitter.text(text).await?;
                    }
                    "thinking_delta" => {
                        let text = delta["thinking"].as_str().unwrap_or_default();
                        append_str(&mut pending.block, "thinking", text);
                        emitter.reasoning(text).await?;
                    }
                    "signature_delta" => {
                        let signature = delta["signature"].as_str().unwrap_or_default();
                        append_str(&mut pending.block, "signature", signature);
                    }
                    "input_json_delta" => {
                        let fragment = delta["partial_json"].as_str().unwrap_or_default();
                        pending.partial_json.push_str(fragment);
                        if !fragment.is_empty() {
                            emitter
                                .emit(StreamEvent::ToolCallDelta {
                                    index,
                                    fragment: fragment.to_string(),
                                })
                                .await?;
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                let delta = &data["delta"];
                if let Some(reason) = delta["stop_reason"].as_str() {
                    stop_reason = match reason {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        "refusal" => StopReason::Refusal,
                        other => StopReason::Other(other.to_string()),
                    };
                }
                let details = &delta["stop_details"];
                if details.is_object() {
                    refusal_detail = details["explanation"]
                        .as_str()
                        .or_else(|| details["category"].as_str())
                        .map(str::to_string);
                }
                apply_usage(&data["usage"], &mut usage, &mut uncached_input);
            }
            "message_stop" => {
                finished = true;
                break;
            }
            "error" => {
                let error = &data["error"];
                let kind = error["type"].as_str().unwrap_or("error");
                let message = error["message"].as_str().unwrap_or("unknown stream error");
                let retryable = matches!(
                    kind,
                    "overloaded_error" | "api_error" | "rate_limit_error" | "timeout_error"
                );
                return Err(ReadError::Failed(if retryable {
                    LlmError::Stream(format!("{kind}: {message}"))
                } else {
                    LlmError::Protocol(format!("{kind}: {message}"))
                }));
            }
            _ => {} // ping and event types added later
        }
    }

    if !finished {
        return Err(ReadError::Failed(LlmError::Stream(
            "connection closed before the response finished".into(),
        )));
    }

    let mut content = assemble(blocks, &model);
    if stop_reason == StopReason::Refusal
        && let Some(detail) = refusal_detail
    {
        content.push(ContentBlock::text(format!(
            "[The model declined to continue: {detail}]"
        )));
    }
    emitter
        .emit(StreamEvent::Completed(Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            usage,
            stop_reason,
            model: Some(model),
        }))
        .await?;
    Ok(())
}

fn append_str(block: &mut Value, field: &str, text: &str) {
    let current = block[field].as_str().unwrap_or_default();
    block[field] = Value::String(format!("{current}{text}"));
}

/// Anthropic reports uncached input, cache reads and cache writes separately;
/// the neutral `Usage` wants the total prompt size.
fn apply_usage(raw: &Value, usage: &mut Usage, uncached_input: &mut u64) {
    if !raw.is_object() {
        return;
    }
    if let Some(n) = raw["input_tokens"].as_u64() {
        *uncached_input = n;
    }
    if let Some(n) = raw["cache_read_input_tokens"].as_u64() {
        usage.cache_read_tokens = n;
    }
    if let Some(n) = raw["cache_creation_input_tokens"].as_u64() {
        usage.cache_write_tokens = n;
    }
    if let Some(n) = raw["output_tokens"].as_u64() {
        usage.output_tokens = n;
    }
    usage.input_tokens = *uncached_input + usage.cache_read_tokens + usage.cache_write_tokens;
}

fn assemble(blocks: Vec<Option<PendingBlock>>, model: &str) -> Vec<ContentBlock> {
    let meta = |raw: Value| ProviderMeta {
        protocol: PROTOCOL.to_string(),
        model: model.to_string(),
        raw,
    };
    let blocks: Vec<PendingBlock> = blocks.into_iter().flatten().collect();

    // After a server-side refusal fallback, reasoning and tool calls produced
    // by the declined model (everything before the last `fallback` marker)
    // must not be replayed; text and the marker itself stay in place.
    let last_fallback = blocks.iter().rposition(|b| b.block["type"] == "fallback");

    let mut content = Vec::new();
    for (position, pending) in blocks.into_iter().enumerate() {
        let before_fallback = last_fallback.is_some_and(|boundary| position < boundary);
        let PendingBlock { block, partial_json } = pending;
        match block["type"].as_str().unwrap_or_default() {
            "text" => {
                let text = block["text"].as_str().unwrap_or_default();
                if !text.is_empty() {
                    content.push(ContentBlock::text(text));
                }
            }
            "thinking" | "redacted_thinking" => {
                if !before_fallback {
                    content.push(ContentBlock::Reasoning {
                        text: block["thinking"].as_str().unwrap_or_default().to_string(),
                        meta: Some(meta(block)),
                    });
                }
            }
            "tool_use" => {
                if !before_fallback {
                    // Without eager streaming the API may deliver the whole
                    // input in `content_block_start` instead of deltas.
                    let (input, invalid_json) = if partial_json.trim().is_empty() && block["input"].is_object() {
                        (block["input"].clone(), None)
                    } else {
                        http::parse_tool_arguments(&partial_json)
                    };
                    content.push(ContentBlock::ToolCall {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        input,
                        invalid_json,
                        meta: None,
                    });
                }
            }
            _ => content.push(ContentBlock::Opaque { meta: meta(block) }),
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use swe_protocol::ToolSpec;

    fn provider(base_url: &str) -> Anthropic {
        Anthropic::new(
            ProviderSettings {
                id: "anthropic".into(),
                protocol: Protocol::Anthropic,
                base_url: base_url.into(),
                api_key: Some("k".into()),
                headers: vec![],
            },
            reqwest::Client::new(),
        )
    }

    fn request(model: &str, messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            system: "You are a coding agent.".into(),
            messages,
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            max_output_tokens: None,
            effort: Effort::Xhigh,
            cache_key: None,
        }
    }

    #[test]
    fn body_for_current_model_uses_adaptive_thinking_and_caching() {
        let (body, fallback) = provider("https://api.anthropic.com/v1")
            .build_body(&request("claude-opus-5-5", vec![Message::user_text("hi")]));
        assert!(fallback);
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["thinking"], json!({"type": "adaptive", "display": "summarized"}));
        assert_eq!(body["output_config"], json!({"effort": "xhigh"}));
        assert_eq!(body["max_tokens"], 64_000);
        assert_eq!(body["stream"], true);
        assert!(body.get("temperature").is_none());
        assert_eq!(body["system"][0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn body_respects_model_generation_and_gateways() {
        let gateway = provider("https://gateway.example.com/anthropic/v1");
        let (body, fallback) = gateway.build_body(&request("claude-opus-5-5", vec![Message::user_text("hi")]));
        assert!(!fallback);
        assert!(body.get("fallbacks").is_none());
        assert!(body["tools"][0].get("eager_input_streaming").is_none());

        let first_party = provider("https://api.anthropic.com/v1");
        let (legacy, fallback) = first_party.build_body(&request("claude-haiku-4-5", vec![Message::user_text("hi")]));
        assert!(!fallback);
        assert!(legacy.get("thinking").is_none());
        assert!(legacy.get("output_config").is_none());

        let (four_six, _) = first_party.build_body(&request("claude-sonnet-4-6", vec![Message::user_text("hi")]));
        assert_eq!(four_six["output_config"]["effort"], "max");
        assert!(four_six.get("fallbacks").is_none());
    }

    #[test]
    fn converts_history_with_tool_round_trip() {
        let thinking_raw = json!({"type": "thinking", "thinking": "plan", "signature": "sig"});
        let history = vec![
            Message::user_text("fix it"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Reasoning {
                        text: "plan".into(),
                        meta: Some(ProviderMeta {
                            protocol: "anthropic".into(),
                            model: "claude-opus-5-5".into(),
                            raw: thinking_raw.clone(),
                        }),
                    },
                    ContentBlock::Reasoning {
                        text: "foreign".into(),
                        meta: Some(ProviderMeta {
                            protocol: "openai-responses".into(),
                            model: "gpt".into(),
                            raw: json!({"type": "reasoning"}),
                        }),
                    },
                    ContentBlock::text("Reading."),
                    ContentBlock::ToolCall {
                        id: "call:1".into(),
                        name: "read_file".into(),
                        input: json!({"path": "a"}),
                        invalid_json: None,
                        meta: None,
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("also check b"),
                    ContentBlock::tool_result("call:1", "contents", false),
                ],
            },
        ];
        let converted = convert_messages(&history);
        assert_eq!(converted.len(), 3);
        let assistant = converted[1]["content"].as_array().unwrap();
        assert_eq!(assistant.len(), 3, "foreign reasoning is dropped");
        assert_eq!(assistant[0], thinking_raw, "own reasoning is replayed verbatim");
        assert_eq!(assistant[2]["id"], "call_1");
        let user = converted[2]["content"].as_array().unwrap();
        assert_eq!(user[0]["type"], "tool_result", "tool results lead the user turn");
        assert_eq!(user[0]["tool_use_id"], "call_1");
        assert_eq!(user[1]["type"], "text");
        assert_eq!(user[1]["cache_control"], json!({"type": "ephemeral"}));
        assert!(user[0].get("cache_control").is_none());
    }

    #[test]
    fn merges_consecutive_roles_and_skips_empty_messages() {
        let history = vec![
            Message::user_text("one"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::text("  ")],
            },
            Message::user_text("two"),
        ];
        let converted = convert_messages(&history);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn fallback_boundary_drops_declined_reasoning_and_tool_calls() {
        let pending = |block: Value| {
            Some(PendingBlock {
                block,
                partial_json: String::new(),
            })
        };
        let content = assemble(
            vec![
                pending(json!({"type": "thinking", "thinking": "a", "signature": "s1"})),
                pending(json!({"type": "text", "text": "partial"})),
                pending(json!({"type": "tool_use", "id": "t0", "name": "x", "input": {}})),
                pending(json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}})),
                pending(json!({"type": "thinking", "thinking": "b", "signature": "s2"})),
                pending(json!({"type": "text", "text": "answer"})),
            ],
            "b",
        );
        let kinds: Vec<&str> = content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { .. } => "text",
                ContentBlock::Reasoning { .. } => "reasoning",
                ContentBlock::ToolCall { .. } => "tool",
                ContentBlock::Opaque { .. } => "opaque",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["text", "opaque", "reasoning", "text"]);
    }
}
