//! Context compaction: when a conversation outgrows the context budget, it
//! is summarised and the summary replaces the history.
//!
//! This is what lets a session run for hours. The shape is deliberately the
//! simplest one: the next request consists of the summary (plus the user's
//! pending message, if any) and nothing else. No earlier turn and no earlier
//! reasoning block is replayed, so nothing carried over is tied to the old
//! transcript, which is the form reasoning-binding providers require.

use swe_llm::{ChatRequest, LlmError, StreamEvent};
use swe_protocol::{ContentBlock, Effort, EventKind, Message, Role, TodoStatus, Usage};

use crate::agent::{AgentSpec, Env};

pub(crate) enum CompactError {
    Cancelled,
    Failed(String),
}

const SUMMARY_SYSTEM: &str = "\
You write hand-over summaries for software engineering sessions. An AI agent has been working with a user and \
its conversation has grown too long to keep; your summary will replace the whole transcript, and the agent \
will continue from it with no other memory of what happened. Anything you leave out is lost, so be complete \
and specific rather than short.";

const SUMMARY_REQUEST: &str = "\
Write the hand-over summary of the session above. Use these sections:

1. **Task** - what the user asked for, including every explicit requirement, constraint and preference. Quote \
the user's own wording where it matters.
2. **State of the work** - what has been done so far and what the current status is.
3. **Files** - every file created or changed (path, what changed and why), and the files that were read and \
turned out to matter, with the key facts learned from them (important signatures, line references, snippets \
that will be needed again).
4. **Commands and results** - how to build, run and test; the outcome of the latest runs; errors seen and how \
they were resolved.
5. **Decisions and findings** - design decisions with their reasons, dead ends already tried, things that \
turned out to be wrong.
6. **Open items** - what remains, in order, and the exact next step that was about to be taken.

Write plain text only; do not call any tools.";

/// Roughly how many characters of transcript to feed the summariser per
/// token of context budget.
const CHARS_PER_TOKEN: u64 = 3;
const MAX_TRANSCRIPT_CHARS: u64 = 1_200_000;
const MAX_RESULT_CHARS: usize = 6_000;
const MAX_INPUT_CHARS: usize = 3_000;

/// Replace `history` with a summary of itself. Returns the usage of the
/// summarisation request.
pub(crate) async fn compact(
    env: &Env,
    spec: &AgentSpec,
    history: &mut Vec<Message>,
    tokens_before: u64,
) -> Result<Usage, CompactError> {
    let session = &env.session;

    // A user message that has not been answered yet is not history: keep it
    // verbatim after the summary instead of paraphrasing the request.
    let pending = match history.last() {
        Some(last)
            if last.role == Role::User
                && !last
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. })) =>
        {
            history.pop()
        }
        _ => None,
    };

    let budget = (spec.context_limit.saturating_mul(CHARS_PER_TOKEN)).clamp(40_000, MAX_TRANSCRIPT_CHARS) as usize;
    let transcript = clip(&render_transcript(history), budget);
    let chat = ChatRequest {
        model: spec.model.clone(),
        system: SUMMARY_SYSTEM.to_string(),
        messages: vec![Message::user_text(format!(
            "<transcript>\n{transcript}\n</transcript>\n\n{SUMMARY_REQUEST}"
        ))],
        tools: Vec::new(),
        max_output_tokens: None,
        effort: Effort::Medium,
        cache_key: None,
    };

    let mut attempt = 0;
    let (summary, usage) = loop {
        match summarise(env, spec, chat.clone()).await {
            Ok(done) => break done,
            Err(None) => {
                if let Some(pending) = pending {
                    history.push(pending);
                }
                return Err(CompactError::Cancelled);
            }
            Err(Some(error)) if error.is_retryable() && attempt < 3 => {
                attempt += 1;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(3 * attempt)) => {}
                    _ = env.cancel.cancelled() => {
                        if let Some(pending) = pending {
                            history.push(pending);
                        }
                        return Err(CompactError::Cancelled);
                    }
                }
            }
            Err(Some(error)) => {
                if let Some(pending) = pending {
                    history.push(pending);
                }
                return Err(CompactError::Failed(error.to_string()));
            }
        }
    };
    if summary.trim().is_empty() {
        if let Some(pending) = pending {
            history.push(pending);
        }
        return Err(CompactError::Failed("the model returned an empty summary".into()));
    }

    let mut text = format!(
        "This session continues from an earlier part of the conversation that no longer fits in the context \
         window. Here is the hand-over summary of everything that happened before:\n\n<summary>\n{}\n</summary>",
        summary.trim()
    );
    let todos = session.todos();
    if !todos.is_empty() {
        text.push_str("\n\nCurrent todo list:\n");
        for todo in &todos {
            let mark = match todo.status {
                TodoStatus::Completed => "[x]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Pending => "[ ]",
            };
            text.push_str(&format!("- {mark} {}\n", todo.content));
        }
    }
    let mut content = vec![ContentBlock::text(text)];
    match pending {
        Some(pending) => {
            content.push(ContentBlock::text("The user's latest message follows."));
            content.extend(pending.content);
        }
        None => content.push(ContentBlock::text(
            "Continue the work from where it left off, without asking the user to repeat anything. If the \
             summary shows the task was already finished, say so briefly instead of redoing it.",
        )),
    }
    history.clear();
    history.push(Message {
        role: Role::User,
        content,
    });

    session.emit(
        &spec.agent_id,
        EventKind::Compacted {
            summary: summary.trim().to_string(),
            tokens_before,
        },
    );
    Ok(usage)
}

