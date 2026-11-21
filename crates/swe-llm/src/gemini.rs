//! Google Gemini via `models/{model}:streamGenerateContent?alt=sse`.
//!
//! Notes on the less obvious parts:
//!
//! * Gemini 3 and later validate *thought signatures* on function-call turns.
//!   The signature may sit on the function-call part or on a thought part, so
//!   model parts are stored raw and replayed verbatim. Function calls that
//!   came from another provider get the bypass value Google's own CLI uses.
//! * Function-call arguments arrive whole (never as partial JSON), and a
//!   tool-calling turn finishes with `STOP` like any other.
//! * Sampling parameters are not sent: current models ignore them.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use swe_protocol::{ContentBlock, Effort, Message, ProviderMeta, Role, StopReason, Usage};

use crate::http::{self, ApiFailure, Emitter, ReadError, SseReader};
use crate::{
    ChatRequest, Completion, EventStream, LlmError, ModelInfo, Protocol, Provider, ProviderSettings, StreamEvent,
};

const PROTOCOL: &str = "gemini";
/// Accepted by the API in place of a real signature (used by Gemini CLI).
const SIGNATURE_BYPASS: &str = "skip_thought_signature_validator";

pub struct Gemini {
    settings: ProviderSettings,
    client: reqwest::Client,
    /// Models that rejected `thinkingLevel`; they are asked without it.
    no_thinking_level: Mutex<HashSet<String>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Thinking {
    /// No thinking configuration at all (pre-2.5 and non-Gemini models).
    None,
    /// 2.5 generation: budget-based, `thinkingLevel` is an error.
    Budget,
    /// Gemini 3 and later: level-based.
    Level { xhigh: bool },
}

fn thinking_style(model: &str) -> Thinking {
    let model = model.trim_start_matches("models/");
    if !model.starts_with("gemini-") {
        return Thinking::None;
    }
    if model.starts_with("gemini-1.") || model.starts_with("gemini-2.0") {
        return Thinking::None;
    }
    if model.starts_with("gemini-2.") {
        return Thinking::Budget;
    }
    Thinking::Level {
        xhigh: model.contains("hydrogen") || model.contains("neon"),
    }
}

impl Gemini {
    pub fn new(settings: ProviderSettings, client: reqwest::Client) -> Self {
        Gemini {
            settings,
            client,
            no_thinking_level: Mutex::new(HashSet::new()),
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
            .header("x-goog-api-key", key);
        for (name, value) in &self.settings.headers {
            builder = builder.header(name, value);
        }
        Ok(builder)
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let mut body = Map::new();
        if !request.system.is_empty() {
            body.insert("systemInstruction".into(), json!({"parts": [{"text": request.system}]}));
        }
        body.insert("contents".into(), Value::Array(convert_messages(&request.messages)));
        if !request.tools.is_empty() {
            let declarations: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parametersJsonSchema": clean_schema(&tool.input_schema),
                    })
                })
                .collect();
            body.insert("tools".into(), json!([{"functionDeclarations": declarations}]));
        }

        let mut generation = Map::new();
        if let Some(max) = request.max_output_tokens {
            generation.insert("maxOutputTokens".into(), json!(max));
        }
        let style = thinking_style(&request.model);
        let level_rejected = self.no_thinking_level.lock().unwrap().contains(&request.model);
        match style {
            Thinking::None => {}
            Thinking::Budget => {
                generation.insert("thinkingConfig".into(), json!({"includeThoughts": true}));
            }
            Thinking::Level { .. } if level_rejected => {
                generation.insert("thinkingConfig".into(), json!({"includeThoughts": true}));
            }
            Thinking::Level { xhigh } => {
                let level = match request.effort {
                    Effort::Low => "low",
                    Effort::Medium => "medium",
                    Effort::High => "high",
                    Effort::Xhigh if xhigh => "xhigh",
                    Effort::Xhigh => "high",
                };
                generation.insert(
                    "thinkingConfig".into(),
                    json!({"includeThoughts": true, "thinkingLevel": level}),
                );
            }
        }
        if !generation.is_empty() {
            body.insert("generationConfig".into(), Value::Object(generation));
        }
        Value::Object(body)
    }
}

