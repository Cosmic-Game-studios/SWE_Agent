//! Vendor CLI engines: run a session on the user's own installed `claude` or
//! `codex` command line tool instead of SWE Agent's own loop.
//!
//! This exists for people who want to use the login their vendor CLI already
//! has (for example a subscription) rather than an API key. The boundary is
//! deliberate and strict:
//!
//! * SWE Agent only starts the unmodified binary found on `PATH`, in the
//!   session's workspace, and reads its machine-readable output.
//! * It never reads, stores or forwards that tool's credentials, and it has
//!   no login flow of its own for these vendors.
//!
//! Whether a given plan covers use through another program is defined by the
//! vendor's terms, not by this code; the UI says so where the engine is
//! chosen. Google's terms rule this out for consumer plans, which is why
//! there is no such engine for Gemini.
//!
//! In this mode the vendor tool runs its own agent loop and its own tools.
//! Its headless modes cannot ask for approval mid-run, so the session's
//! permission mode is mapped to the tool's own policy flags up front.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};
use swe_protocol::{
    ContentBlock, Engine, EventKind, Message, PermissionMode, ROOT_AGENT, Role, SessionMeta, StopReason, Todo,
    TodoStatus, ToolDisplay, ToolKind, TurnOutcome, Usage,
};
use swe_tools::shell::{kill_tree, own_process_group};
use swe_tools::text::{truncate_middle, unified_diff};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::agent::Env;
use crate::session::UserInput;

