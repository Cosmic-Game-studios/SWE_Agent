//! End-to-end tests of the provider adapters against a local HTTP server that
//! replays each provider's streaming wire format.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{Value, json};
use swe_llm::{ChatRequest, Completion, LlmError, Protocol, Provider, ProviderSettings, StreamEvent, build_provider};
use swe_protocol::{ContentBlock, Effort, Message, StopReason, ToolSpec, Usage};

#[derive(Clone, Debug)]
struct Captured {
    method: String,
    path: String,
    query: String,
    headers: HeaderMap,
    body: Value,
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
    headers: Vec<(&'static str, &'static str)>,
}

impl Reply {
    fn sse(body: impl Into<String>) -> Self {
        Reply {
            status: 200,
            content_type: "text/event-stream",
            body: body.into(),
            headers: vec![],
        }
    }

    fn json(status: u16, body: Value) -> Self {
        Reply {
            status,
            content_type: "application/json",
            body: body.to_string(),
            headers: vec![],
        }
    }
}

type Responder = dyn Fn(&Captured, usize) -> Reply + Send + Sync;

#[derive(Clone)]
struct ServerState {
    requests: Arc<Mutex<Vec<Captured>>>,
    responder: Arc<Responder>,
}

struct MockServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Captured>>>,
}

impl MockServer {
    /// `responder` gets each request and its zero-based sequence number.
    async fn start(responder: impl Fn(&Captured, usize) -> Reply + Send + Sync + 'static) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = ServerState {
            requests: requests.clone(),
            responder: Arc::new(responder),
        };
        let app = axum::Router::new().fallback(handle).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        MockServer { address, requests }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }

    fn requests(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }

    fn provider(&self, protocol: Protocol, base_path: &str) -> Arc<dyn Provider> {
        build_provider(ProviderSettings {
            id: "test".into(),
            protocol,
            base_url: self.url(base_path),
            api_key: Some("secret-key".into()),
            headers: vec![("x-extra".into(), "1".into())],
        })
        .unwrap()
    }
}

async fn handle(
    State(state): State<ServerState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let captured = Captured {
        method: method.to_string(),
        path: uri.path().to_string(),
        query: uri.query().unwrap_or_default().to_string(),
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    let sequence = {
        let mut requests = state.requests.lock().unwrap();
        requests.push(captured.clone());
        requests.len() - 1
    };
    let reply = (state.responder)(&captured, sequence);
    let mut response = (StatusCode::from_u16(reply.status).unwrap(), reply.body).into_response();
    response
        .headers_mut()
        .insert("content-type", reply.content_type.parse().unwrap());
    for (name, value) in reply.headers {
        response.headers_mut().insert(name, value.parse().unwrap());
    }
    response
}

/// Build an SSE body from `(event, data)` pairs; an empty event name omits the `event:` line.
fn sse(events: &[(&str, Value)]) -> String {
    let mut out = String::new();
    for (event, data) in events {
        if !event.is_empty() {
            out.push_str(&format!("event: {event}\n"));
        }
        out.push_str(&format!("data: {data}\n\n"));
    }
    out
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        system: "You are a coding agent.".into(),
        messages: vec![Message::user_text("Read src/main.rs")],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        }],
        max_output_tokens: None,
        effort: Effort::High,
        cache_key: Some("session-1".into()),
    }
}

/// Run a request to completion and return every stream item.
async fn run(provider: &Arc<dyn Provider>, request: ChatRequest) -> Vec<Result<StreamEvent, LlmError>> {
    provider
        .stream(request)
        .await
        .expect("request accepted")
        .collect()
        .await
}

fn completion(events: &[Result<StreamEvent, LlmError>]) -> Completion {
    match events.last() {
        Some(Ok(StreamEvent::Completed(done))) => done.clone(),
        other => panic!("stream did not complete: {other:?}"),
    }
}

fn text_of(events: &[Result<StreamEvent, LlmError>]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::TextDelta(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn reasoning_of(events: &[Result<StreamEvent, LlmError>]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::ReasoningDelta(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_arguments_of(events: &[Result<StreamEvent, LlmError>]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::ToolCallDelta { fragment, .. }) => Some(fragment.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_call(done: &Completion) -> (String, String, Value) {
    let call = done.message.tool_calls().next().expect("a tool call");
    (call.id.to_string(), call.name.to_string(), call.input.clone())
}

// ---------------------------------------------------------------- Anthropic

fn anthropic_stream() -> String {
    sse(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5", "content": [],
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 2000, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Need to read the file."}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "SIG123"}}),
        ),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        ),
        ("ping", json!({"type": "ping"})),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Let me "}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "look."}}),
        ),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"src/"}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "main.rs\"}"}}),
        ),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 2})),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 42}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ])
}