/// `$schema` is not part of the accepted subset; everything else passes through.
fn clean_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
    }
    schema
}

fn map_failure(failure: ApiFailure, provider: &str) -> LlmError {
    let error = failure.json.as_ref().map(|json| json["error"].clone()).unwrap_or(Value::Null);
    let mut retry_after = failure.retry_after;
    let mut daily_quota = false;
    let mut invalid_key = false;
    for detail in error["details"].as_array().into_iter().flatten() {
        let kind = detail["@type"].as_str().unwrap_or_default();
        if kind.ends_with("RetryInfo") {
            if let Some(delay) = detail["retryDelay"].as_str().and_then(parse_duration) {
                retry_after = Some(delay);
            }
        } else if kind.ends_with("QuotaFailure") {
            daily_quota |= detail["violations"].as_array().into_iter().flatten().any(|violation| {
                let quota = violation["quotaId"].as_str().unwrap_or_default();
                quota.contains("PerDay") || quota.contains("Daily")
            });
        } else if kind.ends_with("ErrorInfo") {
            invalid_key |= detail["reason"] == "API_KEY_INVALID";
        }
    }
    if invalid_key {
        return LlmError::Auth(format!("{provider}: {}", failure.message()));
    }
    match failure.into_error(provider) {
        LlmError::Api { provider, status, message, retryable, .. } => LlmError::Api {
            provider,
            status,
            message,
            // A per-day quota will not recover within any sensible backoff.
            retryable: retryable && !daily_quota,
            retry_after,
        },
        other => other,
    }
}

/// Parse a protobuf duration such as `34s` or `1.5s`.
fn parse_duration(text: &str) -> Option<Duration> {
    let seconds: f64 = text.trim().strip_suffix('s')?.parse().ok()?;
    (seconds >= 0.0).then(|| Duration::from_millis((seconds * 1000.0) as u64))
}

#[async_trait]
impl Provider for Gemini {
    fn id(&self) -> &str {
        &self.settings.id
    }

    fn protocol(&self) -> Protocol {
        Protocol::Gemini
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let model = request.model.trim_start_matches("models/").to_string();
        for _ in 0..2 {
            let body = self.build_body(&request);
            let builder = self
                .request(reqwest::Method::POST, &format!("/models/{model}:streamGenerateContent"))?
                .query(&[("alt", "sse")])
                .json(&body);
            match http::send(builder).await? {
                Ok(response) => {
                    let model = model.clone();
                    return Ok(http::spawn_stream(move |emitter| {
                        read_stream(SseReader::new(response), emitter, model)
                    }));
                }
                Err(failure) => {
                    let message = failure.message().to_ascii_lowercase();
                    let about_level = message.contains("thinking_level") || message.contains("thinkinglevel")
                        || message.contains("thinking level");
                    if failure.status == 400
                        && about_level
                        && self.no_thinking_level.lock().unwrap().insert(request.model.clone())
                    {
                        tracing::warn!(model = %request.model, "model rejected thinkingLevel, retrying without it");
                        continue;
                    }
                    return Err(map_failure(failure, &self.settings.id));
                }
            }
        }
        Err(LlmError::Protocol("request kept being rejected after downgrade".into()))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        let mut models = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut builder = self
                .request(reqwest::Method::GET, "/models")?
                .query(&[("pageSize", "1000")]);
            if let Some(token) = &page_token {
                builder = builder.query(&[("pageToken", token.as_str())]);
            }
            let response = match http::send(builder).await? {
                Ok(response) => response,
                Err(failure) => return Err(map_failure(failure, &self.settings.id)),
            };
            let page: Value = response.json().await.map_err(http::network_error)?;
            for entry in page["models"].as_array().into_iter().flatten() {
                let generates = entry["supportedGenerationMethods"]
                    .as_array()
                    .is_some_and(|methods| methods.iter().any(|m| m == "generateContent"));
                let Some(name) = entry["name"].as_str() else { continue };
                if !generates {
                    continue;
                }
                let id = name.trim_start_matches("models/");
                models.push(ModelInfo {
                    id: id.to_string(),
                    name: entry["displayName"].as_str().unwrap_or(id).to_string(),
                    context_window: entry["inputTokenLimit"].as_u64(),
                    max_output_tokens: entry["outputTokenLimit"].as_u64(),
                });
            }
            match page["nextPageToken"].as_str() {
                Some(token) if !token.is_empty() => page_token = Some(token.to_string()),
                _ => break,
            }
        }
        Ok(models)
    }
}

