//! A deterministic in-process provider.
//!
//! Used by the test suites and by the demo mode of the server (no API key
//! needed). Two flavours:
//!
//! * [`MockProvider::scripted`] / [`MockProvider::from_fn`] return exactly the
//!   replies a test supplies.
//! * [`MockProvider::demo`] interprets a tiny directive language in the user's
//!   prompt, so an end-to-end run through the real agent loop, tools and UI
//!   can be scripted from the outside:
//!
//!   ```text
//!   !think Looking at the layout first.
//!   !say I'll list the files.
//!   !tool list_dir {"path": "."}
//!   !then
//!   !tool write_file {"path": "hello.txt", "content": "hi\n"}
//!   !then
//!   !say All done.
//!   ```
//!
//!   Each `!then` starts a new assistant step; consecutive `!tool` lines in a
//!   step become parallel tool calls. A prompt without directives gets a
//!   short canned exploration.
//!
//!   Scripts can also live in a scenario file named by the environment
//!   variable `SWE_AGENT_DEMO_SCRIPT`. Each `## <trigger>` heading starts a
//!   scenario that plays when a prompt contains the trigger text, which lets
//!   a demo use ordinary-looking prompts.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use swe_protocol::{ContentBlock, Message, Role, StopReason, Usage};

use crate::http::{self, ReadError};
use crate::{ChatRequest, Completion, EventStream, LlmError, ModelInfo, Protocol, Provider, StreamEvent};

#[derive(Clone, Debug)]
pub enum MockReply {
    Message {
        content: Vec<ContentBlock>,
        /// Defaults to `ToolUse` when the content has tool calls, else `EndTurn`.
        stop_reason: Option<StopReason>,
        usage: Option<Usage>,
    },
    Error(LlmError),
}

impl MockReply {
    pub fn text(text: impl Into<String>) -> Self {
        MockReply::Message {
            content: vec![ContentBlock::text(text)],
            stop_reason: None,
            usage: None,
        }
    }

    pub fn tool(name: &str, input: Value) -> Self {
        MockReply::tools(vec![(name, input)])
    }

    pub fn tools(calls: Vec<(&str, Value)>) -> Self {
        MockReply::Message {
            content: calls.into_iter().map(|(name, input)| tool_call(name, input)).collect(),
            stop_reason: None,
            usage: None,
        }
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        if let MockReply::Message { usage: slot, .. } = &mut self {
            *slot = Some(usage);
        }
        self
    }
}

pub fn tool_call(name: &str, input: Value) -> ContentBlock {
    ContentBlock::ToolCall {
        id: http::new_call_id(),
        name: name.to_string(),
        input,
        invalid_json: None,
        meta: None,
    }
}

type Responder = dyn Fn(&ChatRequest) -> MockReply + Send + Sync;

pub struct MockProvider {
    id: String,
    responder: Arc<Responder>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    /// Pause between streamed words.
    chunk_delay: Duration,
    /// Pause before a reply starts, standing in for model latency.
    initial_delay: Duration,
}

impl MockProvider {
    pub fn from_fn(responder: impl Fn(&ChatRequest) -> MockReply + Send + Sync + 'static) -> Self {
        MockProvider {
            id: "mock".into(),
            responder: Arc::new(responder),
            requests: Arc::new(Mutex::new(Vec::new())),
            chunk_delay: Duration::ZERO,
            initial_delay: Duration::ZERO,
        }
    }

