//! OpenAI-compatible Chat Completions (`POST {base}/chat/completions`, SSE).
//!
//! This is the lingua franca of everything that is not a first-party API:
//! OpenRouter, Ollama, LM Studio, vLLM, Groq, DeepSeek, xAI, Together and so
//! on. The adapter is therefore deliberately tolerant: it reads every known
//! spelling of streamed reasoning, copes with servers that omit `index`,
//! `[DONE]` or usage, and stops sending optional fields a server rejects.
//!
//! OpenRouter gets a few extras: the unified `reasoning` parameter, replay of
//! `reasoning_details` (required for tool calling with Claude, Gemini and
//! OpenAI reasoning models), automatic prompt caching for Anthropic models
//! and sticky routing per conversation.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use swe_protocol::{ContentBlock, Effort, Message, ProviderMeta, Role, StopReason, Usage};

use crate::http::{self, ApiFailure, Emitter, ReadError, SseReader};
use crate::{
    ChatRequest, Completion, EventStream, LlmError, ModelInfo, Protocol, Provider, ProviderSettings, StreamEvent,
};

const PROTOCOL: &str = "openai-chat";

pub struct OpenAiChat {
    settings: ProviderSettings,
    client: reqwest::Client,
    openrouter: bool,
    /// Optional request fields this endpoint has rejected, as `model\0field`
    /// (or `\0field` when the rejection is not model specific).
    rejected: Mutex<HashSet<String>>,
}

impl OpenAiChat {
    pub fn new(settings: ProviderSettings, client: reqwest::Client) -> Self {
        let openrouter = settings.base_url.contains("openrouter.ai");
        OpenAiChat {
            settings,
            client,
            openrouter,
            rejected: Mutex::new(HashSet::new()),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .request(method, format!("{}{}", self.settings.base_url, path));
        if let Some(key) = self.settings.api_key.as_deref().filter(|k| !k.is_empty()) {
            builder = builder.bearer_auth(key);
        }
        if self.openrouter {
            builder = builder
                .header("HTTP-Referer", "https://github.com/Cosmic-Game-studios/SWE_Agent")
                .header("X-OpenRouter-Title", "SWE Agent")
                .header("X-Title", "SWE Agent");
        }
        for (name, value) in &self.settings.headers {
            builder = builder.header(name, value);
        }
        builder
    }

    fn is_rejected(&self, model: &str, field: &str) -> bool {
        let rejected = self.rejected.lock().unwrap();
        rejected.contains(&format!("{model}\0{field}")) || rejected.contains(&format!("\0{field}"))
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert(
            "messages".into(),
            Value::Array(convert_messages(request, &self.settings.id)),
        );
        body.insert("stream".into(), json!(true));
        if !self.is_rejected("", "stream_options") {
            body.insert("stream_options".into(), json!({"include_usage": true}));
        }
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        },
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
        }
        if let Some(max) = request.max_output_tokens {
            body.insert("max_tokens".into(), json!(max));
        }
        if self.openrouter {
            if !self.is_rejected(&request.model, "reasoning") {
                let effort = match request.effort {
                    Effort::Low => "low",
                    Effort::Medium => "medium",
                    Effort::High => "high",
                    Effort::Xhigh => "xhigh",
                };
                body.insert("reasoning".into(), json!({"effort": effort}));
            }
            if request.model.starts_with("anthropic/") {
                // Automatic mode: OpenRouter moves the breakpoint to the last
                // cacheable block on every request.
                body.insert("cache_control".into(), json!({"type": "ephemeral"}));
            }
            if let Some(key) = &request.cache_key {
                body.insert("session_id".into(), json!(key));
            }
        }
        Value::Object(body)
    }

    /// Remember an optional field the server just rejected. Returns true when
    /// the request should be retried without it.
    fn downgrade(&self, model: &str, failure: &ApiFailure) -> bool {
        if !matches!(failure.status, 400 | 422) {
            return false;
        }
        let message = failure.message().to_ascii_lowercase();
        let key = if message.contains("stream_options") {
            "\0stream_options".to_string()
        } else if self.openrouter && message.contains("reasoning") {
            format!("{model}\0reasoning")
        } else {
            return false;
        };
        self.rejected.lock().unwrap().insert(key)
    }
}