#[tokio::test]
async fn anthropic_streams_thinking_text_and_tool_use() {
    let server = MockServer::start(|_, _| Reply::sse(anthropic_stream())).await;
    let provider = server.provider(Protocol::Anthropic, "/v1");
    let events = run(&provider, request("claude-opus-5-5")).await;

    assert_eq!(reasoning_of(&events), "Need to read the file.");
    assert_eq!(text_of(&events), "Let me look.");
    assert_eq!(tool_arguments_of(&events), "{\"path\": \"src/main.rs\"}");
    assert!(events.iter().any(
        |e| matches!(e, Ok(StreamEvent::ToolCallStart { id, name, .. }) if id == "toolu_1" && name == "read_file")
    ));

    let done = completion(&events);
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(done.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(
        done.usage,
        Usage {
            input_tokens: 2110,
            output_tokens: 42,
            cache_read_tokens: 2000,
            cache_write_tokens: 100,
            reasoning_tokens: 0
        }
    );
    assert_eq!(
        tool_call(&done),
        ("toolu_1".into(), "read_file".into(), json!({"path": "src/main.rs"}))
    );
    match &done.message.content[0] {
        ContentBlock::Reasoning { text, meta: Some(meta) } => {
            assert_eq!(text, "Need to read the file.");
            assert_eq!(meta.protocol, "anthropic");
            assert_eq!(
                meta.raw,
                json!({"type": "thinking", "thinking": "Need to read the file.", "signature": "SIG123"})
            );
        }
        other => panic!("expected reasoning first, got {other:?}"),
    }

    let sent = &server.requests()[0];
    assert_eq!((sent.method.as_str(), sent.path.as_str()), ("POST", "/v1/messages"));
    assert_eq!(sent.headers["x-api-key"], "secret-key");
    assert_eq!(sent.headers["anthropic-version"], "2026-10-21");
    assert_eq!(sent.headers["x-extra"], "1");
    assert!(
        sent.headers.get("anthropic-beta").is_none(),
        "no beta header for gateways"
    );
    assert_eq!(sent.body["model"], "claude-opus-5-5");
    assert_eq!(sent.body["stream"], true);
    assert_eq!(sent.body["thinking"]["type"], "adaptive");
    assert_eq!(sent.body["tools"][0]["name"], "read_file");
}

#[tokio::test]
async fn anthropic_second_turn_replays_thinking_verbatim() {
    let server = MockServer::start(|_, _| Reply::sse(anthropic_stream())).await;
    let provider = server.provider(Protocol::Anthropic, "/v1");
    let first = completion(&run(&provider, request("claude-opus-5-5")).await);

    let mut follow_up = request("claude-opus-5-5");
    follow_up.messages.push(first.message.clone());
    follow_up.messages.push(Message {
        role: swe_protocol::Role::User,
        content: vec![ContentBlock::tool_result("toolu_1", "fn main() {}", false)],
    });
    run(&provider, follow_up).await;

    let sent = &server.requests()[1].body["messages"];
    assert_eq!(sent[1]["role"], "assistant");
    assert_eq!(
        sent[1]["content"][0],
        json!({"type": "thinking", "thinking": "Need to read the file.", "signature": "SIG123"})
    );
    assert_eq!(sent[1]["content"][2]["type"], "tool_use");
    assert_eq!(sent[2]["content"][0]["type"], "tool_result");
    assert_eq!(sent[2]["content"][0]["cache_control"], json!({"type": "ephemeral"}));
}

#[tokio::test]
async fn anthropic_optional_fields_are_dropped_when_the_endpoint_rejects_them() {
    // The path makes the adapter treat this server as the first-party API.
    let server = MockServer::start(|captured, _| {
        if captured.body.get("fallbacks").is_some() {
            return Reply::json(400, json!({"type": "error", "error": {"type": "invalid_request_error", "message": "fallbacks: Extra inputs are not permitted"}}));
        }
        if captured.body["tools"][0].get("eager_input_streaming").is_some() {
            return Reply::json(400, json!({"type": "error", "error": {"type": "invalid_request_error", "message": "tools.0.eager_input_streaming: Extra inputs are not permitted"}}));
        }
        Reply::sse(anthropic_stream())
    })
    .await;
    let provider = server.provider(Protocol::Anthropic, "/api.anthropic.com/v1");

    let done = completion(&run(&provider, request("claude-opus-5-5")).await);
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].headers["anthropic-beta"], "server-side-fallback-2026-07-01");
    assert_eq!(requests[0].body["fallbacks"], "default");
    assert!(requests[2].headers.get("anthropic-beta").is_none());

    // The downgrade is remembered: the next request succeeds first try.
    completion(&run(&provider, request("claude-opus-5-5")).await);
    assert_eq!(server.requests().len(), 4);
}