    /// Replies are handed out in order; once exhausted the model says "Done.".
    pub fn scripted(replies: Vec<MockReply>) -> Self {
        let queue = Mutex::new(VecDeque::from(replies));
        Self::from_fn(move |_| {
            queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| MockReply::text("Done."))
        })
    }

    /// Directive-driven demo model, see the module docs.
    pub fn demo(id: impl Into<String>) -> Self {
        let scenarios = std::env::var("SWE_AGENT_DEMO_SCRIPT")
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| parse_scenarios(&text))
            .unwrap_or_default();
        Self::demo_with_scenarios(id, scenarios)
    }

    /// Demo model with `(trigger, script)` scenarios instead of a file.
    pub fn demo_with_scenarios(id: impl Into<String>, scenarios: Vec<(String, String)>) -> Self {
        let mut provider = Self::from_fn(move |request| demo_reply(request, &scenarios));
        provider.id = id.into();
        provider.chunk_delay = Duration::from_millis(18);
        provider.initial_delay = Duration::from_millis(700);
        provider
    }

    /// Set the pause between streamed words; the pause before each reply
    /// scales with it, so zero makes the provider instantaneous.
    pub fn with_chunk_delay(mut self, delay: Duration) -> Self {
        self.chunk_delay = delay;
        self.initial_delay = delay * 40;
        self
    }

    /// Every request received so far, for assertions.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Shared handle to the request log that stays valid after the provider
    /// has been moved into an `Arc<dyn Provider>`.
    pub fn request_log(&self) -> Arc<Mutex<Vec<ChatRequest>>> {
        self.requests.clone()
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn protocol(&self) -> Protocol {
        Protocol::Mock
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let reply = (self.responder)(&request);
        let estimated_input = estimate_tokens(&request);
        self.requests.lock().unwrap().push(request.clone());
        let (content, stop_reason, usage) = match reply {
            MockReply::Error(err) => return Err(err),
            MockReply::Message {
                content,
                stop_reason,
                usage,
            } => (content, stop_reason, usage),
        };
        let delay = self.chunk_delay;
        let initial_delay = self.initial_delay;
        let model = request.model;
        Ok(http::spawn_stream(move |emitter| async move {
            pause(initial_delay).await;
            let mut output_chars = 0usize;
            let mut tool_index = 0usize;
            for block in &content {
                match block {
                    ContentBlock::Text { text, .. } => {
                        output_chars += text.len();
                        for chunk in chunks(text) {
                            pause(delay).await;
                            emitter.text(chunk).await?;
                        }
                    }
                    ContentBlock::Reasoning { text, .. } => {
                        output_chars += text.len();
                        for chunk in chunks(text) {
                            pause(delay).await;
                            emitter.reasoning(chunk).await?;
                        }
                    }
                    ContentBlock::ToolCall {
                        id,
                        name,
                        input,
                        invalid_json,
                        ..
                    } => {
                        let arguments = invalid_json.clone().unwrap_or_else(|| input.to_string());
                        output_chars += arguments.len();
                        emitter
                            .emit(StreamEvent::ToolCallStart {
                                index: tool_index,
                                id: id.clone(),
                                name: name.clone(),
                            })
                            .await?;
                        pause(delay).await;
                        emitter
                            .emit(StreamEvent::ToolCallDelta {
                                index: tool_index,
                                fragment: arguments,
                            })
                            .await?;
                        tool_index += 1;
                    }
                    _ => {}
                }
            }
            let has_tools = content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }));
            let completion = Completion {
                message: Message {
                    role: Role::Assistant,
                    content,
                },
                usage: usage.unwrap_or(Usage {
                    input_tokens: estimated_input,
                    output_tokens: (output_chars as u64).div_ceil(4),
                    ..Default::default()
                }),
                stop_reason: stop_reason.unwrap_or(if has_tools {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                }),
                model: Some(model),
            };
            emitter.emit(StreamEvent::Completed(completion)).await?;
            Ok::<(), ReadError>(())
        }))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, LlmError> {
        Ok(vec![ModelInfo {
            id: "demo".into(),
            name: "Demo model (scripted, offline)".into(),
            context_window: Some(200_000),
            max_output_tokens: Some(32_000),
        }])
    }
}

async fn pause(delay: Duration) {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
}