fn map_failure(failure: ApiFailure, provider: &str) -> LlmError {
    let error = failure
        .json
        .as_ref()
        .map(|json| json["error"].clone())
        .unwrap_or(Value::Null);
    let code = error["code"].as_str().unwrap_or_default().to_string();
    let error_type = error["metadata"]["error_type"].as_str().unwrap_or_default().to_string();
    if code == "context_length_exceeded" || error_type == "context_length_exceeded" {
        return LlmError::ContextOverflow(failure.message());
    }
    // Quota and credit problems arrive as 429/402 but never fix themselves.
    let terminal = code == "insufficient_quota"
        || code.ends_with("spend_limit_exceeded")
        || code == "credit_balance_exhausted"
        || error_type == "payment_required"
        || failure.status == 402;
    let status = failure.status;
    match failure.into_error(provider) {
        LlmError::Api {
            provider,
            message,
            retryable,
            retry_after,
            ..
        } => LlmError::Api {
            provider,
            status,
            message,
            // 524 (edge timeout) and 529 (provider overloaded) are >= 500 and already retryable.
            retryable: retryable && !terminal,
            retry_after,
        },
        other => other,
    }
}

#[async_trait]
impl Provider for OpenAiChat {
    fn id(&self) -> &str {
        &self.settings.id
    }

    fn protocol(&self) -> Protocol {
        Protocol::OpenaiChat
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        for _ in 0..3 {
            let body = self.build_body(&request);
            let builder = self.request(reqwest::Method::POST, "/chat/completions").json(&body);
            match http::send(builder).await? {
                Ok(response) => {
                    let model = request.model.clone();
                    let provider = self.settings.id.clone();
                    return Ok(http::spawn_stream(move |emitter| {
                        read_stream(SseReader::new(response), emitter, model, provider)
                    }));
                }
                Err(failure) => {
                    if self.downgrade(&request.model, &failure) {
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
        let response = match http::send(self.request(reqwest::Method::GET, "/models")).await? {
            Ok(response) => response,
            Err(failure) => return Err(map_failure(failure, &self.settings.id)),
        };
        let page: Value = response.json().await.map_err(http::network_error)?;
        let entries = page["data"]
            .as_array()
            .or_else(|| page["models"].as_array())
            .or_else(|| page.as_array());
        let mut models = Vec::new();
        for entry in entries.into_iter().flatten() {
            let Some(id) = entry["id"].as_str().or_else(|| entry["name"].as_str()) else {
                continue;
            };
            // OpenRouter says which models can call tools; an agent needs them.
            if let Some(parameters) = entry["supported_parameters"].as_array()
                && !parameters.iter().any(|p| p == "tools")
            {
                continue;
            }
            models.push(ModelInfo {
                id: id.to_string(),
                name: entry["name"].as_str().unwrap_or(id).to_string(),
                context_window: entry["context_length"]
                    .as_u64()
                    .or_else(|| entry["top_provider"]["context_length"].as_u64())
                    .or_else(|| entry["context_window"].as_u64()),
                max_output_tokens: entry["top_provider"]["max_completion_tokens"].as_u64(),
            });
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

/// Translate the neutral history into Chat Completions `messages`.
fn convert_messages(request: &ChatRequest, provider_id: &str) -> Vec<Value> {
    let mut out = Vec::new();
    if !request.system.is_empty() {
        out.push(json!({"role": "system", "content": request.system}));
    }
    for message in &request.messages {
        match message.role {
            Role::User => {
                // Tool results answer the preceding assistant message and
                // have to come before any new user content.
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } = block
                    {
                        let content = if *is_error && !content.starts_with("Error") {
                            format!("Error: {content}")
                        } else {
                            content.clone()
                        };
                        out.push(json!({"role": "tool", "tool_call_id": tool_call_id, "content": content}));
                    }
                }
                let mut parts = Vec::new();
                let mut has_image = false;
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text, .. } if !text.is_empty() => {
                            parts.push(json!({"type": "text", "text": text}));
                        }
                        ContentBlock::Image { media_type, data } => {
                            has_image = true;
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": {"url": format!("data:{media_type};base64,{data}")},
                            }));
                        }
                        _ => {}
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                // Plain strings are understood by every server; arrays of
                // parts only by the multimodal ones.
                let content = if has_image {
                    Value::Array(parts)
                } else {
                    let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
                    Value::String(text.join("\n\n"))
                };
                out.push(json!({"role": "user", "content": content}));
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                let mut extra = Map::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: chunk, .. } => {
                            if !text.is_empty() && !chunk.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(chunk);
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            input,
                            invalid_json,
                            ..
                        } => {
                            let arguments = invalid_json.clone().unwrap_or_else(|| input.to_string());
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": arguments},
                            }));
                        }
                        ContentBlock::Reasoning { meta: Some(meta), .. }
                            // Signed or encrypted reasoning is only valid for
                            // the provider and model that produced it.
                            if meta.protocol == PROTOCOL
                                && meta.model == request.model
                                && meta.raw["provider"] == provider_id
                            => {
                                for field in ["reasoning_details", "reasoning_content"] {
                                    if let Some(value) = meta.raw.get(field) {
                                        extra.insert(field.to_string(), value.clone());
                                    }
                                }
                            }
                        _ => {}
                    }
                }
                if text.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                let mut assistant = Map::new();
                assistant.insert("role".into(), json!("assistant"));
                assistant.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text)
                    },
                );
                if !tool_calls.is_empty() {
                    assistant.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                assistant.extend(extra);
                out.push(Value::Object(assistant));
            }
        }
    }
    out
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
}

