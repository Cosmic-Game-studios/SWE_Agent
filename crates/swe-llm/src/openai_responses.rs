//! OpenAI via the Responses API (`POST /v1/responses`, SSE), used statelessly.
//!
//! Current OpenAI models only support tool calling together with reasoning on
//! this API, so it is the one the native OpenAI provider uses.
//!
//! Notes on the less obvious parts:
//!
//! * `store: false`: nothing is kept server-side. The complete output items
//!   (reasoning with `encrypted_content`, messages with their `phase`,
//!   function calls with their ids) are stored raw and resent unchanged,
//!   which is what keeps the model's reasoning across tool calls.
//! * Reasoning items are bound to the model that produced them; after a model
//!   switch they are left out, while messages and tool calls stay.
//! * Optional reasoning settings a model rejects are dropped and remembered.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use swe_protocol::{ContentBlock, Effort, Message, ProviderMeta, Role, StopReason, Usage};

use crate::http::{self, ApiFailure, Emitter, ReadError, SseReader};
use crate::{
    ChatRequest, Completion, EventStream, LlmError, ModelInfo, Protocol, Provider, ProviderSettings, StreamEvent,
};

const PROTOCOL: &str = "openai-responses";

pub struct OpenAiResponses {
    settings: ProviderSettings,
    client: reqwest::Client,
    /// Optional settings rejected per model, as `model\0setting`.
    rejected: Mutex<HashSet<String>>,
}

impl OpenAiResponses {
    pub fn new(settings: ProviderSettings, client: reqwest::Client) -> Self {
        OpenAiResponses {
            settings,
            client,
            rejected: Mutex::new(HashSet::new()),
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
            .bearer_auth(key);
        for (name, value) in &self.settings.headers {
            builder = builder.header(name, value);
        }
        Ok(builder)
    }

    fn is_rejected(&self, model: &str, setting: &str) -> bool {
        self.rejected.lock().unwrap().contains(&format!("{model}\0{setting}"))
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(request.model));
        if !request.system.is_empty() {
            body.insert("instructions".into(), json!(request.system));
        }
        body.insert(
            "input".into(),
            Value::Array(convert_messages(&request.messages, &request.model)),
        );
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                        // Strict mode needs every property required and no
                        // open objects, which general tool schemas do not meet.
                        "strict": false,
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
            body.insert("tool_choice".into(), json!("auto"));
            body.insert("parallel_tool_calls".into(), json!(true));
        }
        if let Some(max) = request.max_output_tokens {
            body.insert("max_output_tokens".into(), json!(max));
        }
        if !self.is_rejected(&request.model, "reasoning") {
            let effort = match request.effort {
                Effort::Low => "low",
                Effort::Medium => "medium",
                Effort::High => "high",
                Effort::Xhigh if self.is_rejected(&request.model, "xhigh") => "high",
                Effort::Xhigh => "xhigh",
            };
            let mut reasoning = json!({"effort": effort});
            if !self.is_rejected(&request.model, "summary") {
                reasoning["summary"] = json!("auto");
            }
            body.insert("reasoning".into(), reasoning);
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
        body.insert("store".into(), json!(false));
        body.insert("stream".into(), json!(true));
        if let Some(key) = &request.cache_key {
            body.insert("prompt_cache_key".into(), json!(key));
        }
        Value::Object(body)
    }

    /// Remember a reasoning setting the model rejected. Returns true when the
    /// request is worth resending.
    fn downgrade(&self, request: &ChatRequest, failure: &ApiFailure) -> bool {
        if failure.status != 400 {
            return false;
        }
        let param = failure
            .json
            .as_ref()
            .and_then(|json| json["error"]["param"].as_str())
            .unwrap_or_default()
            .to_string();
        let message = failure.message().to_ascii_lowercase();
        let setting = if param == "reasoning.summary"
            || message.contains("reasoning.summary")
            || message.contains("reasoning summar")
        {
            "summary"
        } else if (param == "reasoning.effort" || message.contains("reasoning.effort"))
            && request.effort == Effort::Xhigh
            && !self.is_rejected(&request.model, "xhigh")
        {
            "xhigh"
        } else if param.starts_with("reasoning") || param == "include" || message.contains("reasoning") {
            "reasoning"
        } else {
            return false;
        };
        self.rejected
            .lock()
            .unwrap()
            .insert(format!("{}\0{setting}", request.model))
    }
}

/// Error codes that arrive with a retryable status but never fix themselves.
fn is_terminal_code(code: &str) -> bool {
    matches!(
        code,
        "insufficient_quota"
            | "credit_balance_exhausted"
            | "invalid_prompt"
            | "content_policy_violation"
            | "cyber_policy"
            | "bio_policy"
    ) || code.ends_with("spend_limit_exceeded")
}

