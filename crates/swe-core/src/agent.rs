//! The agent loop: ask the model, run the tools it calls, repeat until it
//! has nothing left to do.
//!
//! The same loop drives the root agent of a session and every sub-agent; the
//! differences (steering messages, checkpoints) hang off [`AgentSpec`].
//!
//! Histories are strictly append-only between compactions. Providers cache
//! prompts by prefix and current Claude models bind their reasoning blocks to
//! the exact conversation that produced them, so nothing that was already
//! sent is ever edited, reordered or dropped here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::future::join_all;
use serde_json::{Value, json};
use swe_llm::{ChatRequest, Completion, LlmError, Provider, StreamEvent};
use swe_protocol::{
    ApprovalDecision, ContentBlock, Effort, EventKind, Message, ROOT_AGENT, Role, StopReason, ToolSpec, TurnOutcome,
    Usage,
};
use swe_tools::text::truncate_middle;
use swe_tools::{OutputSink, Tool, ToolContext, ToolOutput, ToolRegistry};
use tokio_util::sync::CancellationToken;

use crate::compaction;
use crate::permissions::Verdict;
use crate::session::{ApprovalOutcome, ManagerInner, Session, UserInput};

/// Attempts for one model request before the turn fails.
const MAX_RETRIES: u32 = 8;
/// A provider asking us to wait longer than this is treated as a failure.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(600);
/// How often in a row a response may hit the output limit.
const MAX_CONTINUATIONS: u32 = 3;
/// Upper bound for a single tool result, for the model and the event log.
const MAX_TOOL_RESULT_CHARS: usize = 100_000;

/// Everything an agent needs from its surroundings.
#[derive(Clone)]
pub(crate) struct Env {
    pub manager: Arc<ManagerInner>,
    pub session: Arc<Session>,
    /// Fires when the user stops the turn.
    pub cancel: CancellationToken,
}

/// Static description of one agent for the duration of a run.
pub(crate) struct AgentSpec {
    pub agent_id: String,
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub effort: Effort,
    pub system: String,
    pub tools: ToolRegistry,
    /// Working directory: the workspace, or the agent's worktree.
    pub cwd: PathBuf,
    /// Compact once the context in use exceeds this many tokens.
    pub context_limit: u64,
    pub max_steps: u32,
}

pub(crate) struct TurnResult {
    pub outcome: TurnOutcome,
    /// Usage of this agent's own requests.
    pub usage: Usage,
    /// Text of the last assistant message that had any.
    pub final_text: String,
    pub error: Option<String>,
}

/// Called after every completed step with the current history, so a crash
/// loses at most the step in flight.
pub(crate) type Checkpoint<'a> = &'a (dyn Fn(&[Message]) + Send + Sync);

struct Call {
    id: String,
    name: String,
    input: Value,
    invalid_json: Option<String>,
}

enum RequestError {
    /// The user stopped the turn; `partial` is the text streamed so far.
    Cancelled { partial: String },
    Failed(String),
}