/// Run the summarisation request. `Err(None)` means the user cancelled.
async fn summarise(env: &Env, spec: &AgentSpec, chat: ChatRequest) -> Result<(String, Usage), Option<LlmError>> {
    use futures::StreamExt;
    let mut stream = tokio::select! {
        stream = spec.provider.stream(chat) => stream.map_err(Some)?,
        _ = env.cancel.cancelled() => return Err(None),
    };
    loop {
        let item = tokio::select! {
            item = stream.next() => item,
            _ = env.cancel.cancelled() => return Err(None),
        };
        match item {
            Some(Ok(StreamEvent::Completed(done))) => return Ok((done.message.text(), done.usage)),
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(Some(error)),
            None => return Err(Some(LlmError::Stream("the summary ended without completing".into()))),
        }
    }
}

/// Flatten a history into text for the summariser. Provider-specific
/// payloads (reasoning signatures, images) are left out.
fn render_transcript(history: &[Message]) -> String {
    let mut out = String::new();
    for message in history {
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                    let speaker = match message.role {
                        Role::User => "USER",
                        Role::Assistant => "ASSISTANT",
                    };
                    out.push_str(&format!("[{speaker}]\n{}\n\n", text.trim()));
                }
                ContentBlock::ToolCall { name, input, .. } => {
                    out.push_str(&format!(
                        "[TOOL CALL {name}]\n{}\n\n",
                        shorten(&input.to_string(), MAX_INPUT_CHARS)
                    ));
                }
                ContentBlock::ToolResult { content, is_error, .. } => {
                    let label = if *is_error { "TOOL ERROR" } else { "TOOL RESULT" };
                    out.push_str(&format!("[{label}]\n{}\n\n", shorten(content, MAX_RESULT_CHARS)));
                }
                ContentBlock::Image { .. } => out.push_str("[USER attached an image]\n\n"),
                _ => {}
            }
        }
    }
    out
}

fn shorten(text: &str, max_chars: usize) -> String {
    swe_tools::text::truncate_middle(text, max_chars)
}

/// Fit a transcript into `budget` bytes, keeping the beginning (the original
/// task) and, mostly, the end (the current state of the work).
fn clip(transcript: &str, budget: usize) -> String {
    if transcript.len() <= budget {
        return transcript.to_string();
    }
    let head = budget / 6;
    let tail = budget - head;
    let mut head_end = head;
    while !transcript.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = transcript.len() - tail;
    while !transcript.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n\n[... the middle of the session is omitted here because it is too long ...]\n\n{}",
        &transcript[..head_end],
        &transcript[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transcript_keeps_dialogue_and_tools_but_not_reasoning() {
        let history = vec![
            Message::user_text("Fix the bug"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Reasoning {
                        text: "secret thoughts".into(),
                        meta: None,
                    },
                    ContentBlock::text("Looking."),
                    ContentBlock::ToolCall {
                        id: "1".into(),
                        name: "read_file".into(),
                        input: json!({"path": "a.rs"}),
                        invalid_json: None,
                        meta: None,
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("1", "fn main() {}", false)],
            },
        ];
        let transcript = render_transcript(&history);
        assert_eq!(
            transcript,
            "[USER]\nFix the bug\n\n[ASSISTANT]\nLooking.\n\n[TOOL CALL read_file]\n{\"path\":\"a.rs\"}\n\n[TOOL RESULT]\nfn main() {}\n\n"
        );
    }

    #[test]
    fn clipping_keeps_the_start_and_most_of_the_end() {
        let transcript = format!("TASK{}END", "é".repeat(10_000));
        let clipped = clip(&transcript, 1_000);
        assert!(clipped.starts_with("TASK"));
        assert!(clipped.ends_with("END"));
        assert!(clipped.contains("omitted"));
        assert!(clipped.len() < 1_200);
        assert_eq!(clip("short", 1_000), "short");
    }
}