fn map_failure(failure: ApiFailure, provider: &str) -> LlmError {
    let code = failure
        .json
        .as_ref()
        .and_then(|json| json["error"]["code"].as_str())
        .unwrap_or_default()
        .to_string();
    if code == "context_length_exceeded" {
        return LlmError::ContextOverflow(failure.message());
    }
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
            retryable: retryable && !is_terminal_code(&code),
            retry_after,
        },
        other => other,
    }
}

/// Map an error reported inside the stream (`error` / `response.failed`).
fn stream_error(code: &str, message: &str) -> LlmError {
    let text = if code.is_empty() {
        message.to_string()
    } else {
        format!("{code}: {message}")
    };
    match code {
        "context_length_exceeded" => LlmError::ContextOverflow(text),
        "rate_limit_exceeded" | "server_error" | "slow_down" | "server_is_overloaded" => LlmError::Stream(text),
        _ => LlmError::Protocol(text),
    }
}

#[async_trait]
impl Provider for OpenAiResponses {
    fn id(&self) -> &str {
        &self.settings.id
    }

    fn protocol(&self) -> Protocol {
        Protocol::OpenaiResponses
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        // Up to three reasoning settings can be rejected one after another.
        for _ in 0..4 {
            let body = self.build_body(&request);
            let builder = self.request(reqwest::Method::POST, "/responses")?.json(&body);
            match http::send(builder).await? {
                Ok(response) => {
                    let model = request.model.clone();
                    return Ok(http::spawn_stream(move |emitter| {
                        read_stream(SseReader::new(response), emitter, model)
                    }));
                }
                Err(failure) => {
                    if self.downgrade(&request, &failure) {
                        tracing::warn!(
                            model = %request.model,
                            "model rejected a reasoning setting, retrying without it: {}",
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
        let response = match http::send(self.request(reqwest::Method::GET, "/models")?).await? {
            Ok(response) => response,
            Err(failure) => return Err(map_failure(failure, &self.settings.id)),
        };
        let page: Value = response.json().await.map_err(http::network_error)?;
        let mut models: Vec<ModelInfo> = page["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["id"].as_str())
            .filter(|id| is_text_model(id))
            .map(|id| {
                let (context_window, max_output_tokens) = known_limits(id);
                ModelInfo {
                    id: id.to_string(),
                    name: id.to_string(),
                    context_window,
                    max_output_tokens,
                }
            })
            .collect();
        // Newest families first: the listing itself is unordered.
        models.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(models)
    }
}

/// The models endpoint lists everything (embeddings, audio, images, ...) and
/// carries no capability data, so filter by name.
fn is_text_model(id: &str) -> bool {
    const PREFIXES: &[&str] = &["gpt-", "o1", "o3", "o4", "codex", "chatgpt-"];
    const EXCLUDED: &[&str] = &[
        "embedding",
        "audio",
        "realtime",
        "tts",
        "transcribe",
        "whisper",
        "image",
        "dall-e",
        "sora",
        "moderation",
        "search",
        "instruct",
        "computer-use",
    ];
    PREFIXES.iter().any(|prefix| id.starts_with(prefix)) && !EXCLUDED.iter().any(|word| id.contains(word))
}

/// Context window and output cap for model families whose limits are known;
/// the API does not report them.
fn known_limits(id: &str) -> (Option<u64>, Option<u64>) {
    if id.starts_with("gpt-6") || id.starts_with("gpt-5.5") || id.starts_with("gpt-5.6") {
        (Some(1_050_000), Some(128_000))
    } else if id.starts_with("gpt-5") {
        (Some(400_000), Some(128_000))
    } else {
        (None, None)
    }
}

/// Translate the neutral history into Responses `input` items.
fn convert_messages(messages: &[Message], model: &str) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } = block
                    {
                        let output = if *is_error && !content.starts_with("Error") {
                            format!("Error: {content}")
                        } else {
                            content.clone()
                        };
                        items.push(json!({"type": "function_call_output", "call_id": tool_call_id, "output": output}));
                    }
                }
                let mut parts = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text, .. } if !text.is_empty() => {
                            parts.push(json!({"type": "input_text", "text": text}));
                        }
                        ContentBlock::Image { media_type, data } => parts.push(json!({
                            "type": "input_image",
                            "image_url": format!("data:{media_type};base64,{data}"),
                            "detail": "auto",
                        })),
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    items.push(json!({"type": "message", "role": "user", "content": parts}));
                }
            }
            Role::Assistant => {
                for block in &message.content {
                    let own = |meta: &Option<ProviderMeta>| meta.as_ref().filter(|m| m.protocol == PROTOCOL).cloned();
                    match block {
                        ContentBlock::Text { text, meta } => match own(meta) {
                            Some(meta) => items.push(meta.raw),
                            None if !text.is_empty() => items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}],
                            })),
                            None => {}
                        },
                        ContentBlock::Reasoning { meta, .. } => {
                            if let Some(meta) = own(meta).filter(|m| m.model == model) {
                                // Without its encrypted content a stateless
                                // reasoning item cannot be resolved server-side.
                                if meta.raw["encrypted_content"].is_string() {
                                    items.push(meta.raw);
                                }
                            }
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            input,
                            invalid_json,
                            meta,
                        } => match own(meta) {
                            Some(meta) => items.push(meta.raw),
                            None => items.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": invalid_json.clone().unwrap_or_else(|| input.to_string()),
                            })),
                        },
                        ContentBlock::Opaque { meta } => {
                            if meta.protocol == PROTOCOL {
                                items.push(meta.raw.clone());
                            }
                        }
                        ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {}
                    }
                }
            }
        }
    }
    items
}