/// Run `spec` on `history` (which must end with a user message) until the
/// model stops calling tools, the user cancels, or something fails.
pub(crate) async fn run_agent(
    env: &Env,
    spec: &AgentSpec,
    history: &mut Vec<Message>,
    checkpoint: Option<Checkpoint<'_>>,
) -> TurnResult {
    let is_root = spec.agent_id == ROOT_AGENT;
    let session = &env.session;
    let tool_specs = spec.tools.specs();
    let mut usage = Usage::default();
    let mut final_text = String::new();
    let mut context_tokens = if is_root { session.meta().context_tokens } else { 0 };
    let mut continuations = 0u32;
    let mut steps = 0u32;

    let finish = |outcome: TurnOutcome, usage: Usage, final_text: String, error: Option<String>| TurnResult {
        outcome,
        usage,
        final_text,
        error,
    };
    let save = |history: &[Message]| {
        if let Some(checkpoint) = checkpoint {
            checkpoint(history);
        }
    };

    loop {
        if env.cancel.is_cancelled() {
            return finish(TurnOutcome::Cancelled, usage, final_text, None);
        }
        if is_root {
            inject_steering(session, history);
        }
        if steps >= spec.max_steps {
            let message = format!("Stopped after {steps} steps (the per-turn step limit). Send a message to continue.");
            session.emit(&spec.agent_id, EventKind::Notice { text: message });
            return finish(TurnOutcome::StepLimit, usage, final_text, None);
        }
        steps += 1;

        if context_tokens > spec.context_limit {
            match compaction::compact(env, spec, history, context_tokens).await {
                Ok(spent) => {
                    usage.add(&spent);
                    session.add_usage(&spent);
                    save(history);
                }
                Err(compaction::CompactError::Cancelled) => {
                    return finish(TurnOutcome::Cancelled, usage, final_text, None);
                }
                Err(compaction::CompactError::Failed(message)) => {
                    let message = format!("Could not summarise the conversation to free context: {message}");
                    session.emit(&spec.agent_id, EventKind::Error { message: message.clone() });
                    return finish(TurnOutcome::Error, usage, final_text, Some(message));
                }
            }
        }

        let completion = match request(env, spec, history, &tool_specs, &mut usage).await {
            Ok(completion) => completion,
            Err(RequestError::Cancelled { partial }) => {
                if !partial.trim().is_empty() {
                    // Keep what the user already saw on screen.
                    let message = Message::assistant_text(partial.clone());
                    history.push(message.clone());
                    session.emit(
                        &spec.agent_id,
                        EventKind::AssistantMessage {
                            message,
                            usage: Usage::default(),
                            stop_reason: StopReason::Other("cancelled".into()),
                        },
                    );
                    final_text = partial;
                    save(history);
                }
                return finish(TurnOutcome::Cancelled, usage, final_text, None);
            }
            Err(RequestError::Failed(message)) => {
                session.emit(&spec.agent_id, EventKind::Error { message: message.clone() });
                return finish(TurnOutcome::Error, usage, final_text, Some(message));
            }
        };

        usage.add(&completion.usage);
        context_tokens = completion.usage.context_tokens();
        session.add_usage(&completion.usage);
        if is_root {
            session.set_context_tokens(context_tokens);
        }
        let text = completion.message.text();
        if !text.trim().is_empty() {
            final_text = text;
        }
        let calls: Vec<Call> = completion
            .message
            .tool_calls()
            .map(|call| Call {
                id: call.id.to_string(),
                name: call.name.to_string(),
                input: call.input.clone(),
                invalid_json: call.invalid_json.map(str::to_string),
            })
            .collect();
        history.push(completion.message.clone());
        session.emit(
            &spec.agent_id,
            EventKind::AssistantMessage {
                message: completion.message,
                usage: completion.usage,
                stop_reason: completion.stop_reason.clone(),
            },
        );

        match completion.stop_reason {
            StopReason::Refusal => {
                // A refusal can cut a tool call off mid-input; never run this turn's tools.
                if !calls.is_empty() {
                    history.push(reject_all(session, spec, &calls, "Not executed: the response was declined by the provider."));
                }
                save(history);
                return finish(TurnOutcome::Refusal, usage, final_text, None);
            }
            StopReason::MaxTokens => {
                continuations += 1;
                if continuations > MAX_CONTINUATIONS {
                    let message = "The model kept hitting its output limit; stopping.".to_string();
                    session.emit(&spec.agent_id, EventKind::Error { message: message.clone() });
                    if !calls.is_empty() {
                        history.push(reject_all(session, spec, &calls, "Not executed: the response was cut off."));
                    }
                    save(history);
                    return finish(TurnOutcome::Error, usage, final_text, Some(message));
                }
                if calls.is_empty() {
                    history.push(Message::user_text(
                        "Your response was cut off by the output token limit. Continue exactly where you \
                         stopped; do not repeat what you already wrote.",
                    ));
                } else {
                    // The arguments may be truncated even when they parse.
                    history.push(reject_all(
                        session,
                        spec,
                        &calls,
                        "Not executed: your response hit the output token limit while tool calls were being \
                         written, so their arguments may be incomplete. Issue the calls again; write large \
                         files in several smaller steps.",
                    ));
                }
                save(history);
                continue;
            }
            _ => {}
        }
        continuations = 0;

        if calls.is_empty() {
            save(history);
            if is_root && session.has_steering() {
                continue;
            }
            return finish(TurnOutcome::Completed, usage, final_text, None);
        }

        let results = execute_tools(env, spec, &calls).await;
        history.push(Message { role: Role::User, content: results });
        save(history);
    }
}

/// Fold user messages that arrived mid-turn into the conversation.
fn inject_steering(session: &Session, history: &mut Vec<Message>) {
    let pending = session.take_steering();
    if pending.is_empty() {
        return;
    }
    let mut blocks = Vec::new();
    for input in pending {
        session.emit(ROOT_AGENT, EventKind::UserMessage { text: input.text.clone(), images: input.images.clone() });
        blocks.extend(input.into_blocks());
    }
    // The newest user message has not been sent yet, so extending it keeps
    // the roles alternating without rewriting anything the model has seen.
    match history.last_mut() {
        Some(last) if last.role == Role::User => last.content.extend(blocks),
        _ => history.push(Message { role: Role::User, content: blocks }),
    }
}