#[tokio::test]
async fn anthropic_errors_are_classified() {
    let server = MockServer::start(|_, sequence| match sequence {
        0 => Reply {
            status: 529,
            content_type: "application/json",
            body: json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}).to_string(),
            headers: vec![("retry-after", "7")],
        },
        1 => Reply::json(401, json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})),
        2 => Reply::json(400, json!({"type": "error", "error": {"type": "invalid_request_error", "message": "prompt is too long: 1200000 tokens > 1000000 maximum"}})),
        3 => Reply::json(400, json!({"type": "error", "error": {"type": "invalid_request_error", "message": "messages.5.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation."}})),
        4 => Reply::sse(sse(&[
            ("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {"input_tokens": 1}}})),
            ("error", json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})),
        ])),
        _ => Reply::sse(sse(&[("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {"input_tokens": 1}}}))])),
    })
    .await;
    let provider = server.provider(Protocol::Anthropic, "/v1");
    let attempt = || async { provider.stream(request("claude-opus-5-5")).await };

    let overloaded = attempt().await.err().expect("529 fails");
    assert!(overloaded.is_retryable());
    assert_eq!(overloaded.retry_after(), Some(std::time::Duration::from_secs(7)));
    assert!(matches!(attempt().await.err(), Some(LlmError::Auth(_))));
    assert!(matches!(attempt().await.err(), Some(LlmError::ContextOverflow(_))));
    assert!(matches!(attempt().await.err(), Some(LlmError::ReasoningRejected(_))));

    let mid_stream: Vec<_> = attempt().await.unwrap().collect().await;
    assert!(matches!(mid_stream.last(), Some(Err(LlmError::Stream(message))) if message.contains("Overloaded")));
    let cut_off: Vec<_> = attempt().await.unwrap().collect().await;
    assert!(
        matches!(cut_off.last(), Some(Err(LlmError::Stream(_)))),
        "EOF before message_stop is an error"
    );
}

#[tokio::test]
async fn anthropic_reports_unparseable_tool_input_and_refusals() {
    let server = MockServer::start(|_, sequence| {
        if sequence == 0 {
            Reply::sse(sse(&[
                ("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {"input_tokens": 5}}})),
                ("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "read_file", "input": {}}})),
                ("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"src/ma"}})),
                ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
                ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 9}})),
                ("message_stop", json!({"type": "message_stop"})),
            ]))
        } else {
            Reply::sse(sse(&[
                ("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {"input_tokens": 5}}})),
                ("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber", "explanation": null}}, "usage": {"output_tokens": 0}})),
                ("message_stop", json!({"type": "message_stop"})),
            ]))
        }
    })
    .await;
    let provider = server.provider(Protocol::Anthropic, "/v1");

    let truncated = completion(&run(&provider, request("claude-opus-5-5")).await);
    assert_eq!(truncated.stop_reason, StopReason::MaxTokens);
    let call = truncated.message.tool_calls().next().unwrap();
    assert_eq!(call.input, &json!({}));
    assert_eq!(call.invalid_json, Some("{\"path\": \"src/ma"));

    let refused = completion(&run(&provider, request("claude-opus-5-5")).await);
    assert_eq!(refused.stop_reason, StopReason::Refusal);
    assert!(refused.message.text().contains("cyber"));
}

#[tokio::test]
async fn anthropic_lists_models_across_pages() {
    let server = MockServer::start(|captured, _| {
        if captured.query.contains("after_id=claude-opus-5-5") {
            Reply::json(200, json!({"data": [{"id": "claude-haiku-5-5", "display_name": "Claude Haiku 5.5", "max_input_tokens": 1000000, "max_tokens": 128000}], "has_more": false, "last_id": "claude-haiku-5-5"}))
        } else {
            Reply::json(200, json!({"data": [{"id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_input_tokens": 1000000, "max_tokens": 128000}], "has_more": true, "last_id": "claude-opus-5-5"}))
        }
    })
    .await;
    let models = server.provider(Protocol::Anthropic, "/v1").list_models().await.unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["claude-opus-5-5", "claude-haiku-5-5"]
    );
    assert_eq!(models[0].name, "Claude Opus 5.5");
    assert_eq!(models[0].context_window, Some(1_000_000));
    assert_eq!(server.requests()[0].path, "/v1/models");
}