const MAX_OUTPUT_CHARS: usize = 100_000;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EngineInfo {
    pub id: Engine,
    pub name: String,
    pub description: String,
    /// Command looked up on `PATH`; empty for the native engine.
    pub binary: String,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

fn binary_for(engine: Engine) -> String {
    let (variable, default) = match engine {
        Engine::ClaudeCli => ("SWE_AGENT_CLAUDE_BIN", "claude"),
        Engine::CodexCli => ("SWE_AGENT_CODEX_BIN", "codex"),
        Engine::Native => return String::new(),
    };
    std::env::var(variable).ok().filter(|value| !value.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(binary);
    if candidate.components().count() > 1 {
        return candidate.is_file().then_some(candidate);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let extensions: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
        extensions.iter().map(|ext| dir.join(format!("{binary}{ext}"))).find(|file| file.is_file())
    })
}

/// The engines a session can run on, and whether their tools are installed.
pub fn engines() -> Vec<EngineInfo> {
    let mut engines = vec![EngineInfo {
        id: Engine::Native,
        name: "SWE Agent".to_string(),
        description: "The built-in agent loop with your own API key. Works with every provider and model, \
                      with approvals, sub-agents and orchestration."
            .to_string(),
        binary: String::new(),
        installed: true,
        path: None,
    }];
    let external = [
        (
            Engine::ClaudeCli,
            "claude (your install)",
            "Runs the Claude Code command line tool installed on this machine and uses whatever login it \
             has. SWE Agent never sees its credentials. No mid-run approvals.",
        ),
        (
            Engine::CodexCli,
            "codex (your install)",
            "Runs the Codex command line tool installed on this machine and uses whatever login it has. \
             SWE Agent never sees its credentials. No mid-run approvals, no streaming text.",
        ),
    ];
    for (id, name, description) in external {
        let binary = binary_for(id);
        let path = find_on_path(&binary);
        engines.push(EngineInfo {
            id,
            name: name.to_string(),
            description: description.to_string(),
            installed: path.is_some(),
            path: path.map(|p| p.to_string_lossy().into_owned()),
            binary,
        });
    }
    engines
}

/// What a line of CLI output means for the session.
#[derive(Debug, PartialEq)]
enum Out {
    Event { agent_id: String, kind: EventKind },
    /// The CLI's own conversation id.
    SessionId(String),
    /// Usage to add to the session.
    Usage(Usage),
    /// Usage of the whole CLI conversation so far.
    TotalUsage(Usage),
    /// The run is over; `Some` carries the failure.
    Finished(Option<String>),
}

fn event(agent_id: &str, kind: EventKind) -> Out {
    Out::Event { agent_id: agent_id.to_string(), kind }
}

trait LineParser: Send {
    fn feed(&mut self, line: &Value) -> Vec<Out>;
}

struct OpenTool {
    name: String,
    input: Value,
    started: Instant,
}

// ------------------------------------------------------------ claude

fn claude_command(binary: &str, meta: &SessionMeta) -> Command {
    let mut command = Command::new(binary);
    command.args(["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]);
    command.args([
        "--permission-mode",
        match meta.mode {
            PermissionMode::Plan => "plan",
            PermissionMode::Ask => "default",
            PermissionMode::AutoEdit => "acceptEdits",
            PermissionMode::FullAuto => "bypassPermissions",
        },
    ]);
    if !meta.model.trim().is_empty() {
        command.args(["--model", meta.model.trim()]);
    }
    if let Some(session) = &meta.external_session_id {
        command.args(["--resume", session]);
    }
    command
}

fn claude_tool_kind(name: &str) -> ToolKind {
    match name {
        "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" | "BashOutput" | "TaskOutput" => ToolKind::Read,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ToolKind::Edit,
        "Bash" | "KillShell" => ToolKind::Execute,
        "WebFetch" | "WebSearch" => ToolKind::Network,
        "Task" | "Agent" => ToolKind::Agent,
        other if other.starts_with("mcp__") => ToolKind::Execute,
        _ => ToolKind::Meta,
    }
}

/// One line describing a tool call, from whichever argument identifies it.
fn summarize_input(input: &Value) -> String {
    const KEYS: &[&str] = &["command", "file_path", "path", "pattern", "url", "query", "description", "prompt"];
    let text = KEYS.iter().find_map(|key| input.get(*key).and_then(Value::as_str)).unwrap_or_default();
    swe_tools::text::truncate_line(text.lines().next().unwrap_or_default(), 200)
}

fn todos_from(input: &Value) -> Option<Vec<Todo>> {
    let todos = input.get("todos")?.as_array()?;
    Some(
        todos
            .iter()
            .filter_map(|todo| {
                let content = todo["content"].as_str().or_else(|| todo["subject"].as_str())?;
                let status = match todo["status"].as_str().unwrap_or("pending") {
                    "completed" => TodoStatus::Completed,
                    "in_progress" => TodoStatus::InProgress,
                    _ => TodoStatus::Pending,
                };
                Some(Todo { content: content.to_string(), status })
            })
            .collect(),
    )
}

#[derive(Default)]
struct ClaudeParser {
    tools: HashMap<String, OpenTool>,
    /// Tool call id of a delegation -> agent id used for its events.
    agents: HashMap<String, String>,
    announced_key: bool,
}

impl ClaudeParser {
    fn agent_for(&self, line: &Value) -> String {
        line["parent_tool_use_id"]
            .as_str()
            .and_then(|parent| self.agents.get(parent))
            .cloned()
            .unwrap_or_else(|| ROOT_AGENT.to_string())
    }
}

impl LineParser for ClaudeParser {
    fn feed(&mut self, line: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        let agent = self.agent_for(line);
        match line["type"].as_str().unwrap_or_default() {
            "system" => match line["subtype"].as_str().unwrap_or_default() {
                "init" => {
                    if let Some(id) = line["session_id"].as_str() {
                        out.push(Out::SessionId(id.to_string()));
                    }
                    // An exported API key silently takes precedence over a
                    // subscription login; the user should know which is billed.
                    let source = line["apiKeySource"].as_str().unwrap_or("none");
                    if source != "none" && !self.announced_key {
                        self.announced_key = true;
                        out.push(event(
                            ROOT_AGENT,
                            EventKind::Notice {
                                text: format!("claude is authenticating with an API key ({source}), not with an account login."),
                            },
                        ));
                    }
                }
                "api_retry" => out.push(event(
                    &agent,
                    EventKind::Retrying {
                        attempt: line["attempt"].as_u64().unwrap_or(1) as u32,
                        delay_ms: line["retry_delay_ms"].as_u64().unwrap_or(0),
                        message: line["error"].as_str().unwrap_or("request failed").to_string(),
                    },
                )),
                _ => {}
            },
            "stream_event" => {
                let delta = &line["event"]["delta"];
                if line["event"]["type"] == "content_block_delta" {
                    if let Some(text) = delta["text"].as_str().filter(|_| delta["type"] == "text_delta") {
                        out.push(event(&agent, EventKind::AssistantDelta { text: text.to_string() }));
                    } else if let Some(text) = delta["thinking"].as_str() {
                        out.push(event(&agent, EventKind::ReasoningDelta { text: text.to_string() }));
                    }
                }
            }
            "assistant" => {
                let mut content = Vec::new();
                let mut calls = Vec::new();
                for block in line["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str().unwrap_or_default() {
                        "text" => {
                            let text = block["text"].as_str().unwrap_or_default();
                            if !text.is_empty() {
                                content.push(ContentBlock::text(text));
                            }
                        }
                        "thinking" => {
                            let text = block["thinking"].as_str().unwrap_or_default();
                            if !text.is_empty() {
                                content.push(ContentBlock::Reasoning { text: text.to_string(), meta: None });
                            }
                        }
                        "tool_use" => {
                            let id = block["id"].as_str().unwrap_or_default().to_string();
                            let name = block["name"].as_str().unwrap_or_default().to_string();
                            let input = block["input"].clone();
                            content.push(ContentBlock::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                                invalid_json: None,
                                meta: None,
                            });
                            calls.push((id, name, input));
                        }
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    out.push(event(
                        &agent,
                        EventKind::AssistantMessage {
                            message: Message { role: Role::Assistant, content },
                            // Per-message usage repeats across split events;
                            // the final `result` carries the real totals.
                            usage: Usage::default(),
                            stop_reason: match line["message"]["stop_reason"].as_str() {
                                Some("tool_use") => StopReason::ToolUse,
                                Some("max_tokens") => StopReason::MaxTokens,
                                Some("refusal") => StopReason::Refusal,
                                _ => StopReason::EndTurn,
                            },
                        },
                    ));
                }
                for (id, name, input) in calls {
                    out.push(event(
                        &agent,
                        EventKind::ToolStarted {
                            tool_call_id: id.clone(),
                            name: name.clone(),
                            kind: claude_tool_kind(&name),
                            input: input.clone(),
                            summary: summarize_input(&input),
                        },
                    ));
                    if matches!(name.as_str(), "Task" | "Agent") {
                        let child = format!("x{}", &id[id.len().saturating_sub(10)..]);
                        self.agents.insert(id.clone(), child.clone());
                        out.push(event(
                            &child,
                            EventKind::AgentSpawned {
                                parent_agent_id: agent.clone(),
                                tool_call_id: id.clone(),
                                description: input["description"].as_str().unwrap_or("Sub-agent").to_string(),
                                agent_type: input["subagent_type"].as_str().unwrap_or("general").to_string(),
                                provider: Engine::ClaudeCli.as_str().to_string(),
                                model: line["message"]["model"].as_str().unwrap_or_default().to_string(),
                                worktree: None,
                            },
                        ));
                    }
                    if let Some(todos) = todos_from(&input).filter(|_| name == "TodoWrite") {
                        out.push(event(ROOT_AGENT, EventKind::TodosUpdated { todos }));
                    }
                    self.tools.insert(id, OpenTool { name, input, started: Instant::now() });
                }
            }
            "user" => {
                for block in line["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] != "tool_result" {
                        continue;
                    }
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    let Some(tool) = self.tools.remove(id) else { continue };
                    let output = match &block["content"] {
                        Value::String(text) => text.clone(),
                        Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    let is_error = block["is_error"] == true;
                    let display = match tool.name.as_str() {
                        "Edit" if !is_error => {
                            let path = tool.input["file_path"].as_str().unwrap_or_default();
                            let diff = unified_diff(
                                path,
                                tool.input["old_string"].as_str().unwrap_or_default(),
                                tool.input["new_string"].as_str().unwrap_or_default(),
                            );
                            Some(ToolDisplay::Diff { path: path.to_string(), diff: diff.unified, added: diff.added, removed: diff.removed, created: false })
                        }
                        "Write" if !is_error => {
                            let path = tool.input["file_path"].as_str().unwrap_or_default();
                            let diff = unified_diff(path, "", tool.input["content"].as_str().unwrap_or_default());
                            Some(ToolDisplay::Diff { path: path.to_string(), diff: diff.unified, added: diff.added, removed: 0, created: false })
                        }
                        "Bash" => Some(ToolDisplay::Command {
                            command: tool.input["command"].as_str().unwrap_or_default().to_string(),
                            exit_code: None,
                            background_id: None,
                        }),
                        _ => None,
                    };
                    out.push(event(
                        &agent,
                        EventKind::ToolFinished {
                            tool_call_id: id.to_string(),
                            name: tool.name,
                            output: truncate_middle(&output, MAX_OUTPUT_CHARS),
                            is_error,
                            duration_ms: tool.started.elapsed().as_millis() as u64,
                            display,
                        },
                    ));
                    if let Some(child) = self.agents.remove(id) {
                        out.push(event(
                            &child,
                            EventKind::AgentFinished {
                                outcome: if is_error { TurnOutcome::Error } else { TurnOutcome::Completed },
                                usage: Usage::default(),
                            },
                        ));
                    }
                }
            }
            "rate_limit_event" => {
                let info = &line["rate_limit_info"];
                match info["status"].as_str().unwrap_or_default() {
                    "allowed_warning" => {
                        let used = info["utilization"].as_f64().map(|u| format!(" ({:.0}% used)", u * 100.0)).unwrap_or_default();
                        out.push(event(ROOT_AGENT, EventKind::Notice { text: format!("Approaching the plan's usage limit{used}.") }));
                    }
                    "rejected" => out.push(event(ROOT_AGENT, EventKind::Notice { text: "The plan's usage limit has been reached.".into() })),
                    _ => {}
                }
            }
            "result" => {
                let mut usage = Usage::default();
                let per_model = line["modelUsage"].as_object().filter(|models| !models.is_empty());
                match per_model {
                    Some(models) => {
                        for model in models.values() {
                            let cached = model["cacheReadInputTokens"].as_u64().unwrap_or(0);
                            let written = model["cacheCreationInputTokens"].as_u64().unwrap_or(0);
                            usage.add(&Usage {
                                input_tokens: model["inputTokens"].as_u64().unwrap_or(0) + cached + written,
                                output_tokens: model["outputTokens"].as_u64().unwrap_or(0),
                                cache_read_tokens: cached,
                                cache_write_tokens: written,
                                reasoning_tokens: 0,
                            });
                        }
                    }
                    None => {
                        let raw = &line["usage"];
                        let cached = raw["cache_read_input_tokens"].as_u64().unwrap_or(0);
                        let written = raw["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                        usage = Usage {
                            input_tokens: raw["input_tokens"].as_u64().unwrap_or(0) + cached + written,
                            output_tokens: raw["output_tokens"].as_u64().unwrap_or(0),
                            cache_read_tokens: cached,
                            cache_write_tokens: written,
                            reasoning_tokens: 0,
                        };
                    }
                }
                out.push(Out::Usage(usage));
                if let Some(id) = line["session_id"].as_str() {
                    out.push(Out::SessionId(id.to_string()));
                }
                let failed = line["is_error"] == true || line["subtype"].as_str().is_some_and(|s| s != "success");
                out.push(Out::Finished(failed.then(|| {
                    let errors: Vec<&str> = line["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                    if !errors.is_empty() {
                        errors.join("; ")
                    } else {
                        line["result"]
                            .as_str()
                            .filter(|text| !text.is_empty())
                            .or_else(|| line["subtype"].as_str())
                            .unwrap_or("the run failed")
                            .to_string()
                    }
                })));
            }
            _ => {}
        }
        out
    }
}

// ------------------------------------------------------------- codex

fn codex_command(binary: &str, meta: &SessionMeta) -> Command {
    let mut command = Command::new(binary);
    command.args(["exec", "--json", "--skip-git-repo-check", "-C", &meta.workspace]);
    command.args([
        "--sandbox",
        match meta.mode {
            PermissionMode::Plan | PermissionMode::Ask => "read-only",
            PermissionMode::AutoEdit => "workspace-write",
            PermissionMode::FullAuto => "danger-full-access",
        },
    ]);
    if !meta.model.trim().is_empty() {
        command.args(["--model", meta.model.trim()]);
    }
    let effort = match meta.effort {
        swe_protocol::Effort::Low => "low",
        swe_protocol::Effort::Medium => "medium",
        swe_protocol::Effort::High => "high",
        swe_protocol::Effort::Xhigh => "xhigh",
    };
    command.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
    if let Some(thread) = &meta.external_session_id {
        command.args(["resume", thread]);
    }
    // The prompt is written to stdin.
    command
}

struct CodexParser {
    /// Item ids restart at `item_0` in every process; make them unique.
    run: String,
    tools: HashMap<String, OpenTool>,
}

impl CodexParser {
    fn new() -> Self {
        CodexParser { run: crate::session::short_id()[..6].to_string(), tools: HashMap::new() }
    }

    fn call_id(&self, item: &Value) -> String {
        format!("{}-{}", self.run, item["id"].as_str().unwrap_or("item"))
    }

    fn start(&mut self, out: &mut Vec<Out>, item: &Value, name: &str, kind: ToolKind, input: Value, summary: String) {
        let id = self.call_id(item);
        if self.tools.contains_key(&id) {
            return;
        }
        out.push(event(
            ROOT_AGENT,
            EventKind::ToolStarted { tool_call_id: id.clone(), name: name.to_string(), kind, input: input.clone(), summary },
        ));
        self.tools.insert(id, OpenTool { name: name.to_string(), input, started: Instant::now() });
    }

    fn finish(&mut self, out: &mut Vec<Out>, item: &Value, output: String, is_error: bool, display: Option<ToolDisplay>) {
        let id = self.call_id(item);
        let Some(tool) = self.tools.remove(&id) else { return };
        out.push(event(
            ROOT_AGENT,
            EventKind::ToolFinished {
                tool_call_id: id,
                name: tool.name,
                output: truncate_middle(&output, MAX_OUTPUT_CHARS),
                is_error,
                duration_ms: tool.started.elapsed().as_millis() as u64,
                display,
            },
        ));
    }
}

impl LineParser for CodexParser {
    fn feed(&mut self, line: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        let kind = line["type"].as_str().unwrap_or_default();
        let item = &line["item"];
        let done = kind == "item.completed";
        match kind {
            "thread.started" => {
                if let Some(id) = line["thread_id"].as_str() {
                    out.push(Out::SessionId(id.to_string()));
                }
            }
            "turn.completed" => {
                let raw = &line["usage"];
                out.push(Out::TotalUsage(Usage {
                    input_tokens: raw["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: raw["output_tokens"].as_u64().unwrap_or(0),
                    cache_read_tokens: raw["cached_input_tokens"].as_u64().unwrap_or(0),
                    cache_write_tokens: raw["cache_write_input_tokens"].as_u64().unwrap_or(0),
                    reasoning_tokens: raw["reasoning_output_tokens"].as_u64().unwrap_or(0),
                }));
                out.push(Out::Finished(None));
            }
            "turn.failed" => {
                out.push(Out::Finished(Some(line["error"]["message"].as_str().unwrap_or("the turn failed").to_string())));
            }
            "error" => {
                out.push(Out::Finished(Some(line["message"].as_str().unwrap_or("codex reported an error").to_string())));
            }
            "item.started" | "item.updated" | "item.completed" => match item["type"].as_str().unwrap_or_default() {
                "agent_message" if done => {
                    let text = item["text"].as_str().unwrap_or_default();
                    if !text.is_empty() {
                        out.push(event(
                            ROOT_AGENT,
                            EventKind::AssistantMessage {
                                message: Message::assistant_text(text),
                                usage: Usage::default(),
                                stop_reason: StopReason::EndTurn,
                            },
                        ));
                    }
                }
                "reasoning" if done => {
                    let text = item["text"].as_str().unwrap_or_default();
                    if !text.is_empty() {
                        out.push(event(
                            ROOT_AGENT,
                            EventKind::AssistantMessage {
                                message: Message {
                                    role: Role::Assistant,
                                    content: vec![ContentBlock::Reasoning { text: text.to_string(), meta: None }],
                                },
                                usage: Usage::default(),
                                stop_reason: StopReason::EndTurn,
                            },
                        ));
                    }
                }
                "command_execution" => {
                    let command = item["command"].as_str().unwrap_or_default().to_string();
                    self.start(&mut out, item, "bash", ToolKind::Execute, json!({"command": command}), command.clone());
                    if done {
                        let exit_code = item["exit_code"].as_i64().map(|code| code as i32);
                        let status = item["status"].as_str().unwrap_or_default();
                        let mut output = item["aggregated_output"].as_str().unwrap_or_default().to_string();
                        if status == "declined" {
                            output = format!("[declined by the sandbox policy]\n{output}").trim_end().to_string();
                        }
                        let failed = exit_code.is_some_and(|code| code != 0) || matches!(status, "failed" | "declined");
                        self.finish(&mut out, item, output, failed, Some(ToolDisplay::Command { command, exit_code, background_id: None }));
                    }
                }
                "file_change" => {
                    let changes: Vec<String> = item["changes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|change| {
                            format!("{} {}", change["kind"].as_str().unwrap_or("update"), change["path"].as_str().unwrap_or_default())
                        })
                        .collect();
                    let summary = changes.join(", ");
                    self.start(&mut out, item, "apply_patch", ToolKind::Edit, json!({"changes": item["changes"]}), summary.clone());
                    if done {
                        let failed = item["status"] == "failed";
                        self.finish(&mut out, item, changes.join("\n"), failed, None);
                    }
                }
                "mcp_tool_call" => {
                    let name = format!("mcp__{}__{}", item["server"].as_str().unwrap_or("server"), item["tool"].as_str().unwrap_or("tool"));
                    let arguments = item["arguments"].clone();
                    self.start(&mut out, item, &name, ToolKind::Execute, arguments.clone(), swe_tools::text::truncate_line(&arguments.to_string(), 160));
                    if done {
                        let error = item["error"]["message"].as_str().or_else(|| item["error"].as_str());
                        let text: Vec<&str> = item["result"]["content"].as_array().into_iter().flatten().filter_map(|part| part["text"].as_str()).collect();
                        let failed = error.is_some() || item["status"] == "failed";
                        self.finish(&mut out, item, error.map(str::to_string).unwrap_or_else(|| text.join("\n")), failed, None);
                    }
                }
                "web_search" => {
                    let query = item["query"].as_str().unwrap_or_default().to_string();
                    self.start(&mut out, item, "web_search", ToolKind::Network, json!({"query": query}), query);
                    if done {
                        self.finish(&mut out, item, String::new(), false, None);
                    }
                }
                "todo_list" => {
                    let todos = item["items"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|entry| {
                            Some(Todo {
                                content: entry["text"].as_str()?.to_string(),
                                status: if entry["completed"] == true { TodoStatus::Completed } else { TodoStatus::Pending },
                            })
                        })
                        .collect();
                    out.push(event(ROOT_AGENT, EventKind::TodosUpdated { todos }));
                }
                "error" if done => {
                    if let Some(message) = item["message"].as_str() {
                        out.push(event(ROOT_AGENT, EventKind::Notice { text: message.to_string() }));
                    }
                }
                _ => {}
            },
            _ => {}
        }
        out
    }
}

// ------------------------------------------------------------ runner

pub(crate) async fn run_turn(env: &Env, meta: &SessionMeta, input: UserInput) -> (TurnOutcome, Option<String>) {
    let session = &env.session;
    let fail = |message: String| {
        session.emit(ROOT_AGENT, EventKind::Error { message: message.clone() });
        (TurnOutcome::Error, Some(message))
    };
    let binary = binary_for(meta.engine);
    let Some(path) = find_on_path(&binary) else {
        return fail(format!(
            "`{binary}` was not found on PATH. Install the vendor's command line tool and log in with it, or \
             switch this session to the built-in engine with an API key."
        ));
    };
    if !input.images.is_empty() {
        session.emit(ROOT_AGENT, EventKind::Notice { text: "Images are not passed on to this engine.".into() });
    }
    if matches!(meta.mode, PermissionMode::Ask) {
        session.emit(
            ROOT_AGENT,
            EventKind::Notice {
                text: format!(
                    "`{binary}` cannot ask for approval while it runs: in ask mode, anything that needs approval \
                     is refused. Switch to auto-edit or full-auto to let it make changes."
                ),
            },
        );
    }

    let binary_path = path.to_string_lossy().into_owned();
    let (mut command, mut parser): (Command, Box<dyn LineParser>) = match meta.engine {
        Engine::ClaudeCli => (claude_command(&binary_path, meta), Box::new(ClaudeParser::default())),
        Engine::CodexCli => (codex_command(&binary_path, meta), Box::new(CodexParser::new())),
        Engine::Native => return fail("the built-in engine does not run through a CLI".to_string()),
    };
    command
        .current_dir(&meta.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    own_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return fail(format!("cannot start `{binary}`: {err}")),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let prompt = input.text.clone();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }
    let stderr_tail = {
        let mut stderr = child.stderr.take().expect("piped");
        tokio::spawn(async move {
            let mut text = String::new();
            let mut buffer = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                text.push_str(&String::from_utf8_lossy(&buffer[..n]));
                if text.len() > 16_000 {
                    let mut cut = text.len() - 8_000;
                    while !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    text.drain(..cut);
                }
            }
            text
        })
    };

    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut finished: Option<Option<String>> = None;
    let mut cancelled = false;
    loop {
        let line = tokio::select! {
            line = lines.next_line() => line,
            _ = env.cancel.cancelled() => {
                cancelled = true;
                break;
            }
        };
        let Ok(Some(line)) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else { continue };
        for out in parser.feed(&value) {
            match out {
                Out::Event { agent_id, kind } => {
                    session.emit(&agent_id, kind);
                }
                Out::SessionId(id) => session.set_external_session_id(id),
                Out::Usage(usage) => session.add_usage(&usage),
                Out::TotalUsage(total) => {
                    // The CLI reports the whole conversation; add what is new.
                    let known = session.meta().usage;
                    session.add_usage(&Usage {
                        input_tokens: total.input_tokens.saturating_sub(known.input_tokens),
                        output_tokens: total.output_tokens.saturating_sub(known.output_tokens),
                        cache_read_tokens: total.cache_read_tokens.saturating_sub(known.cache_read_tokens),
                        cache_write_tokens: total.cache_write_tokens.saturating_sub(known.cache_write_tokens),
                        reasoning_tokens: total.reasoning_tokens.saturating_sub(known.reasoning_tokens),
                    });
                }
                Out::Finished(error) => finished = Some(error),
            }
        }
    }

    if cancelled {
        kill_tree(&mut child);
        let _ = child.wait().await;
        return (TurnOutcome::Cancelled, None);
    }
    let status = match tokio::time::timeout(std::time::Duration::from_secs(15), child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            kill_tree(&mut child);
            None
        }
    };
    let stderr = stderr_tail.await.unwrap_or_default();
    match finished {
        Some(None) => (TurnOutcome::Completed, None),
        Some(Some(error)) => fail(error),
        None => {
            let code = status.and_then(|s| s.code());
            let detail = stderr.trim();
            let mut message = match code {
                Some(0) => format!("`{binary}` exited without reporting a result."),
                Some(code) => format!("`{binary}` exited with code {code}."),
                None => format!("`{binary}` was terminated."),
            };
            if !detail.is_empty() {
                message.push('\n');
                message.push_str(&truncate_middle(detail, 2_000));
            }
            fail(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(outs: &[Out]) -> Vec<String> {
        outs.iter()
            .map(|out| match out {
                Out::Event { kind, agent_id } => {
                    let value = serde_json::to_value(kind).unwrap();
                    format!("{}:{}", agent_id, value["type"].as_str().unwrap())
                }
                Out::SessionId(id) => format!("session:{id}"),
                Out::Usage(_) => "usage".into(),
                Out::TotalUsage(_) => "total_usage".into(),
                Out::Finished(None) => "finished".into(),
                Out::Finished(Some(error)) => format!("failed:{error}"),
            })
            .collect()
    }

    fn feed_all(parser: &mut dyn LineParser, lines: &[Value]) -> Vec<Out> {
        lines.iter().flat_map(|line| parser.feed(line)).collect()
    }

    #[test]
    fn claude_stream_maps_to_session_events() {
        let mut parser = ClaudeParser::default();
        let outs = feed_all(
            &mut parser,
            &[
                json!({"type": "system", "subtype": "init", "session_id": "sess-1", "apiKeySource": "none", "model": "claude-sonnet-5"}),
                json!({"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "I'll "}}}),
                json!({"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hm"}}}),
                json!({"type": "assistant", "parent_tool_use_id": null, "message": {"model": "claude-sonnet-5", "stop_reason": "tool_use", "content": [
                    {"type": "thinking", "thinking": "plan", "signature": "s"},
                    {"type": "text", "text": "I'll run the tests."},
                    {"type": "tool_use", "id": "toolu_01A", "name": "Bash", "input": {"command": "npm test"}},
                    {"type": "tool_use", "id": "toolu_01B", "name": "Edit", "input": {"file_path": "a.rs", "old_string": "one\n", "new_string": "two\n"}}
                ]}}),
                json!({"type": "user", "parent_tool_use_id": null, "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "PASS 12 tests", "is_error": false},
                    {"type": "tool_result", "tool_use_id": "toolu_01B", "content": [{"type": "text", "text": "edited"}]}
                ]}}),
                json!({"type": "system", "subtype": "api_retry", "attempt": 2, "retry_delay_ms": 1000, "error": "overloaded"}),
                json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed_warning", "utilization": 0.91}}),
                json!({"type": "result", "subtype": "success", "is_error": false, "session_id": "sess-1", "result": "All tests pass.",
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                    "modelUsage": {"claude-sonnet-5": {"inputTokens": 30, "outputTokens": 412, "cacheReadInputTokens": 18000, "cacheCreationInputTokens": 2100}}}),
            ],
        );
        assert_eq!(
            kinds(&outs),
            [
                "session:sess-1",
                "root:assistant_delta",
                "root:reasoning_delta",
                "root:assistant_message",
                "root:tool_started",
                "root:tool_started",
                "root:tool_finished",
                "root:tool_finished",
                "root:retrying",
                "root:notice",
                "usage",
                "session:sess-1",
                "finished",
            ]
        );
        let usage = outs.iter().find_map(|out| if let Out::Usage(usage) = out { Some(*usage) } else { None }).unwrap();
        assert_eq!(usage, Usage { input_tokens: 20_130, output_tokens: 412, cache_read_tokens: 18_000, cache_write_tokens: 2_100, reasoning_tokens: 0 });
        let edit = outs.iter().find_map(|out| match out {
            Out::Event { kind: EventKind::ToolFinished { name, display, output, .. }, .. } if name == "Edit" => Some((display.clone(), output.clone())),
            _ => None,
        });
        let (display, output) = edit.unwrap();
        assert_eq!(output, "edited");
        assert!(matches!(display, Some(ToolDisplay::Diff { added: 1, removed: 1, .. })));
    }

    #[test]
    fn claude_sub_agents_failures_and_api_keys_are_reported() {
        let mut parser = ClaudeParser::default();
        let outs = feed_all(
            &mut parser,
            &[
                json!({"type": "system", "subtype": "init", "session_id": "s", "apiKeySource": "ANTHROPIC_API_KEY"}),
                json!({"type": "assistant", "parent_tool_use_id": null, "message": {"content": [
                    {"type": "tool_use", "id": "toolu_task000001", "name": "Task", "input": {"description": "Find usages", "subagent_type": "Explore", "prompt": "..."}},
                    {"type": "tool_use", "id": "toolu_todo", "name": "TodoWrite", "input": {"todos": [{"content": "Fix", "status": "in_progress"}]}}
                ]}}),
                json!({"type": "assistant", "parent_tool_use_id": "toolu_task000001", "message": {"content": [{"type": "text", "text": "Searching."}]}}),
                json!({"type": "user", "parent_tool_use_id": null, "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_task000001", "content": "Found 3."}]}}),
                json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["boom", "bang"], "modelUsage": {}, "usage": {"input_tokens": 5, "output_tokens": 2}}),
            ],
        );
        assert_eq!(
            kinds(&outs),
            [
                "session:s",
                "root:notice",
                "root:assistant_message",
                "root:tool_started",
                "xtask000001:agent_spawned",
                "root:tool_started",
                "root:todos_updated",
                "xtask000001:assistant_message",
                "root:tool_finished",
                "xtask000001:agent_finished",
                "usage",
                "failed:boom; bang",
            ]
        );
    }

    #[test]
    fn codex_stream_maps_to_session_events() {
        let mut parser = CodexParser::new();
        let outs = feed_all(
            &mut parser,
            &[
                json!({"type": "thread.started", "thread_id": "0199a213"}),
                json!({"type": "turn.started"}),
                json!({"type": "item.completed", "item": {"id": "item_0", "type": "reasoning", "text": "**Inspecting failing test**"}}),
                json!({"type": "item.started", "item": {"id": "item_1", "type": "command_execution", "command": "bash -lc 'cargo test'", "aggregated_output": "", "exit_code": null, "status": "in_progress"}}),
                json!({"type": "item.completed", "item": {"id": "item_1", "type": "command_execution", "command": "bash -lc 'cargo test'", "aggregated_output": "test result: FAILED\n", "exit_code": 101, "status": "failed"}}),
                json!({"type": "item.completed", "item": {"id": "item_2", "type": "file_change", "changes": [{"path": "/repo/src/lib.rs", "kind": "update"}], "status": "completed"}}),
                json!({"type": "item.started", "item": {"id": "item_3", "type": "todo_list", "items": [{"text": "Fix test", "completed": false}]}}),
                json!({"type": "item.updated", "item": {"id": "item_3", "type": "todo_list", "items": [{"text": "Fix test", "completed": true}]}}),
                json!({"type": "item.completed", "item": {"id": "item_5", "type": "agent_message", "text": "Done. Tests pass."}}),
                json!({"type": "item.completed", "item": {"id": "item_6", "type": "error", "message": "model rerouted"}}),
                json!({"type": "turn.completed", "usage": {"input_tokens": 24763, "cached_input_tokens": 24448, "output_tokens": 122, "reasoning_output_tokens": 64}}),
            ],
        );
        assert_eq!(
            kinds(&outs),
            [
                "session:0199a213",
                "root:assistant_message",
                "root:tool_started",
                "root:tool_finished",
                "root:tool_started",
                "root:tool_finished",
                "root:todos_updated",
                "root:todos_updated",
                "root:assistant_message",
                "root:notice",
                "total_usage",
                "finished",
            ]
        );
        let command = outs.iter().find_map(|out| match out {
            Out::Event { kind: EventKind::ToolFinished { name, is_error, display, .. }, .. } if name == "bash" => Some((*is_error, display.clone())),
            _ => None,
        });
        assert_eq!(
            command,
            Some((true, Some(ToolDisplay::Command { command: "bash -lc 'cargo test'".into(), exit_code: Some(101), background_id: None })))
        );
        assert_eq!(kinds(&parser.feed(&json!({"type": "turn.failed", "error": {"message": "quota"}}))), ["failed:quota"]);
    }

    #[test]
    fn commands_map_the_permission_mode_to_vendor_flags() {
        let meta = |mode, model: &str, resume: Option<&str>| SessionMeta {
            id: "s".into(),
            title: "t".into(),
            workspace: "/work".into(),
            engine: Engine::ClaudeCli,
            provider: "claude-cli".into(),
            model: model.into(),
            mode,
            effort: swe_protocol::Effort::High,
            agent: "coder".into(),
            status: swe_protocol::SessionStatus::Idle,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            usage: Usage::default(),
            context_tokens: 0,
            context_window: None,
            last_seq: 0,
            error: None,
            external_session_id: resume.map(str::to_string),
        };
        let args = |command: Command| -> Vec<String> {
            command.as_std().get_args().map(|arg| arg.to_string_lossy().into_owned()).collect()
        };
        let claude = args(claude_command("claude", &meta(PermissionMode::AutoEdit, "sonnet", Some("sess-9"))));
        assert_eq!(
            claude,
            ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--permission-mode", "acceptEdits", "--model", "sonnet", "--resume", "sess-9"]
        );
        assert!(!args(claude_command("claude", &meta(PermissionMode::Ask, "", None))).contains(&"--model".to_string()));

        let codex = args(codex_command("codex", &meta(PermissionMode::FullAuto, "gpt-6.2-sol", Some("thread-1"))));
        assert_eq!(
            codex,
            ["exec", "--json", "--skip-git-repo-check", "-C", "/work", "--sandbox", "danger-full-access", "--model", "gpt-6.2-sol", "-c", "model_reasoning_effort=\"high\"", "resume", "thread-1"]
        );
        assert!(args(codex_command("codex", &meta(PermissionMode::Ask, "", None))).contains(&"read-only".to_string()));
    }

    #[test]
    fn engines_list_reports_what_is_installed() {
        let engines = engines();
        assert_eq!(engines[0].id, Engine::Native);
        assert!(engines[0].installed);
        assert_eq!(engines.len(), 3);
        assert!(find_on_path("definitely-not-a-real-binary-xyz").is_none());
        assert!(find_on_path("sh").is_some() || cfg!(windows));
    }
}