/// Answer every call of a response with the same error, without running anything.
fn reject_all(session: &Session, spec: &AgentSpec, calls: &[Call], reason: &str) -> Message {
    let content = calls
        .iter()
        .map(|call| {
            report_rejection(session, spec, call, reason);
            ContentBlock::tool_result(&call.id, reason, true)
        })
        .collect();
    Message { role: Role::User, content }
}

fn report_rejection(session: &Session, spec: &AgentSpec, call: &Call, reason: &str) {
    let (kind, summary) = match spec.tools.get(&call.name) {
        Some(tool) => (tool.kind(), tool.summarize(&call.input)),
        None => (swe_protocol::ToolKind::Meta, String::new()),
    };
    session.emit(
        &spec.agent_id,
        EventKind::ToolStarted {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            kind,
            input: call.input.clone(),
            summary,
        },
    );
    session.emit(
        &spec.agent_id,
        EventKind::ToolFinished {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            output: reason.to_string(),
            is_error: true,
            duration_ms: 0,
            display: None,
        },
    );
}

/// One model request, with retries, context recovery and live streaming.
async fn request(
    env: &Env,
    spec: &AgentSpec,
    history: &mut Vec<Message>,
    tools: &[ToolSpec],
    usage: &mut Usage,
) -> Result<Completion, RequestError> {
    let session = &env.session;
    let mut attempt = 0u32;
    let mut compacted = false;
    let mut stripped = false;
    loop {
        let chat = ChatRequest {
            model: spec.model.clone(),
            system: spec.system.clone(),
            messages: history.clone(),
            tools: tools.to_vec(),
            max_output_tokens: None,
            effort: spec.effort,
            cache_key: Some(format!("{}-{}", session.id, spec.agent_id)),
        };
        let error = match stream_once(env, spec, chat).await {
            Ok(completion) => return Ok(completion),
            Err(StreamFailure::Cancelled(partial)) => return Err(RequestError::Cancelled { partial }),
            Err(StreamFailure::Llm(error)) => error,
        };
        match &error {
            // The provider's limit is lower than we assumed: summarise and go on.
            LlmError::ContextOverflow(_) if !compacted => {
                compacted = true;
                match compaction::compact(env, spec, history, 0).await {
                    Ok(spent) => {
                        usage.add(&spent);
                        session.add_usage(&spent);
                    }
                    Err(compaction::CompactError::Cancelled) => {
                        return Err(RequestError::Cancelled { partial: String::new() });
                    }
                    Err(compaction::CompactError::Failed(message)) => {
                        return Err(RequestError::Failed(format!("{error} (and summarising failed: {message})")));
                    }
                }
            }
            // Documented recovery when replayed reasoning no longer validates:
            // send the history once without any reasoning blocks.
            LlmError::ReasoningRejected(_) if !stripped => {
                stripped = true;
                for message in history.iter_mut() {
                    message.content.retain(|block| !matches!(block, ContentBlock::Reasoning { .. }));
                }
                history.retain(|message| !message.content.is_empty());
                session.emit(
                    &spec.agent_id,
                    EventKind::Notice {
                        text: "The provider rejected earlier reasoning blocks; continuing without them.".into(),
                    },
                );
            }
            error if error.is_retryable() && attempt < MAX_RETRIES => {
                attempt += 1;
                let delay = match error.retry_after() {
                    Some(delay) if delay > MAX_RETRY_DELAY => {
                        return Err(RequestError::Failed(format!(
                            "{error} (the provider asks to wait {}s)",
                            delay.as_secs()
                        )));
                    }
                    Some(delay) => delay,
                    None => backoff(attempt),
                };
                session.emit(
                    &spec.agent_id,
                    EventKind::Retrying { attempt, delay_ms: delay.as_millis() as u64, message: error.to_string() },
                );
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = env.cancel.cancelled() => return Err(RequestError::Cancelled { partial: String::new() }),
                }
            }
            error => return Err(RequestError::Failed(error.to_string())),
        }
    }
}

/// Exponential backoff with jitter: 2s, 4s, 8s ... capped at 60s.
fn backoff(attempt: u32) -> Duration {
    let base = 2_000u64.saturating_mul(1 << attempt.saturating_sub(1).min(5)).min(60_000);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // +/- 20%
    let jitter = base / 5;
    Duration::from_millis(base - jitter + nanos % (2 * jitter + 1))
}