// --------------------------------------------------------- OpenAI Responses

fn responses_stream() -> String {
    let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Inspecting files."}], "encrypted_content": "ENC"});
    let message = json!({"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "phase": "commentary",
        "content": [{"type": "output_text", "text": "Reading it.", "annotations": []}]});
    let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}", "status": "completed"});
    sse(&[
        (
            "response.created",
            json!({"type": "response.created", "sequence_number": 0, "response": {"id": "resp_1", "status": "in_progress"}}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        ),
        (
            "response.reasoning_summary_part.added",
            json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "Inspecting files."}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 1, "content_index": 0, "delta": "Reading "}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 1, "content_index": 0, "delta": "it."}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 1, "item": message}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read_file", "arguments": ""}}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 2, "delta": "{\"path\":"}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 2, "delta": "\"src/main.rs\"}"}),
        ),
        (
            "response.function_call_arguments.done",
            json!({"type": "response.function_call_arguments.done", "item_id": "fc_1", "output_index": 2, "arguments": "{\"path\":\"src/main.rs\"}"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 2, "item": call}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_1", "model": "gpt-6.2-sol-2026-10-01", "status": "completed",
            "usage": {"input_tokens": 1200, "input_tokens_details": {"cached_tokens": 1024, "cache_write_tokens": 0}, "output_tokens": 350, "output_tokens_details": {"reasoning_tokens": 192}, "total_tokens": 1550}}}),
        ),
    ])
}

#[tokio::test]
async fn openai_responses_streams_and_round_trips_items() {
    let server = MockServer::start(|_, _| Reply::sse(responses_stream())).await;
    let provider = server.provider(Protocol::OpenaiResponses, "/v1");
    let events = run(&provider, request("gpt-6.2-sol")).await;

    assert_eq!(reasoning_of(&events), "Inspecting files.");
    assert_eq!(text_of(&events), "Reading it.");
    assert_eq!(tool_arguments_of(&events), "{\"path\":\"src/main.rs\"}");
    let done = completion(&events);
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(done.model.as_deref(), Some("gpt-6.2-sol-2026-10-01"));
    assert_eq!(
        done.usage,
        Usage {
            input_tokens: 1200,
            output_tokens: 350,
            cache_read_tokens: 1024,
            cache_write_tokens: 0,
            reasoning_tokens: 192
        }
    );
    assert_eq!(
        tool_call(&done),
        ("call_1".into(), "read_file".into(), json!({"path": "src/main.rs"}))
    );

    let sent = &server.requests()[0];
    assert_eq!(sent.path, "/v1/responses");
    assert_eq!(sent.headers["authorization"], "Bearer secret-key");
    assert_eq!(sent.body["store"], false);
    assert_eq!(sent.body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(sent.body["prompt_cache_key"], "session-1");

    // Second turn: the output items go back exactly as they were received.
    let mut follow_up = request("gpt-6.2-sol");
    follow_up.messages.push(done.message.clone());
    follow_up.messages.push(Message {
        role: swe_protocol::Role::User,
        content: vec![ContentBlock::tool_result("call_1", "fn main() {}", false)],
    });
    run(&provider, follow_up).await;
    let input = &server.requests()[1].body["input"];
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["encrypted_content"], "ENC");
    assert_eq!(input[2]["phase"], "commentary");
    assert_eq!(input[3]["id"], "fc_1");
    assert_eq!(
        input[4],
        json!({"type": "function_call_output", "call_id": "call_1", "output": "fn main() {}"})
    );
}