async fn read_stream(mut sse: SseReader, emitter: Emitter, model: String) -> Result<(), ReadError> {
    // Completed output items by `output_index`.
    let mut items: Vec<Option<Value>> = Vec::new();
    let mut final_response: Option<Value> = None;
    let mut incomplete_reason: Option<String> = None;
    let mut reasoning_parts_seen = 0u32;

    while let Some(event) = sse.next().await? {
        let data = event.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let payload: Value =
            serde_json::from_str(data).map_err(|e| LlmError::Protocol(format!("invalid JSON in stream event: {e}")))?;
        let kind = payload["type"].as_str().unwrap_or(&event.event).to_string();
        let output_index = payload["output_index"].as_u64().unwrap_or(0) as usize;
        match kind.as_str() {
            "response.output_item.added" => {
                let item = &payload["item"];
                if item["type"] == "function_call" {
                    emitter
                        .emit(StreamEvent::ToolCallStart {
                            index: output_index,
                            id: item["call_id"].as_str().unwrap_or_default().to_string(),
                            name: item["name"].as_str().unwrap_or_default().to_string(),
                        })
                        .await?;
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                emitter.text(payload["delta"].as_str().unwrap_or_default()).await?;
            }
            "response.reasoning_summary_part.added" => {
                // Summaries come in several parts; keep them visually apart.
                if reasoning_parts_seen > 0 {
                    emitter.reasoning("\n\n").await?;
                }
                reasoning_parts_seen += 1;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                emitter.reasoning(payload["delta"].as_str().unwrap_or_default()).await?;
            }
            "response.function_call_arguments.delta" => {
                let fragment = payload["delta"].as_str().unwrap_or_default();
                if !fragment.is_empty() {
                    emitter
                        .emit(StreamEvent::ToolCallDelta {
                            index: output_index,
                            fragment: fragment.to_string(),
                        })
                        .await?;
                }
            }
            "response.output_item.done" => {
                if items.len() <= output_index {
                    items.resize(output_index + 1, None);
                }
                items[output_index] = Some(payload["item"].clone());
            }
            "response.completed" => {
                final_response = Some(payload["response"].clone());
                break;
            }
            "response.incomplete" => {
                incomplete_reason = Some(
                    payload["response"]["incomplete_details"]["reason"]
                        .as_str()
                        .unwrap_or("incomplete")
                        .to_string(),
                );
                final_response = Some(payload["response"].clone());
                break;
            }
            "response.failed" => {
                let error = &payload["response"]["error"];
                return Err(ReadError::Failed(stream_error(
                    error["code"].as_str().unwrap_or_default(),
                    error["message"].as_str().unwrap_or("the response failed"),
                )));
            }
            "error" => {
                // Flat `{code, message}` or nested `{error: {...}}`.
                let error = if payload["error"].is_object() {
                    &payload["error"]
                } else {
                    &payload
                };
                return Err(ReadError::Failed(stream_error(
                    error["code"].as_str().unwrap_or_default(),
                    error["message"].as_str().unwrap_or("unknown stream error"),
                )));
            }
            _ => {}
        }
    }

    let Some(response) = final_response else {
        return Err(ReadError::Failed(LlmError::Stream(
            "connection closed before the response finished".into(),
        )));
    };

    let mut output: Vec<Value> = items.into_iter().flatten().collect();
    if output.is_empty() {
        // Should not happen, but the final response repeats every item.
        output = response["output"].as_array().cloned().unwrap_or_default();
    }
    let (content, refused) = assemble(output, &model);
    let has_calls = content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }));
    let stop_reason = match incomplete_reason.as_deref() {
        Some("max_output_tokens") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_string()),
        None if has_calls => StopReason::ToolUse,
        None if refused => StopReason::Refusal,
        None => StopReason::EndTurn,
    };

    emitter
        .emit(StreamEvent::Completed(Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            usage: convert_usage(&response["usage"]),
            stop_reason,
            model: response["model"].as_str().map(str::to_string),
        }))
        .await?;
    Ok(())
}

