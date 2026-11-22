//! Session-level types: metadata, the event stream the UI renders, approvals.
//!
//! Events are the single source of truth for what happened in a session. The
//! durable ones are appended to the session's `events.jsonl`; transient ones
//! (token deltas, live command output) are only broadcast to connected clients.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{Effort, Message, StopReason, Usage};

/// Agent id of the top-level agent in every session.
pub const ROOT_AGENT: &str = "root";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Waiting for the user.
    Idle,
    /// A turn is in progress.
    Running,
    /// A turn is blocked on a tool approval.
    WaitingApproval,
    /// The last turn ended with an error.
    Error,
    /// The process stopped while a turn was running; it can be continued.
    Interrupted,
}

/// How much the agent may do without asking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Read-only: explore and plan, no edits, no commands with side effects.
    Plan,
    /// Ask before every edit and command.
    #[default]
    Ask,
    /// Edits inside the workspace are automatic; commands still ask.
    AutoEdit,
    /// Nothing asks. Use in a sandbox or on work you can throw away.
    FullAuto,
}

impl std::str::FromStr for PermissionMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().replace('_', "-").as_str() {
            "plan" | "read-only" => Ok(PermissionMode::Plan),
            "ask" | "supervised" => Ok(PermissionMode::Ask),
            "auto-edit" | "autoedit" => Ok(PermissionMode::AutoEdit),
            "full-auto" | "fullauto" | "yolo" => Ok(PermissionMode::FullAuto),
            other => Err(format!(
                "unknown permission mode `{other}` (expected plan|ask|auto-edit|full-auto)"
            )),
        }
    }
}

/// What executes the agent loop for a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    /// SWE Agent's own loop, talking to a provider API with the user's key.
    #[default]
    Native,
    /// The user's own installed `claude` command line tool, run as a subprocess.
    ClaudeCli,
    /// The user's own installed `codex` command line tool, run as a subprocess.
    CodexCli,
}

impl Engine {
    pub fn as_str(&self) -> &'static str {
        match self {
            Engine::Native => "native",
            Engine::ClaudeCli => "claude-cli",
            Engine::CodexCli => "codex-cli",
        }
    }
}

impl std::str::FromStr for Engine {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "native" => Ok(Engine::Native),
            "claude-cli" | "claude" => Ok(Engine::ClaudeCli),
            "codex-cli" | "codex" => Ok(Engine::CodexCli),
            other => Err(format!(
                "unknown engine `{other}` (expected native|claude-cli|codex-cli)"
            )),
        }
    }
}

/// Coarse classification of a tool, used by the permission policy and the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads files or state; never asks.
    Read,
    /// Creates or modifies files.
    Edit,
    /// Runs commands.
    Execute,
    /// Talks to the network.
    Network,
    /// Spawns or manages other agents.
    Agent,
    /// Bookkeeping with no side effects outside the session (todo list, ...).
    Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    /// Absolute path of the directory the agent works in.
    pub workspace: String,
    #[serde(default)]
    pub engine: Engine,
    /// Provider id (`anthropic`, `openai`, `google`, `openrouter`, or a custom one).
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub mode: PermissionMode,
    #[serde(default)]
    pub effort: Effort,
    /// Agent profile of the root agent (`coder`, `orchestrator`, ...).
    pub agent: String,
    pub status: SessionStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Cumulative usage over the whole session, sub-agents included.
    #[serde(default)]
    pub usage: Usage,
    /// Context in use by the root agent after its most recent request.
    #[serde(default)]
    pub context_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Sequence number of the newest durable event.
    #[serde(default)]
    pub last_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Conversation id of the vendor CLI, for `Engine` other than native.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    /// Allow, and stop asking for calls matching the offered rule this session.
    AllowAlways,
    Deny,
}