async fn read_stream(mut sse: SseReader, emitter: Emitter, model: String, provider: String) -> Result<(), ReadError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_details: Vec<Value> = Vec::new();
    let mut calls: Vec<PendingCall> = Vec::new();
    let mut usage = Usage::default();
    let mut finish: Option<String> = None;
    let mut served_model: Option<String> = None;
    let mut saw_done = false;

    while let Some(event) = sse.next().await? {
        let data = event.data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            saw_done = true;
            break;
        }
        let chunk: Value =
            serde_json::from_str(data).map_err(|e| LlmError::Protocol(format!("invalid JSON in stream chunk: {e}")))?;

        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            let status = error["code"].as_u64().unwrap_or(500) as u16;
            let failure = ApiFailure {
                status,
                retry_after: None,
                should_retry: None,
                body: data.to_string(),
                json: Some(chunk.clone()),
            };
            return Err(ReadError::Failed(match map_failure(failure, &provider) {
                LlmError::Api {
                    message,
                    retryable: true,
                    ..
                } => LlmError::Stream(message),
                other => other,
            }));
        }
        if let Some(served) = chunk["model"].as_str() {
            served_model = Some(served.to_string());
        }
        if chunk["usage"].is_object() {
            usage = convert_usage(&chunk["usage"]);
        }

        let choice = &chunk["choices"][0];
        let delta = &choice["delta"];
        if let Some(content) = delta["content"].as_str() {
            text.push_str(content);
            emitter.text(content).await?;
        }
        // The same reasoning text can be offered under several names; show
        // the first one that is present.
        let spoken = delta["reasoning"]
            .as_str()
            .or_else(|| delta["reasoning_content"].as_str());
        if let Some(fragment) = spoken {
            reasoning.push_str(fragment);
            emitter.reasoning(fragment).await?;
        }
        for detail in delta["reasoning_details"].as_array().into_iter().flatten() {
            if spoken.is_none()
                && let Some(fragment) = detail["text"].as_str().or_else(|| detail["summary"].as_str())
            {
                reasoning.push_str(fragment);
                emitter.reasoning(fragment).await?;
            }
            merge_reasoning_detail(&mut reasoning_details, detail);
        }
        for (position, call) in delta["tool_calls"].as_array().into_iter().flatten().enumerate() {
            // Servers that omit `index` send one call per chunk: a chunk with
            // an id starts a new call, one without continues the latest.
            let index = match call["index"].as_u64() {
                Some(index) => index as usize,
                None if call["id"].is_string() && position == 0 && !calls.is_empty() => calls.len(),
                None => calls.len().saturating_sub(1).max(position),
            };
            if calls.len() <= index {
                calls.resize_with(index + 1, PendingCall::default);
            }
            let pending = &mut calls[index];
            if let Some(id) = call["id"].as_str().filter(|id| !id.is_empty()) {
                pending.id = id.to_string();
            }
            if let Some(name) = call["function"]["name"].as_str() {
                pending.name.push_str(name);
            }
            if !pending.announced && !pending.name.is_empty() {
                pending.announced = true;
                if pending.id.is_empty() {
                    pending.id = http::new_call_id();
                }
                emitter
                    .emit(StreamEvent::ToolCallStart {
                        index,
                        id: pending.id.clone(),
                        name: pending.name.clone(),
                    })
                    .await?;
            }
            // Arguments are normally string fragments; a few servers send an object.
            let fragment = match &call["function"]["arguments"] {
                Value::String(fragment) => fragment.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            if !fragment.is_empty() {
                pending.arguments.push_str(&fragment);
                emitter.emit(StreamEvent::ToolCallDelta { index, fragment }).await?;
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            finish = Some(reason.to_string());
        }
    }

    let calls: Vec<PendingCall> = calls.into_iter().filter(|call| !call.name.is_empty()).collect();
    let Some(finish) = finish.or_else(|| (saw_done && (!text.is_empty() || !calls.is_empty())).then(|| "stop".into()))
    else {
        return Err(ReadError::Failed(LlmError::Stream(
            "connection closed before the response finished".into(),
        )));
    };

    let mut content = Vec::new();
    if !reasoning.is_empty() || !reasoning_details.is_empty() {
        let mut raw = Map::new();
        raw.insert("provider".into(), json!(provider));
        if !reasoning_details.is_empty() {
            raw.insert("reasoning_details".into(), Value::Array(reasoning_details));
        } else {
            // DeepSeek-style endpoints want the plain string back on tool-call turns.
            raw.insert("reasoning_content".into(), json!(reasoning));
        }
        content.push(ContentBlock::Reasoning {
            text: reasoning,
            meta: Some(ProviderMeta {
                protocol: PROTOCOL.to_string(),
                model: model.clone(),
                raw: Value::Object(raw),
            }),
        });
    }
    if !text.is_empty() {
        content.push(ContentBlock::text(text));
    }
    let has_calls = !calls.is_empty();
    for call in calls {
        let (input, invalid_json) = http::parse_tool_arguments(&call.arguments);
        content.push(ContentBlock::ToolCall {
            id: if call.id.is_empty() {
                http::new_call_id()
            } else {
                call.id
            },
            name: call.name,
            input,
            invalid_json,
            meta: None,
        });
    }

    let stop_reason = match finish.as_str() {
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        // Some servers report `stop` even though they emitted tool calls.
        _ if has_calls => StopReason::ToolUse,
        "stop" | "end_turn" | "eos" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::EndTurn,
        other => StopReason::Other(other.to_string()),
    };

    emitter
        .emit(StreamEvent::Completed(Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            usage,
            stop_reason,
            model: served_model,
        }))
        .await?;
    Ok(())
}