enum StreamFailure {
    Cancelled(String),
    Llm(LlmError),
}

async fn stream_once(env: &Env, spec: &AgentSpec, chat: ChatRequest) -> Result<Completion, StreamFailure> {
    let session = &env.session;
    let mut stream = tokio::select! {
        stream = spec.provider.stream(chat) => stream.map_err(StreamFailure::Llm)?,
        _ = env.cancel.cancelled() => return Err(StreamFailure::Cancelled(String::new())),
    };
    let mut text = String::new();
    // Tool calls being generated: index -> (id, name, characters so far, last reported).
    let mut calls: HashMap<usize, (String, String, u64, u64)> = HashMap::new();
    loop {
        let item = tokio::select! {
            item = stream.next() => item,
            _ = env.cancel.cancelled() => return Err(StreamFailure::Cancelled(text)),
        };
        match item {
            Some(Ok(StreamEvent::TextDelta(delta))) => {
                text.push_str(&delta);
                session.emit(&spec.agent_id, EventKind::AssistantDelta { text: delta });
            }
            Some(Ok(StreamEvent::ReasoningDelta(delta))) => {
                session.emit(&spec.agent_id, EventKind::ReasoningDelta { text: delta });
            }
            Some(Ok(StreamEvent::ToolCallStart { index, id, name })) => {
                session.emit(
                    &spec.agent_id,
                    EventKind::ToolCallStreaming { tool_call_id: id.clone(), name: name.clone(), input_chars: 0 },
                );
                calls.insert(index, (id, name, 0, 0));
            }
            Some(Ok(StreamEvent::ToolCallDelta { index, fragment })) => {
                if let Some((id, name, chars, reported)) = calls.get_mut(&index) {
                    *chars += fragment.len() as u64;
                    // Progress for large inputs (file contents) without an event per token.
                    if *chars - *reported >= 1024 {
                        *reported = *chars;
                        session.emit(
                            &spec.agent_id,
                            EventKind::ToolCallStreaming {
                                tool_call_id: id.clone(),
                                name: name.clone(),
                                input_chars: *chars,
                            },
                        );
                    }
                }
            }
            Some(Ok(StreamEvent::Completed(completion))) => return Ok(completion),
            Some(Err(error)) => return Err(StreamFailure::Llm(error)),
            None => {
                return Err(StreamFailure::Llm(LlmError::Stream("the response ended without completing".into())));
            }
        }
    }
}

enum Planned {
    Run(Arc<dyn Tool>),
    Reject(String),
}

const STOPPED: &str = "[stopped by the user]";

/// Decide about and run the tool calls of one assistant message. Results come
/// back in call order, one per call, whatever happened to each.
async fn execute_tools(env: &Env, spec: &AgentSpec, calls: &[Call]) -> Vec<ContentBlock> {
    let session = &env.session;

    // Phase 1: permissions, in order, so approvals appear one at a time and an
    // "always allow" given for one call already covers the next.
    let mut plans = Vec::with_capacity(calls.len());
    for call in calls {
        plans.push(plan_call(env, spec, call).await);
    }

    // Phase 2: run. Neighbouring parallel-safe calls run together; anything
    // with side effects runs alone, in order.
    let mut outputs: Vec<Option<ToolOutput>> = calls.iter().map(|_| None).collect();
    let mut index = 0;
    while index < calls.len() {
        match &plans[index] {
            Planned::Reject(reason) => {
                report_rejection(session, spec, &calls[index], reason);
                outputs[index] = Some(ToolOutput::error(reason.clone()));
                index += 1;
            }
            Planned::Run(tool) if tool.parallel_safe() => {
                let mut end = index;
                while end < calls.len() && matches!(&plans[end], Planned::Run(tool) if tool.parallel_safe()) {
                    end += 1;
                }
                let batch = (index..end).map(|i| {
                    let Planned::Run(tool) = &plans[i] else { unreachable!("range holds runnable calls") };
                    run_call(env, spec, &calls[i], tool.clone())
                });
                for (offset, output) in join_all(batch).await.into_iter().enumerate() {
                    outputs[index + offset] = Some(output);
                }
                index = end;
            }
            Planned::Run(tool) => {
                outputs[index] = Some(run_call(env, spec, &calls[index], tool.clone()).await);
                index += 1;
            }
        }
    }

    calls
        .iter()
        .zip(outputs)
        .map(|(call, output)| {
            let output = output.expect("every call produced an output");
            ContentBlock::tool_result(&call.id, output.content, output.is_error)
        })
        .collect()
}