#[tokio::test]
async fn openai_responses_handles_incomplete_failed_and_rejected_settings() {
    let server = MockServer::start(|captured, sequence| match sequence {
        0 => Reply::sse(sse(&[
            ("response.output_item.done", json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "Partial"}]}})),
            ("response.incomplete", json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 5, "output_tokens": 7}}})),
        ])),
        1 => Reply::sse(sse(&[("response.failed", json!({"type": "response.failed", "response": {"error": {"code": "rate_limit_exceeded", "message": "Rate limit reached. Please try again in 1.5s."}}}))])),
        2 => Reply::sse(sse(&[("error", json!({"type": "error", "code": "context_length_exceeded", "message": "Your input exceeds the context window."}))])),
        _ => {
            if captured.body["reasoning"].get("summary").is_some() {
                Reply::json(400, json!({"error": {"message": "Your organization must be verified to generate reasoning summaries.", "type": "invalid_request_error", "param": "reasoning.summary", "code": "unsupported_value"}}))
            } else {
                Reply::sse(responses_stream())
            }
        }
    })
    .await;
    let provider = server.provider(Protocol::OpenaiResponses, "/v1");

    let incomplete = completion(&run(&provider, request("gpt-6.2-sol")).await);
    assert_eq!(incomplete.stop_reason, StopReason::MaxTokens);
    assert_eq!(incomplete.message.text(), "Partial");

    let failed = run(&provider, request("gpt-6.2-sol")).await;
    assert!(matches!(failed.last(), Some(Err(err)) if err.is_retryable()));
    let overflow = run(&provider, request("gpt-6.2-sol")).await;
    assert!(matches!(overflow.last(), Some(Err(LlmError::ContextOverflow(_)))));

    let recovered = completion(&run(&provider, request("gpt-6.2-sol")).await);
    assert_eq!(recovered.stop_reason, StopReason::ToolUse);
    let requests = server.requests();
    assert_eq!(requests.len(), 5, "one rejected attempt, one retry without the summary");
    assert_eq!(requests[4].body["reasoning"], json!({"effort": "high"}));
}

// ------------------------------------------------- OpenAI-compatible chat

fn chat_stream() -> String {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "gen-1", "object": "chat.completion.chunk", "created": 1, "model": "anthropic/claude-opus-5.5",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let mut body = String::from(": OPENROUTER PROCESSING\n\n");
    body.push_str(&sse(&[
        ("", chunk(json!({"role": "assistant", "content": "", "reasoning": "Let me ", "reasoning_details": [{"type": "reasoning.text", "text": "Let me ", "signature": null, "format": "anthropic-claude-v1", "index": 0}]}), Value::Null)),
        ("", chunk(json!({"reasoning": "check.", "reasoning_details": [{"type": "reasoning.text", "text": "check.", "index": 0}]}), Value::Null)),
        ("", chunk(json!({"reasoning_details": [{"type": "reasoning.text", "signature": "SIG", "index": 0}]}), Value::Null)),
        ("", chunk(json!({"content": "Reading "}), Value::Null)),
        ("", chunk(json!({"content": "it."}), Value::Null)),
        ("", chunk(json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": ""}}]}), Value::Null)),
        ("", chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":"}}]}), Value::Null)),
        ("", chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"src/main.rs\"}"}}]}), Value::Null)),
        ("", chunk(json!({}), json!("tool_calls"))),
        ("", json!({"id": "gen-1", "object": "chat.completion.chunk", "model": "anthropic/claude-opus-5.5",
            "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 900, "completion_tokens": 60, "total_tokens": 960, "cost": 0.0012,
                "prompt_tokens_details": {"cached_tokens": 800, "cache_write_tokens": 50}, "completion_tokens_details": {"reasoning_tokens": 20}}})),
    ]));
    body.push_str("data: [DONE]\n\n");
    body
}