/// Split text into word-sized chunks, keeping the whitespace.
fn chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if ch.is_whitespace() && index > start {
            let end = index + ch.len_utf8();
            out.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Rough prompt size: four characters per token.
fn estimate_tokens(request: &ChatRequest) -> u64 {
    let mut chars = request.system.len();
    for message in &request.messages {
        for block in &message.content {
            chars += match block {
                ContentBlock::Text { text, .. } | ContentBlock::Reasoning { text, .. } => text.len(),
                ContentBlock::ToolCall { input, .. } => input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
                ContentBlock::Image { .. } => 4_000,
                ContentBlock::Opaque { .. } => 0,
            };
        }
    }
    (chars as u64).div_ceil(4)
}

/// One assistant step of a demo script.
#[derive(Default)]
struct Step {
    thinking: String,
    text: String,
    tools: Vec<(String, Value)>,
}

fn parse_script(prompt: &str) -> Option<Vec<Step>> {
    if !prompt.lines().any(|line| line.trim_start().starts_with('!')) {
        return None;
    }
    let mut steps = vec![Step::default()];
    for line in prompt.lines() {
        let line = line.trim();
        let step = steps.last_mut().expect("at least one step");
        if line == "!then" {
            steps.push(Step::default());
        } else if line == "!say" {
            // A bare `!say` is an empty line (paragraph break).
            step.text.push('\n');
        } else if let Some(rest) = line.strip_prefix("!say ") {
            push_line(&mut step.text, rest);
        } else if let Some(rest) = line.strip_prefix("!think ") {
            push_line(&mut step.thinking, rest);
        } else if let Some(rest) = line.strip_prefix("!tool ") {
            let (name, args) = rest.trim().split_once(' ').unwrap_or((rest.trim(), "{}"));
            let input = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
            step.tools.push((name.to_string(), input));
        }
    }
    Some(steps)
}

/// Split a scenario file into `(trigger, script)` pairs.
fn parse_scenarios(text: &str) -> Vec<(String, String)> {
    let mut scenarios: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if let Some(trigger) = line.strip_prefix("## ") {
            scenarios.push((trigger.trim().to_lowercase(), String::new()));
        } else if let Some((_, script)) = scenarios.last_mut() {
            script.push_str(line);
            script.push('\n');
        }
    }
    scenarios.retain(|(trigger, _)| !trigger.is_empty());
    scenarios
}

fn push_line(target: &mut String, line: &str) {
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(line);
}