/// Structured rendering hints attached to a finished tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDisplay {
    Diff {
        path: String,
        /// Unified diff of the change.
        diff: String,
        added: u32,
        removed: u32,
        created: bool,
    },
    Command {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// Set when the command was left running in the background.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background_id: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Error,
    /// The provider declined to answer.
    Refusal,
    /// The configured per-turn step budget ran out.
    StepLimit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic per session. Transient events carry the seq of the last
    /// durable event so clients can order them but must not resume from them.
    pub seq: u64,
    pub ts: DateTime<Utc>,
    /// `root` or a sub-agent id.
    pub agent_id: String,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A message from the user was accepted into the conversation.
    UserMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
    TurnStarted {
        provider: String,
        model: String,
    },
    /// Streaming assistant text. Transient.
    AssistantDelta {
        text: String,
    },
    /// Streaming reasoning text. Transient.
    ReasoningDelta {
        text: String,
    },
    /// A tool call is being generated; `input_chars` grows as arguments stream. Transient.
    ToolCallStreaming {
        tool_call_id: String,
        name: String,
        input_chars: u64,
    },
    /// A complete assistant message (text, reasoning and tool calls).
    AssistantMessage {
        message: Message,
        usage: Usage,
        stop_reason: StopReason,
    },
    ToolStarted {
        tool_call_id: String,
        name: String,
        kind: ToolKind,
        input: Value,
        /// One-line human summary, e.g. `cargo test -p swe-llm`.
        summary: String,
    },
    /// Live output of a running tool. Transient.
    ToolOutputDelta {
        tool_call_id: String,
        chunk: String,
    },
    ToolFinished {
        tool_call_id: String,
        name: String,
        output: String,
        is_error: bool,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ToolDisplay>,
    },
    ApprovalRequested {
        request_id: String,
        tool_call_id: String,
        name: String,
        kind: ToolKind,
        input: Value,
        summary: String,
        /// Why approval is needed (mode, path outside workspace, ...).
        reason: String,
        /// Label of the rule `allow_always` would add, if one can be offered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        always_label: Option<String>,
    },
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
    },
    TodosUpdated {
        todos: Vec<Todo>,
    },
    /// A sub-agent was started by the tool call `tool_call_id` of `parent_agent_id`.
    AgentSpawned {
        parent_agent_id: String,
        tool_call_id: String,
        description: String,
        agent_type: String,
        provider: String,
        model: String,
        /// Path of the git worktree the agent works in, when isolated.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
    },
    AgentFinished {
        outcome: TurnOutcome,
        usage: Usage,
    },
    /// The conversation was summarised to free context.
    Compacted {
        summary: String,
        tokens_before: u64,
    },
    /// A request failed and is being retried. Transient.
    Retrying {
        attempt: u32,
        delay_ms: u64,
        message: String,
    },
    Error {
        message: String,
    },
    /// Informational line for the timeline (mode switches, fallbacks, ...).
    Notice {
        text: String,
    },
    TurnFinished {
        outcome: TurnOutcome,
        /// Usage of this turn, sub-agents included.
        usage: Usage,
    },
}

impl EventKind {
    /// Transient events are broadcast but never persisted or replayed.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            EventKind::AssistantDelta { .. }
                | EventKind::ReasoningDelta { .. }
                | EventKind::ToolCallStreaming { .. }
                | EventKind::ToolOutputDelta { .. }
                | EventKind::Retrying { .. }
        )
    }
}

/// An image attached to a user message, stored inline as base64.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageRef {
    pub media_type: String,
    pub data: String,
}

/// Text generated so far for messages that are still streaming, so a client
/// that connects mid-turn can catch up without the transient deltas it missed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamingSnapshot {
    pub agent_id: String,
    pub text: String,
    pub reasoning: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_flattens_kind_next_to_envelope() {
        let event = Event {
            seq: 7,
            ts: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            agent_id: ROOT_AGENT.into(),
            kind: EventKind::ToolFinished {
                tool_call_id: "c1".into(),
                name: "bash".into(),
                output: "ok".into(),
                is_error: false,
                duration_ms: 12,
                display: Some(ToolDisplay::Command {
                    command: "ls".into(),
                    exit_code: Some(0),
                    background_id: None,
                }),
            },
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["seq"], 7);
        assert_eq!(value["type"], "tool_finished");
        assert_eq!(value["agent_id"], "root");
        assert_eq!(
            value["display"],
            json!({"kind": "command", "command": "ls", "exit_code": 0})
        );
        let back: Event = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn only_streaming_noise_is_transient() {
        assert!(EventKind::AssistantDelta { text: "x".into() }.is_transient());
        assert!(!EventKind::Notice { text: "x".into() }.is_transient());
    }

    #[test]
    fn modes_and_engines_parse_from_user_input() {
        assert_eq!("full_auto".parse::<PermissionMode>().unwrap(), PermissionMode::FullAuto);
        assert_eq!("auto-edit".parse::<PermissionMode>().unwrap(), PermissionMode::AutoEdit);
        assert!("nope".parse::<PermissionMode>().is_err());
        assert_eq!(serde_json::to_value(Engine::ClaudeCli).unwrap(), json!("claude-cli"));
        assert_eq!("codex".parse::<Engine>().unwrap(), Engine::CodexCli);
    }
}