async fn plan_call(env: &Env, spec: &AgentSpec, call: &Call) -> Planned {
    let session = &env.session;
    if env.cancel.is_cancelled() {
        return Planned::Reject(STOPPED.to_string());
    }
    if let Some(raw) = &call.invalid_json {
        // Hand the unparseable text back so the model can see what it wrote.
        return Planned::Reject(
            json!({
                "error": "The tool arguments were not valid JSON, so the tool was not run. Issue the call again with well-formed arguments.",
                "INVALID_JSON": raw,
            })
            .to_string(),
        );
    }
    let Some(tool) = spec.tools.get(&call.name) else {
        return Planned::Reject(format!(
            "Unknown tool `{}`. Available tools: {}.",
            call.name,
            spec.tools.names().join(", ")
        ));
    };
    let verdict = session.policy().check(tool.as_ref(), &call.input, &spec.cwd);
    match verdict {
        Verdict::Allow => Planned::Run(tool),
        Verdict::Deny { reason } => Planned::Reject(reason),
        Verdict::Ask { reason, always } => {
            let outcome = session
                .request_approval(
                    &env.cancel,
                    &spec.agent_id,
                    &call.id,
                    tool.as_ref(),
                    &call.input,
                    reason,
                    always.clone(),
                )
                .await;
            match outcome {
                ApprovalOutcome::Decided { decision: ApprovalDecision::Allow, .. } => Planned::Run(tool),
                ApprovalOutcome::Decided { decision: ApprovalDecision::AllowAlways, .. } => {
                    if let Some(rule) = always {
                        session.add_rule(rule);
                    }
                    Planned::Run(tool)
                }
                ApprovalOutcome::Decided { decision: ApprovalDecision::Deny, feedback } => {
                    let mut message = "The user declined this action. Do not try to achieve it another way \
                                       without asking."
                        .to_string();
                    if let Some(feedback) = feedback.filter(|text| !text.trim().is_empty()) {
                        message.push_str(&format!(" The user said: {}", feedback.trim()));
                    }
                    Planned::Reject(message)
                }
                ApprovalOutcome::Cancelled => Planned::Reject(STOPPED.to_string()),
            }
        }
    }
}

async fn run_call(env: &Env, spec: &AgentSpec, call: &Call, tool: Arc<dyn Tool>) -> ToolOutput {
    let session = &env.session;
    session.emit(
        &spec.agent_id,
        EventKind::ToolStarted {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            kind: tool.kind(),
            input: call.input.clone(),
            summary: tool.summarize(&call.input),
        },
    );
    let started = Instant::now();
    let output = if env.cancel.is_cancelled() {
        ToolOutput::error(STOPPED)
    } else {
        let sink_session = session.clone();
        let sink_agent = spec.agent_id.clone();
        let sink_call = call.id.clone();
        let ctx = ToolContext {
            cwd: spec.cwd.clone(),
            cancel: env.cancel.clone(),
            output: OutputSink::new(move |chunk| {
                sink_session.emit(
                    &sink_agent,
                    EventKind::ToolOutputDelta { tool_call_id: sink_call.clone(), chunk },
                );
            }),
            shell: session.shell(),
            call_id: call.id.clone(),
        };
        let mut output = tool.run(call.input.clone(), &ctx).await;
        output.content = truncate_middle(&output.content, MAX_TOOL_RESULT_CHARS);
        output
    };
    session.emit(
        &spec.agent_id,
        EventKind::ToolFinished {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            output: output.content.clone(),
            is_error: output.is_error,
            duration_ms: started.elapsed().as_millis() as u64,
            display: output.display.clone(),
        },
    );
    output
}

impl UserInput {
    pub(crate) fn into_blocks(self) -> Vec<ContentBlock> {
        let mut blocks = Vec::new();
        if !self.text.is_empty() {
            blocks.push(ContentBlock::text(self.text));
        }
        for image in self.images {
            blocks.push(ContentBlock::Image { media_type: image.media_type, data: image.data });
        }
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_stays_bounded() {
        for attempt in 1..=12 {
            let delay = backoff(attempt).as_millis() as u64;
            let base = (2_000u64 << (attempt - 1).min(5)).min(60_000);
            assert!(delay >= base - base / 5 && delay <= base + base / 5, "attempt {attempt}: {delay}ms");
        }
        assert!(backoff(12) <= Duration::from_secs(72));
    }
}