#[tokio::test]
async fn chat_completions_streams_reasoning_details_and_tool_calls() {
    let server = MockServer::start(|_, _| Reply::sse(chat_stream())).await;
    let provider = server.provider(Protocol::OpenaiChat, "/openrouter.ai/api/v1");
    let events = run(&provider, request("anthropic/claude-opus-5.5")).await;

    assert_eq!(reasoning_of(&events), "Let me check.");
    assert_eq!(text_of(&events), "Reading it.");
    assert_eq!(tool_arguments_of(&events), "{\"path\":\"src/main.rs\"}");
    let done = completion(&events);
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(
        done.usage,
        Usage {
            input_tokens: 900,
            output_tokens: 60,
            cache_read_tokens: 800,
            cache_write_tokens: 50,
            reasoning_tokens: 20
        }
    );
    assert_eq!(
        tool_call(&done),
        ("call_1".into(), "read_file".into(), json!({"path": "src/main.rs"}))
    );

    let sent = &server.requests()[0];
    assert_eq!(sent.path, "/openrouter.ai/api/v1/chat/completions");
    assert_eq!(sent.headers["authorization"], "Bearer secret-key");
    assert_eq!(sent.headers["x-openrouter-title"], "SWE Agent");
    assert_eq!(sent.body["reasoning"], json!({"effort": "high"}));
    assert_eq!(sent.body["messages"][0]["role"], "system");

    // The merged reasoning_details go back unmodified with the tool-call turn.
    let mut follow_up = request("anthropic/claude-opus-5.5");
    follow_up.messages.push(done.message.clone());
    follow_up.messages.push(Message {
        role: swe_protocol::Role::User,
        content: vec![ContentBlock::tool_result("call_1", "fn main() {}", false)],
    });
    run(&provider, follow_up).await;
    let messages = &server.requests()[1].body["messages"];
    assert_eq!(
        messages[2]["reasoning_details"],
        json!([{"type": "reasoning.text", "text": "Let me check.", "signature": "SIG", "format": "anthropic-claude-v1", "index": 0}])
    );
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "fn main() {}"})
    );
}

#[tokio::test]
async fn chat_completions_tolerates_minimal_local_servers() {
    // No usage, no [DONE], no tool-call index, `stop` despite a tool call,
    // reasoning under the DeepSeek name, arguments in one piece.
    let server = MockServer::start(|captured, sequence| {
        if sequence == 0 {
            assert!(captured.body.get("stream_options").is_some());
            return Reply::json(400, json!({"error": {"message": "Unsupported parameter: stream_options"}}));
        }
        Reply::sse(sse(&[
            ("", json!({"choices": [{"delta": {"role": "assistant", "reasoning_content": "hmm"}}]})),
            ("", json!({"choices": [{"delta": {"content": "ok"}}]})),
            ("", json!({"choices": [{"delta": {"tool_calls": [{"id": "abc", "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}}]}}]})),
            ("", json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})),
        ]))
    })
    .await;
    let provider = server.provider(Protocol::OpenaiChat, "/v1");
    let events = run(&provider, request("qwen3-coder")).await;
    let done = completion(&events);
    assert_eq!(reasoning_of(&events), "hmm");
    assert_eq!(done.stop_reason, StopReason::ToolUse);
    assert_eq!(
        tool_call(&done),
        ("abc".into(), "read_file".into(), json!({"path": "a"}))
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.get("stream_options").is_none());
    assert!(
        requests[1].body.get("reasoning").is_none(),
        "no OpenRouter extras for generic servers"
    );
    assert!(requests[1].headers.get("x-openrouter-title").is_none());
}

#[tokio::test]
async fn chat_completions_surfaces_mid_stream_errors_and_lists_tool_models() {
    let server = MockServer::start(|captured, _| {
        if captured.path.ends_with("/models") {
            return Reply::json(200, json!({"data": [
                {"id": "z/with-tools", "name": "With Tools", "context_length": 200000, "top_provider": {"max_completion_tokens": 64000}, "supported_parameters": ["tools", "reasoning"]},
                {"id": "a/no-tools", "name": "No Tools", "context_length": 8000, "supported_parameters": ["temperature"]},
            ]}));
        }
        Reply::sse(sse(&[
            ("", json!({"choices": [{"delta": {"content": "par"}}]})),
            ("", json!({"error": {"code": 502, "message": "Provider returned error", "metadata": {"error_type": "provider_unavailable"}}, "choices": [{"delta": {"content": ""}, "finish_reason": "error"}]})),
        ]))
    })
    .await;
    let provider = server.provider(Protocol::OpenaiChat, "/openrouter.ai/api/v1");
    let events = run(&provider, request("m")).await;
    assert!(
        matches!(events.last(), Some(Err(LlmError::Stream(message))) if message.contains("Provider returned error"))
    );

    let models = provider.list_models().await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(
        (
            models[0].id.as_str(),
            models[0].context_window,
            models[0].max_output_tokens
        ),
        ("z/with-tools", Some(200_000), Some(64_000))
    );
}

// ------------------------------------------------------------------ Gemini

fn gemini_stream() -> String {
    sse(&[
        (
            "",
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "**Plan** read ", "thought": true}]}, "index": 0}],
            "usageMetadata": {"promptTokenCount": 1234, "totalTokenCount": 1234}, "modelVersion": "gemini-4-hydrogen", "responseId": "r1"}),
        ),
        (
            "",
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "the file.", "thought": true}]}, "index": 0}], "modelVersion": "gemini-4-hydrogen"}),
        ),
        (
            "",
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Reading it."}]}, "index": 0}], "modelVersion": "gemini-4-hydrogen"}),
        ),
        (
            "",
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"id": "abc123", "name": "read_file", "args": {"path": "src/main.rs"}}, "thoughtSignature": "EpwC"},
                {"functionCall": {"id": "def456", "name": "read_file", "args": {"path": "Cargo.toml"}}}
            ]}, "finishReason": "STOP", "index": 0}],
            "usageMetadata": {"promptTokenCount": 1234, "cachedContentTokenCount": 1024, "candidatesTokenCount": 18, "thoughtsTokenCount": 96, "totalTokenCount": 1348},
            "modelVersion": "gemini-4-hydrogen"}),
        ),
    ])
}