fn convert_usage(raw: &Value) -> Usage {
    Usage {
        input_tokens: raw["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: raw["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: raw["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: raw["input_tokens_details"]["cache_write_tokens"].as_u64().unwrap_or(0),
        reasoning_tokens: raw["output_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
    }
}

/// Turn completed output items into content blocks. The second value says
/// whether the model refused.
fn assemble(output: Vec<Value>, model: &str) -> (Vec<ContentBlock>, bool) {
    let meta = |raw: Value| {
        Some(ProviderMeta {
            protocol: PROTOCOL.to_string(),
            model: model.to_string(),
            raw,
        })
    };
    let mut content = Vec::new();
    let mut refused = false;
    for item in output {
        match item["type"].as_str().unwrap_or_default() {
            "message" => {
                let mut text = String::new();
                for part in item["content"].as_array().into_iter().flatten() {
                    match part["type"].as_str().unwrap_or_default() {
                        "output_text" => text.push_str(part["text"].as_str().unwrap_or_default()),
                        "refusal" => {
                            refused = true;
                            text.push_str(part["refusal"].as_str().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
                content.push(ContentBlock::Text { text, meta: meta(item) });
            }
            "reasoning" => {
                let parts = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(item["content"].as_array().into_iter().flatten());
                let text: Vec<&str> = parts.filter_map(|part| part["text"].as_str()).collect();
                content.push(ContentBlock::Reasoning {
                    text: text.join("\n\n"),
                    meta: meta(item),
                });
            }
            "function_call" => {
                let (input, invalid_json) = http::parse_tool_arguments(item["arguments"].as_str().unwrap_or_default());
                content.push(ContentBlock::ToolCall {
                    id: item["call_id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(http::new_call_id),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                    input,
                    invalid_json,
                    meta: meta(item),
                });
            }
            _ => content.push(ContentBlock::Opaque {
                meta: meta(item).expect("meta is always built"),
            }),
        }
    }
    (content, refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swe_protocol::ToolSpec;

    fn provider() -> OpenAiResponses {
        OpenAiResponses::new(
            ProviderSettings {
                id: "openai".into(),
                protocol: Protocol::OpenaiResponses,
                base_url: "https://api.openai.com/v1".into(),
                api_key: Some("k".into()),
                headers: vec![],
            },
            reqwest::Client::new(),
        )
    }

    fn request(model: &str, effort: Effort, messages: Vec<Message>) -> ChatRequest {
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
            effort,
            cache_key: Some("session-1".into()),
        }
    }

    fn failure(param: &str, message: &str) -> ApiFailure {
        let json =
            json!({"error": {"message": message, "type": "invalid_request_error", "param": param, "code": null}});
        ApiFailure {
            status: 400,
            retry_after: None,
            should_retry: None,
            body: json.to_string(),
            json: Some(json),
        }
    }

    #[test]
    fn body_is_stateless_with_encrypted_reasoning() {
        let body = provider().build_body(&request("gpt-6.2-sol", Effort::Xhigh, vec![Message::user_text("hi")]));
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["reasoning"], json!({"effort": "xhigh", "summary": "auto"}));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["prompt_cache_key"], "session-1");
        assert_eq!(
            body["tools"][0],
            json!({"type": "function", "name": "bash", "description": "Run", "parameters": {"type": "object"}, "strict": false})
        );
        assert_eq!(
            body["input"][0],
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]})
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn rejected_reasoning_settings_degrade_step_by_step() {
        let p = provider();
        let req = request("gpt-x", Effort::Xhigh, vec![]);
        assert!(p.downgrade(
            &req,
            &failure(
                "reasoning.summary",
                "Your organization must be verified to generate reasoning summaries."
            )
        ));
        assert_eq!(p.build_body(&req)["reasoning"], json!({"effort": "xhigh"}));
        assert!(p.downgrade(
            &req,
            &failure(
                "reasoning.effort",
                "Unsupported value: 'xhigh' is not supported with this model."
            )
        ));
        assert_eq!(p.build_body(&req)["reasoning"], json!({"effort": "high"}));
        assert!(p.downgrade(
            &req,
            &failure(
                "reasoning.effort",
                "Unsupported parameter: 'reasoning.effort' is not supported with this model."
            )
        ));
        let plain = p.build_body(&req);
        assert!(plain.get("reasoning").is_none());
        assert!(plain.get("include").is_none());
        assert!(
            !p.downgrade(&req, &failure("reasoning.effort", "still unhappy")),
            "nothing left to drop"
        );
        assert!(!p.downgrade(&req, &failure("input", "Invalid input")));
        // Other models are unaffected.
        assert!(
            p.build_body(&request("gpt-6.2-sol", Effort::High, vec![]))
                .get("reasoning")
                .is_some()
        );
    }

    #[test]
    fn replays_own_items_verbatim_and_drops_foreign_reasoning() {
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "ENC"});
        let message = json!({"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "phase": "commentary",
            "content": [{"type": "output_text", "text": "Checking.", "annotations": []}]});
        let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "bash", "arguments": "{\"command\":\"ls\"}", "status": "completed"});
        let (content, refused) = assemble(vec![reasoning.clone(), message.clone(), call.clone()], "gpt-6.2-sol");
        assert!(!refused);
        assert!(matches!(&content[1], ContentBlock::Text { text, .. } if text == "Checking."));
        assert!(
            matches!(&content[2], ContentBlock::ToolCall { id, input, .. } if id == "call_1" && input["command"] == "ls")
        );

        let history = vec![
            Message::user_text("go"),
            Message {
                role: Role::Assistant,
                content,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("call_1", "README.md", false)],
            },
        ];
        let same_model = convert_messages(&history, "gpt-6.2-sol");
        assert_eq!(same_model[1..4], [reasoning, message.clone(), call.clone()]);
        assert_eq!(
            same_model[4],
            json!({"type": "function_call_output", "call_id": "call_1", "output": "README.md"})
        );

        let other_model = convert_messages(&history, "gpt-6.2-astra");
        assert_eq!(other_model[1..3], [message, call], "reasoning is bound to its model");
    }

    #[test]
    fn foreign_history_is_rebuilt_as_plain_items() {
        let history = vec![
            Message::user_text("go"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Reasoning {
                        text: "x".into(),
                        meta: Some(ProviderMeta {
                            protocol: "anthropic".into(),
                            model: "c".into(),
                            raw: json!({}),
                        }),
                    },
                    ContentBlock::text("Listing."),
                    ContentBlock::ToolCall {
                        id: "toolu_1".into(),
                        name: "bash".into(),
                        input: json!({"command": "ls"}),
                        invalid_json: None,
                        meta: None,
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("toolu_1", "nope", true)],
            },
        ];
        let items = convert_messages(&history, "gpt-6.2-sol");
        assert_eq!(items.len(), 4);
        assert_eq!(
            items[1]["content"][0],
            json!({"type": "output_text", "text": "Listing."})
        );
        assert_eq!(
            items[2],
            json!({"type": "function_call", "call_id": "toolu_1", "name": "bash", "arguments": "{\"command\":\"ls\"}"})
        );
        assert_eq!(items[3]["output"], "Error: nope");
    }

    #[test]
    fn spend_limits_are_terminal_even_as_429() {
        let json = json!({"error": {"message": "Spend limit reached", "code": "project_spend_limit_exceeded"}});
        let failure = ApiFailure {
            status: 429,
            retry_after: None,
            should_retry: None,
            body: json.to_string(),
            json: Some(json),
        };
        assert!(!map_failure(failure, "openai").is_retryable());
        assert!(matches!(
            stream_error("context_length_exceeded", "too long"),
            LlmError::ContextOverflow(_)
        ));
        assert!(stream_error("rate_limit_exceeded", "slow down").is_retryable());
        assert!(!stream_error("invalid_prompt", "no").is_retryable());
    }

    #[test]
    fn model_listing_keeps_only_text_models() {
        assert!(is_text_model("gpt-6.2-sol"));
        assert!(is_text_model("o3-pro"));
        assert!(!is_text_model("gpt-image-2"));
        assert!(!is_text_model("text-embedding-3-large"));
        assert!(!is_text_model("gpt-realtime"));
        assert_eq!(known_limits("gpt-6.2-astra"), (Some(1_050_000), Some(128_000)));
        assert_eq!(known_limits("o3"), (None, None));
    }
}