fn convert_usage(raw: &Value) -> Usage {
    Usage {
        input_tokens: raw["prompt_tokens"].as_u64().unwrap_or(0),
        output_tokens: raw["completion_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: raw["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: raw["prompt_tokens_details"]["cache_write_tokens"].as_u64().unwrap_or(0),
        reasoning_tokens: raw["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or(0),
    }
}

/// Fold a streamed `reasoning_details` fragment into the accumulated array.
///
/// Fragments of one entry share `type` and `index`; their string payloads are
/// concatenated and scalar fields (signature, id, format) keep the last
/// non-null value. Entries without an index are appended as they come.
fn merge_reasoning_detail(details: &mut Vec<Value>, fragment: &Value) {
    let same_entry = |existing: &Value| {
        existing["type"] == fragment["type"] && !fragment["index"].is_null() && existing["index"] == fragment["index"]
    };
    let Some(existing) = details.iter_mut().find(|existing| same_entry(existing)) else {
        details.push(fragment.clone());
        return;
    };
    let (Some(target), Some(source)) = (existing.as_object_mut(), fragment.as_object()) else {
        return;
    };
    for (key, value) in source {
        match (target.get(key), value) {
            (Some(Value::String(current)), Value::String(addition))
                if matches!(key.as_str(), "text" | "summary" | "data") =>
            {
                let joined = format!("{current}{addition}");
                target.insert(key.clone(), Value::String(joined));
            }
            (_, Value::Null) => {}
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swe_protocol::ToolSpec;

    fn provider(base_url: &str) -> OpenAiChat {
        OpenAiChat::new(
            ProviderSettings {
                id: "openrouter".into(),
                protocol: Protocol::OpenaiChat,
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
            system: "sys".into(),
            messages,
            tools: vec![ToolSpec {
                name: "bash".into(),
                description: "Run".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_output_tokens: None,
            effort: Effort::High,
            cache_key: Some("session-1".into()),
        }
    }

    #[test]
    fn openrouter_body_has_reasoning_caching_and_session() {
        let p = provider("https://openrouter.ai/api/v1");
        let body = p.build_body(&request("anthropic/claude-opus-5.5", vec![Message::user_text("hi")]));
        assert_eq!(body["reasoning"], json!({"effort": "high"}));
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(body["session_id"], "session-1");
        assert_eq!(body["tools"][0]["function"]["name"], "bash");
        assert_eq!(body["messages"][0], json!({"role": "system", "content": "sys"}));
        assert_eq!(body["messages"][1], json!({"role": "user", "content": "hi"}));
        assert!(body.get("max_tokens").is_none());

        let other = p.build_body(&request("openai/gpt-6.2-sol", vec![]));
        assert!(other.get("cache_control").is_none());
    }

    #[test]
    fn generic_endpoints_get_a_minimal_body() {
        let p = provider("http://localhost:11434/v1");
        let body = p.build_body(&request("qwen3-coder", vec![Message::user_text("hi")]));
        assert!(body.get("reasoning").is_none());
        assert!(body.get("session_id").is_none());
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
    }

    #[test]
    fn rejected_optional_fields_are_dropped_on_retry() {
        let p = provider("https://openrouter.ai/api/v1");
        let failure = |message: &str| ApiFailure {
            status: 400,
            retry_after: None,
            should_retry: None,
            body: String::new(),
            json: Some(json!({"error": {"message": message}})),
        };
        assert!(p.downgrade("m", &failure("Reasoning is not supported by this model")));
        assert!(
            !p.downgrade("m", &failure("Reasoning is not supported by this model")),
            "only once"
        );
        assert!(p.build_body(&request("m", vec![])).get("reasoning").is_none());
        assert!(
            p.build_body(&request("other", vec![])).get("reasoning").is_some(),
            "per model"
        );
        assert!(p.downgrade("m", &failure("Unknown parameter: stream_options")));
        assert!(p.build_body(&request("other", vec![])).get("stream_options").is_none());
        assert!(!p.downgrade("m", &failure("messages is malformed")));
    }

    #[test]
    fn tool_round_trip_orders_results_before_user_text_and_replays_reasoning() {
        let details = json!([{"type": "reasoning.text", "text": "think", "signature": "sig", "index": 0}]);
        let reasoning = |provider: &str, model: &str| ContentBlock::Reasoning {
            text: "think".into(),
            meta: Some(ProviderMeta {
                protocol: PROTOCOL.into(),
                model: model.into(),
                raw: json!({"provider": provider, "reasoning_details": details}),
            }),
        };
        let call = ContentBlock::ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            input: json!({"command": "ls"}),
            invalid_json: None,
            meta: None,
        };
        let history = vec![
            Message::user_text("go"),
            Message {
                role: Role::Assistant,
                content: vec![reasoning("openrouter", "m"), call.clone()],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("and then stop"),
                    ContentBlock::tool_result("call_1", "boom", true),
                ],
            },
        ];
        let messages = convert_messages(&request("m", history.clone()), "openrouter");
        assert_eq!(messages[2]["content"], Value::Null);
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );
        assert_eq!(messages[2]["reasoning_details"], details);
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "Error: boom"})
        );
        assert_eq!(messages[4], json!({"role": "user", "content": "and then stop"}));

        // Another model or provider must not see the signed reasoning.
        let other_model = convert_messages(&request("other", history.clone()), "openrouter");
        assert!(other_model[2].get("reasoning_details").is_none());
        let other_provider = convert_messages(&request("m", history), "deepseek");
        assert!(other_provider[2].get("reasoning_details").is_none());
    }

    #[test]
    fn reasoning_detail_fragments_merge_by_type_and_index() {
        let mut details = Vec::new();
        merge_reasoning_detail(
            &mut details,
            &json!({"type": "reasoning.text", "text": "Let ", "signature": null, "index": 0, "format": "anthropic-claude-v1"}),
        );
        merge_reasoning_detail(
            &mut details,
            &json!({"type": "reasoning.text", "text": "me", "index": 0}),
        );
        merge_reasoning_detail(
            &mut details,
            &json!({"type": "reasoning.text", "signature": "SIG", "index": 0}),
        );
        merge_reasoning_detail(
            &mut details,
            &json!({"type": "reasoning.encrypted", "data": "abc", "index": 1}),
        );
        assert_eq!(
            details,
            vec![
                json!({"type": "reasoning.text", "text": "Let me", "signature": "SIG", "index": 0, "format": "anthropic-claude-v1"}),
                json!({"type": "reasoning.encrypted", "data": "abc", "index": 1}),
            ]
        );
    }

    #[test]
    fn quota_errors_are_not_retried() {
        let failure = |status: u16, body: Value| ApiFailure {
            status,
            retry_after: None,
            should_retry: None,
            body: body.to_string(),
            json: Some(body),
        };
        let quota = map_failure(
            failure(
                429,
                json!({"error": {"message": "You exceeded your quota", "code": "insufficient_quota"}}),
            ),
            "openai",
        );
        assert!(!quota.is_retryable());
        let credits = map_failure(
            failure(402, json!({"error": {"code": 402, "message": "Insufficient credits"}})),
            "openrouter",
        );
        assert!(!credits.is_retryable());
        let busy = map_failure(
            failure(429, json!({"error": {"code": 429, "message": "Rate limit exceeded"}})),
            "openrouter",
        );
        assert!(busy.is_retryable());
        let overloaded = map_failure(
            failure(529, json!({"error": {"code": 529, "message": "Provider overloaded"}})),
            "openrouter",
        );
        assert!(overloaded.is_retryable());
        let overflow = map_failure(
            failure(
                400,
                json!({"error": {"code": 400, "message": "too long", "metadata": {"error_type": "context_length_exceeded"}}}),
            ),
            "openrouter",
        );
        assert!(matches!(overflow, LlmError::ContextOverflow(_)));
    }
}