#[tokio::test]
async fn gemini_streams_thoughts_and_parallel_function_calls() {
    let server = MockServer::start(|_, _| Reply::sse(gemini_stream())).await;
    let provider = server.provider(Protocol::Gemini, "/v1beta");
    let events = run(&provider, request("gemini-4-hydrogen")).await;

    assert_eq!(reasoning_of(&events), "**Plan** read the file.");
    assert_eq!(text_of(&events), "Reading it.");
    let done = completion(&events);
    assert_eq!(done.stop_reason, StopReason::ToolUse, "tool turns finish with STOP");
    assert_eq!(done.model.as_deref(), Some("gemini-4-hydrogen"));
    assert_eq!(
        done.usage,
        Usage {
            input_tokens: 1234,
            output_tokens: 114,
            cache_read_tokens: 1024,
            cache_write_tokens: 0,
            reasoning_tokens: 96
        }
    );
    let calls: Vec<_> = done
        .message
        .tool_calls()
        .map(|c| (c.id.to_string(), c.input.clone()))
        .collect();
    assert_eq!(
        calls,
        [
            ("abc123".to_string(), json!({"path": "src/main.rs"})),
            ("def456".to_string(), json!({"path": "Cargo.toml"}))
        ]
    );

    let sent = &server.requests()[0];
    assert_eq!(sent.path, "/v1beta/models/gemini-4-hydrogen:streamGenerateContent");
    assert_eq!(sent.query, "alt=sse");
    assert_eq!(sent.headers["x-goog-api-key"], "secret-key");
    assert_eq!(
        sent.body["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts": true, "thinkingLevel": "high"})
    );
    assert_eq!(sent.body["tools"][0]["functionDeclarations"][0]["name"], "read_file");

    // Second turn: model parts verbatim (thought, text, signed call), then
    // both function responses in one user content with matching ids.
    let mut follow_up = request("gemini-4-hydrogen");
    follow_up.messages.push(done.message.clone());
    follow_up.messages.push(Message {
        role: swe_protocol::Role::User,
        content: vec![
            ContentBlock::tool_result("abc123", "fn main() {}", false),
            ContentBlock::tool_result("def456", "not found", true),
        ],
    });
    run(&provider, follow_up).await;
    let contents = &server.requests()[1].body["contents"];
    assert_eq!(
        contents[1],
        json!({"role": "model", "parts": [
            {"text": "**Plan** read the file.", "thought": true},
            {"text": "Reading it."},
            {"functionCall": {"id": "abc123", "name": "read_file", "args": {"path": "src/main.rs"}}, "thoughtSignature": "EpwC"},
            {"functionCall": {"id": "def456", "name": "read_file", "args": {"path": "Cargo.toml"}}},
        ]})
    );
    assert_eq!(
        contents[2],
        json!({"role": "user", "parts": [
            {"functionResponse": {"id": "abc123", "name": "read_file", "response": {"output": "fn main() {}"}}},
            {"functionResponse": {"id": "def456", "name": "read_file", "response": {"error": "not found"}}},
        ]})
    );
}