/// Translate the neutral history into `contents`.
fn convert_messages(messages: &[Message]) -> Vec<Value> {
    // Function responses need the function name and must echo the call id
    // exactly when the model supplied one.
    let mut calls: HashMap<&str, (&str, Option<String>)> = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolCall { id, name, meta, .. } = block {
                let wire_id = match meta.as_ref().filter(|m| m.protocol == PROTOCOL) {
                    Some(meta) => meta.raw["functionCall"]["id"].as_str().map(str::to_string),
                    None => Some(id.clone()),
                };
                calls.insert(id.as_str(), (name.as_str(), wire_id));
            }
        }
    }

    let mut out: Vec<(Role, Vec<Value>)> = Vec::new();
    for message in messages {
        let mut parts = Vec::new();
        match message.role {
            Role::User => {
                // All function responses of a step go first, in one content.
                for block in &message.content {
                    if let ContentBlock::ToolResult { tool_call_id, content, is_error } = block {
                        let (name, wire_id) = calls
                            .get(tool_call_id.as_str())
                            .cloned()
                            .unwrap_or(("unknown_tool", None));
                        let key = if *is_error { "error" } else { "output" };
                        let mut response = json!({"name": name, "response": {key: content}});
                        if let Some(id) = wire_id {
                            response["id"] = json!(id);
                        }
                        parts.push(json!({"functionResponse": response}));
                    }
                }
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text, .. } if !text.is_empty() => parts.push(json!({"text": text})),
                        ContentBlock::Image { media_type, data } => {
                            parts.push(json!({"inlineData": {"mimeType": media_type, "data": data}}))
                        }
                        _ => {}
                    }
                }
            }
            Role::Assistant => {
                for block in &message.content {
                    let own_raw = |meta: &Option<ProviderMeta>| {
                        meta.as_ref().filter(|m| m.protocol == PROTOCOL).map(|m| m.raw.clone())
                    };
                    match block {
                        ContentBlock::Text { text, meta } => match own_raw(meta) {
                            Some(raw) => parts.push(raw),
                            None if !text.is_empty() => parts.push(json!({"text": text})),
                            None => {}
                        },
                        ContentBlock::Reasoning { meta, .. } => {
                            if let Some(raw) = own_raw(meta) {
                                parts.push(raw);
                            }
                        }
                        ContentBlock::ToolCall { id, name, input, meta, .. } => match own_raw(meta) {
                            Some(raw) => parts.push(raw),
                            None => parts.push(json!({"functionCall": {"id": id, "name": name, "args": input}})),
                        },
                        ContentBlock::Opaque { meta } => {
                            if meta.protocol == PROTOCOL {
                                parts.push(meta.raw.clone());
                            }
                        }
                        ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {}
                    }
                }
                // A function-call step without any signature (foreign history,
                // or a model that does not sign) would be rejected by Gemini 3+.
                let signed = parts.iter().any(|part| part.get("thoughtSignature").is_some());
                if !signed {
                    if let Some(call) = parts.iter_mut().find(|part| part.get("functionCall").is_some()) {
                        call["thoughtSignature"] = json!(SIGNATURE_BYPASS);
                    }
                }
            }
        }
        if parts.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((role, existing)) if *role == message.role => existing.extend(parts),
            _ => out.push((message.role, parts)),
        }
    }

    out.into_iter()
        .map(|(role, parts)| {
            json!({
                "role": match role { Role::User => "user", Role::Assistant => "model" },
                "parts": parts,
            })
        })
        .collect()
}