fn demo_reply(request: &ChatRequest, scenarios: &[(String, String)]) -> MockReply {
    // The script lives in the most recent user message that carries text;
    // the step to play is the number of assistant turns since then.
    let prompt_index = request.messages.iter().rposition(|m| {
        // Notices the agent loop adds are not a new request from the user.
        m.role == Role::User
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text, .. } if !text.starts_with("<system-notice>")))
    });
    let (prompt, step_index) = match prompt_index {
        Some(index) => (
            request.messages[index].text(),
            request.messages[index + 1..]
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count(),
        ),
        None => (String::new(), 0),
    };

    // A prompt that carries no directives itself may name a scenario.
    let lowered = prompt.to_lowercase();
    let scenario = scenarios.iter().find(|(trigger, _)| lowered.contains(trigger.as_str()));
    let scripted = parse_script(&prompt).or_else(|| scenario.and_then(|(_, script)| parse_script(script)));
    let steps = scripted.unwrap_or_else(|| {
        vec![
            Step {
                thinking: "No model is connected, so this is the built-in demo. I'll look at the workspace.".into(),
                text: "Let me take a look at the project.".into(),
                tools: vec![("list_dir".into(), json!({"path": "."}))],
            },
            Step {
                thinking: String::new(),
                text: "That is the workspace root. This is the offline demo model: it only shows how the \
                       agent loop, tools and approvals work. Add an API key under Settings and pick a real \
                       model to get actual work done."
                    .into(),
                tools: vec![],
            },
        ]
    });

    let Some(step) = steps.get(step_index) else {
        return MockReply::text("Done.");
    };
    let mut content = Vec::new();
    if !step.thinking.is_empty() {
        content.push(ContentBlock::Reasoning {
            text: step.thinking.clone(),
            meta: None,
        });
    }
    if !step.text.is_empty() {
        content.push(ContentBlock::text(step.text.clone()));
    }
    for (name, input) in &step.tools {
        content.push(tool_call(name, input.clone()));
    }
    if content.is_empty() {
        content.push(ContentBlock::text("Done."));
    }
    MockReply::Message {
        content,
        stop_reason: None,
        usage: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect;
    use swe_protocol::Effort;

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "demo".into(),
            system: String::new(),
            messages,
            tools: vec![],
            max_output_tokens: None,
            effort: Effort::High,
            cache_key: None,
        }
    }

    #[tokio::test]
    async fn scripted_replies_come_back_in_order() {
        let provider = MockProvider::scripted(vec![
            MockReply::tool("bash", json!({"command": "ls"})),
            MockReply::text("finished"),
        ]);
        let first = collect(provider.stream(request(vec![Message::user_text("go")])).await.unwrap())
            .await
            .unwrap();
        assert_eq!(first.stop_reason, StopReason::ToolUse);
        assert_eq!(first.message.tool_calls().next().unwrap().name, "bash");
        let second = collect(provider.stream(request(vec![])).await.unwrap()).await.unwrap();
        assert_eq!(second.message.text(), "finished");
        assert_eq!(second.stop_reason, StopReason::EndTurn);
        assert_eq!(provider.requests().len(), 2);
    }

    #[tokio::test]
    async fn demo_plays_directive_script_step_by_step() {
        let provider = MockProvider::demo("mock").with_chunk_delay(Duration::ZERO);
        let prompt = "!think hm\n!say Listing.\n!tool list_dir {\"path\": \".\"}\n!tool glob {\"pattern\": \"*.rs\"}\n!then\n!say All done.";
        let mut history = vec![Message::user_text(prompt)];

        let first = collect(provider.stream(request(history.clone())).await.unwrap())
            .await
            .unwrap();
        assert_eq!(first.message.tool_calls().count(), 2);
        assert_eq!(first.message.text(), "Listing.");
        history.push(first.message.clone());
        history.push(Message {
            role: Role::User,
            content: first
                .message
                .tool_calls()
                .map(|call| ContentBlock::tool_result(call.id, "ok", false))
                .collect(),
        });

        let second = collect(provider.stream(request(history.clone())).await.unwrap())
            .await
            .unwrap();
        assert_eq!(second.message.text(), "All done.");
        assert!(!second.message.has_tool_calls());
        history.push(second.message);

        let third = collect(provider.stream(request(history)).await.unwrap()).await.unwrap();
        assert_eq!(third.message.text(), "Done.");
    }

    #[tokio::test]
    async fn scenarios_play_for_prompts_that_contain_their_trigger() {
        let scenarios = parse_scenarios(
            "ignored preamble\n## add a delete method\n!say On it.\n!tool list_dir {}\n!then\n!say Done here.\n## other thing\n!say Other.\n",
        );
        assert_eq!(scenarios.len(), 2);
        let provider = MockProvider::demo_with_scenarios("mock", scenarios).with_chunk_delay(Duration::ZERO);
        let first = collect(
            provider
                .stream(request(vec![Message::user_text(
                    "Please Add a DELETE method to the store",
                )]))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(first.message.text(), "On it.");
        assert_eq!(first.message.tool_calls().count(), 1);
        let other = collect(
            provider
                .stream(request(vec![Message::user_text("do the other thing")]))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(other.message.text(), "Other.");
        // No trigger and no directives: the canned exploration.
        let canned = collect(
            provider
                .stream(request(vec![Message::user_text("hello")]))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(canned.message.tool_calls().next().unwrap().name, "list_dir");
    }

    #[tokio::test]
    async fn streams_text_in_chunks_before_completion() {
        use futures::StreamExt;
        let provider = MockProvider::scripted(vec![MockReply::text("hello brave new world")]);
        let events: Vec<_> = provider
            .stream(request(vec![]))
            .await
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
            .await;
        let deltas: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, "hello brave new world");
        assert!(events.len() > 2);
        assert!(matches!(events.last(), Some(StreamEvent::Completed(_))));
    }
}