#[tokio::test]
async fn gemini_handles_blocks_empty_turns_and_errors() {
    let server = MockServer::start(|captured, sequence| match sequence {
        0 => Reply::sse(sse(&[("", json!({"promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}, "usageMetadata": {"promptTokenCount": 12, "totalTokenCount": 12}}))])),
        1 => Reply::sse(sse(&[("", json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "thinking only", "thought": true}]}, "finishReason": "STOP"}]}))])),
        2 => Reply::sse(sse(&[("", json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL", "finishMessage": "bad call"}]}))])),
        3 => Reply::sse(sse(&[
            ("", json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "par"}]}}]})),
            ("", json!({"error": {"code": 503, "message": "The model is overloaded.", "status": "UNAVAILABLE"}})),
        ])),
        // Pre-stream errors come back as plain JSON even on the SSE endpoint.
        4 => Reply { status: 429, content_type: "text/event-stream", headers: vec![],
            body: json!({"error": {"code": 429, "message": "Quota exceeded", "status": "RESOURCE_EXHAUSTED", "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "12s"}]}}).to_string() },
        5 => {
            assert_eq!(captured.body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high");
            Reply::json(400, json!({"error": {"code": 400, "message": "Thinking level is not supported for this model.", "status": "INVALID_ARGUMENT"}}))
        }
        _ => {
            assert!(captured.body["generationConfig"]["thinkingConfig"].get("thinkingLevel").is_none());
            Reply::sse(sse(&[("", json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Hello"}]}, "finishReason": "MAX_TOKENS"}]}))]))
        }
    })
    .await;
    let provider = server.provider(Protocol::Gemini, "/v1beta");

    let blocked = completion(&run(&provider, request("gemini-4-hydrogen")).await);
    assert_eq!(blocked.stop_reason, StopReason::Refusal);
    assert!(blocked.message.text().contains("PROHIBITED_CONTENT"));

    let empty = run(&provider, request("gemini-4-hydrogen")).await;
    assert!(matches!(empty.last(), Some(Err(LlmError::Stream(message))) if message.contains("empty response")));
    let malformed = run(&provider, request("gemini-4-hydrogen")).await;
    assert!(
        matches!(malformed.last(), Some(Err(LlmError::Stream(message))) if message.contains("MALFORMED_FUNCTION_CALL"))
    );
    let overloaded = run(&provider, request("gemini-4-hydrogen")).await;
    assert!(matches!(overloaded.last(), Some(Err(err)) if err.is_retryable()));

    let quota = provider
        .stream(request("gemini-4-hydrogen"))
        .await
        .err()
        .expect("429 fails");
    assert!(quota.is_retryable());
    assert_eq!(quota.retry_after(), Some(std::time::Duration::from_secs(12)));

    let recovered = completion(&run(&provider, request("gemini-4-hydrogen")).await);
    assert_eq!(recovered.stop_reason, StopReason::MaxTokens);
    assert_eq!(recovered.message.text(), "Hello");
    assert_eq!(server.requests().len(), 7);
}

#[tokio::test]
async fn gemini_lists_generation_models() {
    let server = MockServer::start(|captured, _| {
        if captured.query.contains("pageToken=next") {
            Reply::json(200, json!({"models": [{"name": "models/text-embedding-005", "displayName": "Embedding", "supportedGenerationMethods": ["embedContent"]}]}))
        } else {
            Reply::json(200, json!({"models": [{"name": "models/gemini-4-hydrogen", "displayName": "Gemini 4 Hydrogen", "inputTokenLimit": 1048576, "outputTokenLimit": 65536, "supportedGenerationMethods": ["generateContent", "countTokens"]}], "nextPageToken": "next"}))
        }
    })
    .await;
    let models = server
        .provider(Protocol::Gemini, "/v1beta")
        .list_models()
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(
        (models[0].id.as_str(), models[0].name.as_str(), models[0].context_window),
        ("gemini-4-hydrogen", "Gemini 4 Hydrogen", Some(1_048_576))
    );
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn dropping_the_stream_cancels_the_request() {
    // A server that sends one event and then keeps the connection open.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0u8; 16384];
        let _ = socket.read(&mut buffer).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        let event = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let _ = socket
            .write_all(format!("{head}{:x}\r\n{event}\r\n", event.len()).as_bytes())
            .await;
        // Resolves once the client goes away.
        while matches!(socket.read(&mut buffer).await, Ok(n) if n > 0) {}
        let _ = closed_tx.send(());
    });
    let provider = build_provider(ProviderSettings {
        id: "local".into(),
        protocol: Protocol::OpenaiChat,
        base_url: format!("http://{address}/v1"),
        api_key: None,
        headers: vec![],
    })
    .unwrap();
    let mut stream = provider.stream(request("m")).await.unwrap();
    assert!(matches!(stream.next().await, Some(Ok(StreamEvent::TextDelta(text))) if text == "hi"));
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(5), closed_rx)
        .await
        .expect("connection is closed after the stream is dropped")
        .unwrap();
}