async fn read_stream(mut sse: SseReader, emitter: Emitter, requested_model: String) -> Result<(), ReadError> {
    let mut parts: Vec<Value> = Vec::new();
    let mut usage = Usage::default();
    let mut finish: Option<String> = None;
    let mut finish_message: Option<String> = None;
    let mut block_reason: Option<String> = None;
    let mut model = requested_model;
    let mut tool_index = 0usize;

    while let Some(event) = sse.next().await? {
        if event.data.is_empty() {
            continue;
        }
        let chunk: Value = serde_json::from_str(&event.data)
            .map_err(|e| LlmError::Protocol(format!("invalid JSON in stream chunk: {e}")))?;
        if chunk.get("error").is_some() {
            let status = chunk["error"]["code"].as_u64().unwrap_or(500) as u16;
            let failure = ApiFailure {
                status,
                retry_after: None,
                should_retry: None,
                body: event.data.clone(),
                json: Some(chunk),
            };
            return Err(ReadError::Failed(match map_failure(failure, "gemini") {
                LlmError::Api { message, retryable: true, .. } => LlmError::Stream(message),
                other => other,
            }));
        }
        if let Some(served) = chunk["modelVersion"].as_str() {
            model = served.to_string();
        }
        if let Some(reason) = chunk["promptFeedback"]["blockReason"].as_str() {
            block_reason = Some(reason.to_string());
        }
        if chunk["usageMetadata"].is_object() {
            usage = convert_usage(&chunk["usageMetadata"]);
        }
        let candidate = &chunk["candidates"][0];
        for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
            if let Some(call) = part.get("functionCall") {
                emitter
                    .emit(StreamEvent::ToolCallStart {
                        index: tool_index,
                        id: call["id"].as_str().unwrap_or_default().to_string(),
                        name: call["name"].as_str().unwrap_or_default().to_string(),
                    })
                    .await?;
                emitter
                    .emit(StreamEvent::ToolCallDelta {
                        index: tool_index,
                        fragment: call.get("args").map(Value::to_string).unwrap_or_else(|| "{}".into()),
                    })
                    .await?;
                tool_index += 1;
            } else if let Some(text) = part["text"].as_str() {
                if part["thought"] == true {
                    emitter.reasoning(text).await?;
                } else {
                    emitter.text(text).await?;
                }
            }
            push_part(&mut parts, part.clone());
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            finish = Some(reason.to_string());
            finish_message = candidate["finishMessage"].as_str().map(str::to_string);
        }
    }

    let mut content = assemble(parts, &model);
    let has_calls = content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }));
    let has_text = content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if !text.trim().is_empty()));

    let stop_reason = match (finish.as_deref(), &block_reason) {
        (None, Some(reason)) => {
            content.push(ContentBlock::text(format!("[The prompt was blocked by the provider: {reason}]")));
            StopReason::Refusal
        }
        (None, None) => {
            return Err(ReadError::Failed(LlmError::Stream(
                "connection closed before the response finished".into(),
            )));
        }
        (Some("STOP"), _) if has_calls => StopReason::ToolUse,
        (Some("STOP"), _) if has_text => StopReason::EndTurn,
        (Some("STOP"), _) => {
            // Known failure mode: the model stops having produced nothing
            // (or only thoughts). Asking again usually works.
            return Err(ReadError::Failed(LlmError::Stream("the model returned an empty response".into())));
        }
        (Some("MAX_TOKENS"), _) => StopReason::MaxTokens,
        (Some(reason @ ("MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "MALFORMED_RESPONSE")), _) => {
            let detail = finish_message.unwrap_or_default();
            return Err(ReadError::Failed(LlmError::Stream(format!("{reason} {detail}").trim().to_string())));
        }
        (Some(reason @ ("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "LANGUAGE")), _) => {
            content.push(ContentBlock::text(format!("[The response was blocked by the provider: {reason}]")));
            StopReason::Refusal
        }
        (Some(other), _) if has_calls => {
            tracing::debug!(finish_reason = other, "unrecognised finish reason on a tool-call turn");
            StopReason::ToolUse
        }
        (Some(other), _) => StopReason::Other(other.to_string()),
    };

    emitter
        .emit(StreamEvent::Completed(Completion {
            message: Message { role: Role::Assistant, content },
            usage,
            stop_reason,
            model: Some(model),
        }))
        .await?;
    Ok(())
}

fn convert_usage(raw: &Value) -> Usage {
    let count = |key: &str| raw[key].as_u64().unwrap_or(0);
    let thoughts = count("thoughtsTokenCount");
    Usage {
        // promptTokenCount already includes cached tokens.
        input_tokens: count("promptTokenCount") + count("toolUsePromptTokenCount"),
        output_tokens: count("candidatesTokenCount") + thoughts,
        cache_read_tokens: count("cachedContentTokenCount"),
        cache_write_tokens: 0,
        reasoning_tokens: thoughts,
    }
}

/// Append a streamed part, merging it into the previous one when both are
/// unsigned text of the same kind. Signed parts are never merged: the
/// signature belongs to exactly the part it arrived on.
fn push_part(parts: &mut Vec<Value>, part: Value) {
    let plain_text = |p: &Value| {
        p.get("text").is_some_and(Value::is_string)
            && p.get("thoughtSignature").is_none()
            && p.as_object().is_some_and(|o| o.keys().all(|k| k == "text" || k == "thought"))
    };
    if let Some(last) = parts.last_mut() {
        if plain_text(last) && plain_text(&part) && last["thought"] == part["thought"] {
            let merged = format!(
                "{}{}",
                last["text"].as_str().unwrap_or_default(),
                part["text"].as_str().unwrap_or_default()
            );
            last["text"] = Value::String(merged);
            return;
        }
    }
    parts.push(part);
}

fn assemble(parts: Vec<Value>, model: &str) -> Vec<ContentBlock> {
    let meta = |raw: Value| Some(ProviderMeta { protocol: PROTOCOL.to_string(), model: model.to_string(), raw });
    let mut content = Vec::new();
    for part in parts {
        if let Some(call) = part.get("functionCall") {
            let input = match call.get("args") {
                Some(args @ Value::Object(_)) => args.clone(),
                _ => json!({}),
            };
            content.push(ContentBlock::ToolCall {
                id: call["id"].as_str().map(str::to_string).unwrap_or_else(http::new_call_id),
                name: call["name"].as_str().unwrap_or_default().to_string(),
                input,
                invalid_json: None,
                meta: meta(part),
            });
        } else if let Some(text) = part["text"].as_str() {
            let text = text.to_string();
            if part["thought"] == true {
                content.push(ContentBlock::Reasoning { text, meta: meta(part) });
            } else if part.get("thoughtSignature").is_some() {
                content.push(ContentBlock::Text { text, meta: meta(part) });
            } else if !text.is_empty() {
                content.push(ContentBlock::Text { text, meta: None });
            }
        } else {
            content.push(ContentBlock::Opaque { meta: meta(part).expect("meta is always built") });
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use swe_protocol::ToolSpec;

    fn provider() -> Gemini {
        Gemini::new(
            ProviderSettings {
                id: "google".into(),
                protocol: Protocol::Gemini,
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                api_key: Some("k".into()),
                headers: vec![],
            },
            reqwest::Client::new(),
        )
    }

    fn request(model: &str, effort: Effort) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            system: "sys".into(),
            messages: vec![Message::user_text("hi")],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "Read".into(),
                input_schema: json!({"$schema": "x", "type": "object", "additionalProperties": false}),
            }],
            max_output_tokens: None,
            effort,
            cache_key: None,
        }
    }

    #[test]
    fn thinking_config_follows_model_generation() {
        let p = provider();
        let hydrogen = p.build_body(&request("gemini-4-hydrogen", Effort::Xhigh));
        assert_eq!(hydrogen["generationConfig"]["thinkingConfig"], json!({"includeThoughts": true, "thinkingLevel": "xhigh"}));
        let carbon = p.build_body(&request("gemini-4-carbon", Effort::Xhigh));
        assert_eq!(carbon["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high");
        let legacy = p.build_body(&request("gemini-2.5-pro", Effort::High));
        assert_eq!(legacy["generationConfig"]["thinkingConfig"], json!({"includeThoughts": true}));
        let other = p.build_body(&request("gemma-3-27b-it", Effort::High));
        assert!(other.get("generationConfig").is_none());
        assert!(hydrogen["generationConfig"].get("temperature").is_none());
        assert!(hydrogen["generationConfig"].get("maxOutputTokens").is_none());
    }

    #[test]
    fn tools_use_json_schema_without_dollar_schema() {
        let body = provider().build_body(&request("gemini-4-hydrogen", Effort::High));
        let declaration = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(declaration["parametersJsonSchema"], json!({"type": "object", "additionalProperties": false}));
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "sys");
    }

    #[test]
    fn replays_own_parts_verbatim_and_pairs_responses() {
        let signed_call = json!({"functionCall": {"id": "g1", "name": "read_file", "args": {"path": "a"}}, "thoughtSignature": "SIG"});
        let unsigned_call = json!({"functionCall": {"name": "read_file", "args": {"path": "b"}}});
        let meta = |raw: &Value| Some(ProviderMeta { protocol: "gemini".into(), model: "m".into(), raw: raw.clone() });
        let history = vec![
            Message::user_text("go"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolCall { id: "g1".into(), name: "read_file".into(), input: json!({"path": "a"}), invalid_json: None, meta: meta(&signed_call) },
                    ContentBlock::ToolCall { id: "call_local".into(), name: "read_file".into(), input: json!({"path": "b"}), invalid_json: None, meta: meta(&unsigned_call) },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::tool_result("g1", "A", false),
                    ContentBlock::tool_result("call_local", "missing", true),
                ],
            },
        ];
        let contents = convert_messages(&history);
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"], json!([signed_call, unsigned_call]));
        assert_eq!(
            contents[2]["parts"],
            json!([
                {"functionResponse": {"id": "g1", "name": "read_file", "response": {"output": "A"}}},
                {"functionResponse": {"name": "read_file", "response": {"error": "missing"}}},
            ])
        );
    }

    #[test]
    fn foreign_tool_calls_get_the_signature_bypass_once_per_step() {
        let call = |id: &str| ContentBlock::ToolCall {
            id: id.into(),
            name: "bash".into(),
            input: json!({"command": "ls"}),
            invalid_json: None,
            meta: None,
        };
        let history = vec![
            Message::user_text("go"),
            Message { role: Role::Assistant, content: vec![ContentBlock::text("ok"), call("toolu_1"), call("toolu_2")] },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("toolu_1", "x", false), ContentBlock::tool_result("toolu_2", "y", false)],
            },
        ];
        let contents = convert_messages(&history);
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(parts[1]["thoughtSignature"], SIGNATURE_BYPASS);
        assert!(parts[2].get("thoughtSignature").is_none());
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["id"], "toolu_1");
    }

    #[test]
    fn streamed_parts_merge_only_when_unsigned() {
        let mut parts = Vec::new();
        push_part(&mut parts, json!({"text": "Pla", "thought": true}));
        push_part(&mut parts, json!({"text": "n", "thought": true}));
        push_part(&mut parts, json!({"text": "Hel"}));
        push_part(&mut parts, json!({"text": "lo"}));
        push_part(&mut parts, json!({"text": "", "thoughtSignature": "S"}));
        assert_eq!(
            parts,
            vec![json!({"text": "Plan", "thought": true}), json!({"text": "Hello"}), json!({"text": "", "thoughtSignature": "S"})]
        );
        let content = assemble(parts, "m");
        assert!(matches!(&content[0], ContentBlock::Reasoning { text, .. } if text == "Plan"));
        assert!(matches!(&content[1], ContentBlock::Text { text, meta: None } if text == "Hello"));
        assert!(matches!(&content[2], ContentBlock::Text { meta: Some(_), .. }));
    }

    #[test]
    fn quota_errors_carry_retry_delay_and_daily_limits_are_final() {
        let body = json!({"error": {"code": 429, "message": "Quota exceeded", "status": "RESOURCE_EXHAUSTED", "details": [
            {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "34.5s"}
        ]}});
        let failure = |json: Value| ApiFailure { status: 429, retry_after: None, should_retry: None, body: json.to_string(), json: Some(json) };
        let err = map_failure(failure(body), "google");
        assert!(err.is_retryable());
        assert_eq!(err.retry_after(), Some(Duration::from_millis(34_500)));

        let daily = json!({"error": {"code": 429, "message": "Quota exceeded", "details": [
            {"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [{"quotaId": "GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]}
        ]}});
        assert!(!map_failure(failure(daily), "google").is_retryable());

        let bad_key = json!({"error": {"code": 400, "message": "API key not valid.", "details": [
            {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID"}
        ]}});
        let mut f = failure(bad_key);
        f.status = 400;
        assert!(matches!(map_failure(f, "google"), LlmError::Auth(_)));
    }
}
