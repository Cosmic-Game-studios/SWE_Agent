//! End-to-end tests of sessions: the real agent loop, real tools and real
//! persistence, driven by the scripted mock provider.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use swe_core::{Config, CreateSession, Paths, SessionManager, UpdateSession, UserInput};
use swe_llm::mock::{MockProvider, MockReply};
use swe_llm::{ChatRequest, LlmError};
use swe_protocol::{
    ApprovalDecision, ContentBlock, Event, EventKind, PermissionMode, ROOT_AGENT, Role, SessionStatus, StopReason,
    TodoStatus, ToolDisplay, TurnOutcome, Usage,
};
use tokio::sync::broadcast::Receiver;

struct Harness {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    manager: SessionManager,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl Harness {
    fn new(provider: MockProvider) -> Self {
        Self::with_config(provider, Config::default())
    }

    fn with_config(provider: MockProvider, config: Config) -> Self {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        config.save(&Paths::at(home.path())).unwrap();
        let manager = SessionManager::open(Paths::at(home.path())).unwrap();
        let requests = provider.request_log();
        manager.registry().inject("mock", Arc::new(provider));
        Harness {
            home,
            workspace,
            manager,
            requests,
        }
    }

    /// The directive-driven demo model, without artificial delays.
    fn scripted() -> Self {
        Self::new(MockProvider::demo("mock").with_chunk_delay(Duration::ZERO))
    }

    fn create(&self, mode: PermissionMode) -> String {
        self.create_with(mode, "coder")
    }

    fn create_with(&self, mode: PermissionMode, agent: &str) -> String {
        self.manager
            .create(CreateSession {
                workspace: self.workspace.path().to_string_lossy().into_owned(),
                provider: Some("mock".into()),
                model: Some("demo".into()),
                mode: Some(mode),
                agent: Some(agent.into()),
                ..Default::default()
            })
            .unwrap()
            .id
    }

    fn subscribe(&self, id: &str) -> Receiver<Event> {
        let session = self.manager.get(id).unwrap();
        let after = session.meta().last_seq;
        session.subscribe(after).1
    }

    /// Send a message and collect every event up to the end of the turn.
    async fn turn(&self, id: &str, text: &str) -> Vec<Event> {
        let mut rx = self.subscribe(id);
        self.manager.send_message(id, UserInput::text(text)).unwrap();
        collect_turn(&mut rx).await
    }

    fn file(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.workspace.path().join(name)).ok()
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

async fn next_event(rx: &mut Receiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event stream closed")
}

async fn wait_for(rx: &mut Receiver<Event>, seen: &mut Vec<Event>, matches: impl Fn(&EventKind) -> bool) -> Event {
    loop {
        let event = next_event(rx).await;
        seen.push(event.clone());
        if matches(&event.kind) {
            return event;
        }
    }
}

async fn collect_turn(rx: &mut Receiver<Event>) -> Vec<Event> {
    let mut seen = Vec::new();
    wait_for(rx, &mut seen, |kind| matches!(kind, EventKind::TurnFinished { .. })).await;
    seen
}

fn outcome(events: &[Event]) -> TurnOutcome {
    events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::TurnFinished { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .expect("turn finished")
}

/// `(tool name, output, is_error)` of every finished tool call, in order.
fn tool_results(events: &[Event]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolFinished {
                name, output, is_error, ..
            } => Some((name.clone(), output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn assistant_texts(events: &[Event], agent: &str) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.agent_id == agent)
        .filter_map(|event| match &event.kind {
            EventKind::AssistantMessage { message, .. } => Some(message.text()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect()
}

fn user_text(request: &ChatRequest, index_from_end: usize) -> String {
    let users: Vec<_> = request.messages.iter().filter(|m| m.role == Role::User).collect();
    users[users.len() - 1 - index_from_end].text()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

#[tokio::test]
async fn a_turn_runs_tools_until_the_model_is_done() {
    let h = Harness::scripted();
    let id = h.create(PermissionMode::FullAuto);
    let events = h
        .turn(
            &id,
            "Create the greeting\n!think Plan it.\n!say Writing the file.\n!tool write_file {\"path\": \"hello.txt\", \"content\": \"hi\\n\"}\n!then\n!tool bash {\"command\": \"cat hello.txt\"}\n!then\n!say All done.",
        )
        .await;

    assert_eq!(outcome(&events), TurnOutcome::Completed);
    assert_eq!(h.file("hello.txt").as_deref(), Some("hi\n"));
    let results = tool_results(&events);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "write_file");
    assert!(results[0].1.contains("Created hello.txt"));
    assert_eq!(results[1], ("bash".into(), "hi".into(), false));
    assert_eq!(assistant_texts(&events, ROOT_AGENT), ["Writing the file.", "All done."]);

    // Streaming deltas arrive before the complete message, and are transient.
    let kinds: Vec<&'static str> = events
        .iter()
        .map(|event| match &event.kind {
            EventKind::UserMessage { .. } => "user",
            EventKind::TurnStarted { .. } => "turn_started",
            EventKind::ReasoningDelta { .. } => "reasoning_delta",
            EventKind::AssistantDelta { .. } => "delta",
            EventKind::AssistantMessage { .. } => "assistant",
            EventKind::ToolStarted { .. } => "tool_started",
            EventKind::ToolFinished { .. } => "tool_finished",
            EventKind::TurnFinished { .. } => "turn_finished",
            _ => "other",
        })
        .collect();
    assert_eq!(&kinds[..2], ["user", "turn_started"]);
    let first_assistant = kinds.iter().position(|k| *k == "assistant").unwrap();
    assert!(kinds[..first_assistant].contains(&"reasoning_delta"));
    assert!(kinds[..first_assistant].contains(&"delta"));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolFinished {
            display: Some(ToolDisplay::Diff { created: true, .. }),
            ..
        }
    )));

    let session = h.manager.get(&id).unwrap();
    let snapshot = session.snapshot();
    assert!(snapshot.events.iter().all(|event| !event.kind.is_transient()));
    assert!(
        snapshot.events.windows(2).all(|pair| pair[1].seq == pair[0].seq + 1),
        "durable events are numbered densely"
    );
    assert!(snapshot.streaming.is_empty());
    let meta = session.meta();
    assert_eq!(meta.status, SessionStatus::Idle);
    assert_eq!(meta.title, "Create the greeting");
    assert!(meta.usage.output_tokens > 0);

    // What the model saw: system prompt, tools, mode notice, and tool results fed back.
    let requests = h.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].system.contains("SWE Agent"));
    assert!(
        requests[0].system.contains(
            &h.workspace
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        )
    );
    let tool_names: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(tool_names.contains(&"task") && tool_names.contains(&"todo_write") && tool_names.contains(&"bash"));
    assert!(user_text(&requests[0], 0).contains("Permission mode: full-auto"));
    assert_eq!(
        requests[2].system, requests[0].system,
        "the system prompt never changes within a session"
    );
    assert!(
        matches!(&requests[2].messages.last().unwrap().content[0], ContentBlock::ToolResult { content, .. } if content == "hi")
    );
}

#[tokio::test]
async fn edits_wait_for_approval_and_respect_the_answer() {
    let h = Harness::scripted();
    let id = h.create(PermissionMode::Ask);
    let session = h.manager.get(&id).unwrap();
    let mut rx = h.subscribe(&id);
    let mut seen = Vec::new();
    h.manager
        .send_message(
            &id,
            UserInput::text("!tool write_file {\"path\": \"a.txt\", \"content\": \"A\"}\n!then\n!tool write_file {\"path\": \"b.txt\", \"content\": \"B\"}\n!then\n!say done"),
        )
        .unwrap();

    // First edit: approve.
    let request = wait_for(&mut rx, &mut seen, |kind| {
        matches!(kind, EventKind::ApprovalRequested { .. })
    })
    .await;
    let EventKind::ApprovalRequested {
        request_id,
        name,
        summary,
        always_label,
        ..
    } = &request.kind
    else {
        unreachable!()
    };
    assert_eq!((name.as_str(), summary.as_str()), ("write_file", "a.txt"));
    assert_eq!(always_label.as_deref(), Some("Always allow `write_file`"));
    assert_eq!(session.meta().status, SessionStatus::WaitingApproval);
    assert_eq!(h.file("a.txt"), None, "nothing happens before the answer");
    assert!(session.resolve_approval(request_id, ApprovalDecision::Allow, None));
    assert!(
        !session.resolve_approval(request_id, ApprovalDecision::Allow, None),
        "an approval is answered once"
    );

    // Second edit: deny with feedback.
    let request = wait_for(&mut rx, &mut seen, |kind| {
        matches!(kind, EventKind::ApprovalRequested { .. })
    })
    .await;
    let EventKind::ApprovalRequested { request_id, .. } = &request.kind else {
        unreachable!()
    };
    assert_eq!(h.file("a.txt").as_deref(), Some("A"));
    session.resolve_approval(request_id, ApprovalDecision::Deny, Some("not that file".into()));

    wait_for(&mut rx, &mut seen, |kind| {
        matches!(kind, EventKind::TurnFinished { .. })
    })
    .await;
    assert_eq!(outcome(&seen), TurnOutcome::Completed);
    assert_eq!(h.file("b.txt"), None);
    let results = tool_results(&seen);
    assert!(!results[0].2);
    assert!(results[1].2);
    assert!(results[1].1.contains("The user declined this action"));
    assert!(results[1].1.contains("not that file"));
    assert_eq!(session.meta().status, SessionStatus::Idle);
    // The model was told about the denial.
    let last_request = h.requests().pop().unwrap();
    assert!(matches!(
        &last_request.messages.last().unwrap().content[0],
        ContentBlock::ToolResult { is_error: true, .. }
    ));
}

#[tokio::test]
async fn always_allow_stops_asking_for_matching_commands() {
    let h = Harness::scripted();
    let id = h.create(PermissionMode::AutoEdit);
    let session = h.manager.get(&id).unwrap();
    let mut rx = h.subscribe(&id);
    let mut seen = Vec::new();
    h.manager
        .send_message(
            &id,
            UserInput::text("!tool bash {\"command\": \"touch one.txt\"}\n!then\n!tool bash {\"command\": \"touch two.txt\"}\n!tool bash {\"command\": \"ls\"}\n!then\n!say done"),
        )
        .unwrap();
    let request = wait_for(&mut rx, &mut seen, |kind| {
        matches!(kind, EventKind::ApprovalRequested { .. })
    })
    .await;
    let EventKind::ApprovalRequested {
        request_id,
        always_label,
        reason,
        ..
    } = &request.kind
    else {
        unreachable!()
    };
    assert_eq!(always_label.as_deref(), Some("Always allow `touch …`"));
    assert!(reason.contains("Commands need approval"));
    session.resolve_approval(request_id, ApprovalDecision::AllowAlways, None);

    wait_for(&mut rx, &mut seen, |kind| {
        matches!(kind, EventKind::TurnFinished { .. })
    })
    .await;
    let approvals = seen
        .iter()
        .filter(|event| matches!(event.kind, EventKind::ApprovalRequested { .. }))
        .count();
    assert_eq!(approvals, 1, "the second touch and the read-only ls run without asking");
    assert!(h.file("one.txt").is_some() && h.file("two.txt").is_some());

    // The rule survives a restart of the process.
    drop(session);
    let reopened = SessionManager::open(Paths::at(h.home.path())).unwrap();
    reopened.registry().inject(
        "mock",
        Arc::new(MockProvider::demo("mock").with_chunk_delay(Duration::ZERO)),
    );
    let session = reopened.get(&id).unwrap();
    let mut rx = session.subscribe(session.meta().last_seq).1;
    reopened
        .send_message(&id, UserInput::text("!tool bash {\"command\": \"touch three.txt\"}"))
        .unwrap();
    let events = collect_turn(&mut rx).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ApprovalRequested { .. }))
    );
    assert!(h.file("three.txt").is_some());
}

#[tokio::test]
async fn plan_mode_blocks_edits_and_mode_changes_are_announced() {
    let h = Harness::scripted();
    let id = h.create(PermissionMode::Plan);
    let events = h
        .turn(
            &id,
            "!tool write_file {\"path\": \"x.txt\", \"content\": \"x\"}\n!then\n!say Here is the plan.",
        )
        .await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    assert_eq!(h.file("x.txt"), None);
    let results = tool_results(&events);
    assert!(results[0].2 && results[0].1.contains("Plan mode is read-only"));
    assert!(user_text(&h.requests()[0], 0).contains("Permission mode: plan"));

    h.manager
        .update(
            &id,
            UpdateSession {
                mode: Some(PermissionMode::FullAuto),
                ..Default::default()
            },
        )
        .unwrap();
    let events = h
        .turn(&id, "!tool write_file {\"path\": \"x.txt\", \"content\": \"x\"}")
        .await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    assert_eq!(h.file("x.txt").as_deref(), Some("x"));
    let second_turn = h.requests()[2].clone();
    assert!(user_text(&second_turn, 0).contains("Permission mode: full-auto"));

    h.turn(&id, "!say nothing to do").await;
    let third_turn = h.requests().pop().unwrap();
    assert!(
        !user_text(&third_turn, 0).contains("Permission mode"),
        "an unchanged mode is not repeated"
    );
}

#[tokio::test]
async fn parallel_calls_come_back_in_order_and_bad_calls_are_explained() {
    let h = Harness::scripted();
    std::fs::write(h.workspace.path().join("a.txt"), "AAA").unwrap();
    std::fs::write(h.workspace.path().join("b.txt"), "BBB").unwrap();
    let id = h.create(PermissionMode::FullAuto);
    let events = h
        .turn(
            &id,
            "!tool read_file {\"path\": \"a.txt\"}\n!tool read_file {\"path\": \"b.txt\"}\n!tool no_such_tool {}\n!tool read_file {\"wrong\": 1}\n!then\n!say ok",
        )
        .await;
    assert_eq!(tool_results(&events).len(), 4, "every call is reported to the UI");

    // All four results go back in a single user message, in call order,
    // however the concurrent reads happened to finish.
    let follow_up = h.requests()[1].clone();
    let last = follow_up.messages.last().unwrap();
    let results: Vec<(&str, bool)> = last
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, is_error, .. } => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 4);
    assert!(results[0].0.contains("AAA") && !results[0].1);
    assert!(results[1].0.contains("BBB") && !results[1].1);
    assert!(results[2].1 && results[2].0.contains("Unknown tool `no_such_tool`"));
    assert!(results[3].1 && results[3].0.contains("Invalid arguments"));
    let assistant = &follow_up.messages[follow_up.messages.len() - 2];
    let call_ids: Vec<&str> = assistant.tool_calls().map(|call| call.id).collect();
    let result_ids: Vec<&str> = last
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(call_ids, result_ids);
}

#[tokio::test]
async fn truncated_or_malformed_tool_calls_are_never_executed() {
    let truncated = ContentBlock::ToolCall {
        id: "c1".into(),
        name: "write_file".into(),
        input: json!({"path": "cut.txt", "content": "partial"}),
        invalid_json: None,
        meta: None,
    };
    let malformed = ContentBlock::ToolCall {
        id: "c2".into(),
        name: "write_file".into(),
        input: json!({}),
        invalid_json: Some("{\"path\": \"bad.txt\", \"content\": \"oops".into()),
        meta: None,
    };
    let h = Harness::new(MockProvider::scripted(vec![
        MockReply::Message {
            content: vec![truncated],
            stop_reason: Some(StopReason::MaxTokens),
            usage: None,
        },
        MockReply::Message {
            content: vec![malformed],
            stop_reason: None,
            usage: None,
        },
        MockReply::Message {
            content: vec![ContentBlock::text("cut off mid-sen")],
            stop_reason: Some(StopReason::MaxTokens),
            usage: None,
        },
        MockReply::text("tence, finished."),
    ]));
    let id = h.create(PermissionMode::FullAuto);
    let events = h.turn(&id, "write files").await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    assert_eq!(h.file("cut.txt"), None);
    assert_eq!(h.file("bad.txt"), None);
    let results = tool_results(&events);
    assert!(results[0].2 && results[0].1.contains("output token limit"));
    assert!(results[1].2 && results[1].1.contains("INVALID_JSON"));
    let requests = h.requests();
    assert!(user_text(&requests[3], 0).contains("Continue exactly where you"));
}

#[tokio::test]
async fn sub_agents_run_in_parallel_with_their_own_context_and_tools() {
    let h = Harness::scripted();
    let id = h.create_with(PermissionMode::FullAuto, "orchestrator");
    let writer = json!({"description": "Write alpha", "prompt": "!tool write_file {\"path\": \"alpha.txt\", \"content\": \"a\"}\n!then\n!say alpha written"});
    let explorer = json!({"description": "Look around", "agent_type": "explore", "prompt": "!tool write_file {\"path\": \"beta.txt\", \"content\": \"b\"}\n!then\n!say cannot write"});
    let events = h
        .turn(
            &id,
            &format!("!tool task {writer}\n!tool task {explorer}\n!then\n!say integrated"),
        )
        .await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    assert_eq!(h.file("alpha.txt").as_deref(), Some("a"));
    assert_eq!(h.file("beta.txt"), None, "explore agents have no write tools");

    let spawned: Vec<(&Event, &str, &str)> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::AgentSpawned {
                parent_agent_id,
                agent_type,
                description,
                ..
            } => {
                assert_eq!(parent_agent_id, ROOT_AGENT);
                Some((event, agent_type.as_str(), description.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(spawned.len(), 2);
    let writer_id = &spawned
        .iter()
        .find(|(_, kind, _)| *kind == "general")
        .unwrap()
        .0
        .agent_id;
    let explorer_id = &spawned
        .iter()
        .find(|(_, kind, _)| *kind == "explore")
        .unwrap()
        .0
        .agent_id;
    assert_ne!(writer_id, explorer_id);
    assert_eq!(assistant_texts(&events, writer_id), ["alpha written"]);
    assert!(events.iter().any(|event| &event.agent_id == explorer_id
        && matches!(&event.kind, EventKind::ToolFinished { output, is_error: true, .. } if output.contains("Unknown tool `write_file`"))));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.kind,
                EventKind::AgentFinished {
                    outcome: TurnOutcome::Completed,
                    ..
                }
            ))
            .count(),
        2
    );

    // The reports are the task tool results of the root agent. The agents run
    // concurrently, so their completion events can arrive in either order.
    let mut reports: Vec<String> = events
        .iter()
        .filter(|event| event.agent_id == ROOT_AGENT)
        .filter_map(|event| match &event.kind {
            EventKind::ToolFinished { name, output, .. } if name == "task" => Some(output.clone()),
            _ => None,
        })
        .collect();
    reports.sort();
    assert_eq!(reports, ["alpha written", "cannot write"]);
    // The model, however, gets the results in the order it made the calls.
    let integration = h
        .requests()
        .into_iter()
        .rev()
        .find(|request| request.system.contains("# Your role: orchestrator"))
        .unwrap();
    let fed_back: Vec<&str> = integration
        .messages
        .last()
        .unwrap()
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(fed_back, ["alpha written", "cannot write"]);

    // Sub-agents start from their prompt alone, with a sub-agent system prompt.
    let requests = h.requests();
    let sub_request = requests
        .iter()
        .find(|request| request.system.contains("# Your role: sub-agent"))
        .unwrap();
    assert_eq!(sub_request.messages.len(), 1);
    assert!(requests[0].system.contains("# Your role: orchestrator"));
    let explore_request = requests
        .iter()
        .find(|request| request.system.contains("read-only research agent"))
        .unwrap();
    let explore_tools: Vec<&str> = explore_request.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(explore_tools, ["glob", "grep", "list_dir", "read_file", "web_fetch"]);
    // Usage of the sub-agents is part of the session total.
    assert!(h.manager.get(&id).unwrap().meta().usage.output_tokens > 0);
}

#[tokio::test]
async fn isolated_sub_agents_work_on_their_own_branch() {
    let h = Harness::scripted();
    init_repo(h.workspace.path());
    let id = h.create(PermissionMode::FullAuto);
    let task = json!({"description": "Add feature file", "isolation": "worktree",
        "prompt": "!tool write_file {\"path\": \"feature.txt\", \"content\": \"new\"}\n!then\n!say feature added"});
    let events = h
        .turn(&id, &format!("!tool task {task}\n!then\n!say merged later"))
        .await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);

    let spawned = events
        .iter()
        .find(|event| matches!(event.kind, EventKind::AgentSpawned { .. }))
        .unwrap();
    let EventKind::AgentSpawned {
        worktree: Some(worktree),
        ..
    } = &spawned.kind
    else {
        panic!("expected a worktree")
    };
    assert!(
        !Path::new(worktree).exists(),
        "the checkout is removed when the agent is done"
    );
    assert_eq!(h.file("feature.txt"), None, "the workspace is untouched");

    let branch = format!("swe/{}", spawned.agent_id);
    let report = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolFinished { name, output, .. } if name == "task" => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert!(report.starts_with("feature added"));
    assert!(report.contains(&format!("branch `{branch}` (1 commit")), "{report}");
    assert_eq!(
        git(h.workspace.path(), &["show", &format!("{branch}:feature.txt")]),
        "new"
    );
    assert_eq!(
        git(h.workspace.path(), &["log", "-1", "--format=%s", &branch]),
        "Add feature file"
    );
}

#[tokio::test]
async fn cancelling_stops_the_model_and_running_tools() {
    // Mid-stream: the partial text the user saw is kept.
    let h = Harness::new(
        MockProvider::from_fn(|_| MockReply::text("word ".repeat(400))).with_chunk_delay(Duration::from_millis(20)),
    );
    let id = h.create(PermissionMode::FullAuto);
    let mut rx = h.subscribe(&id);
    let mut seen = Vec::new();
    h.manager.send_message(&id, UserInput::text("talk")).unwrap();
    wait_for(&mut rx, &mut seen, |kind| {
        matches!(kind, EventKind::AssistantDelta { .. })
    })
    .await;
    assert!(h.manager.get(&id).unwrap().is_running());
    h.manager.cancel(&id).unwrap();
    wait_for(&mut rx, &mut seen, |kind| {
        matches!(kind, EventKind::TurnFinished { .. })
    })
    .await;
    assert_eq!(outcome(&seen), TurnOutcome::Cancelled);
    let partial = assistant_texts(&seen, ROOT_AGENT);
    assert_eq!(partial.len(), 1);
    assert!(partial[0].starts_with("word ") && partial[0].len() < 2000);
    let session = h.manager.get(&id).unwrap();
    assert_eq!(session.meta().status, SessionStatus::Idle);
    assert!(!session.is_running());

    // During a tool: the command is killed and the history stays well-formed.
    let h = Harness::scripted();
    let id = h.create(PermissionMode::FullAuto);
    let mut rx = h.subscribe(&id);
    let mut seen = Vec::new();
    let started = std::time::Instant::now();
    h.manager
        .send_message(
            &id,
            UserInput::text("!tool bash {\"command\": \"sleep 30\"}\n!then\n!say after"),
        )
        .unwrap();
    wait_for(&mut rx, &mut seen, |kind| matches!(kind, EventKind::ToolStarted { .. })).await;
    h.manager.cancel(&id).unwrap();
    wait_for(&mut rx, &mut seen, |kind| {
        matches!(kind, EventKind::TurnFinished { .. })
    })
    .await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(outcome(&seen), TurnOutcome::Cancelled);
    assert!(tool_results(&seen)[0].1.contains("stopped by the user"));

    // The next turn works: every tool call in the history has its result.
    let events = h.turn(&id, "!say still here").await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    let request = h.requests().pop().unwrap();
    let calls: usize = request.messages.iter().map(|m| m.tool_calls().count()).sum();
    let results: usize = request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .count();
    assert_eq!((calls, results), (1, 1));
}

#[tokio::test]
async fn transient_failures_are_retried_and_fatal_ones_end_the_turn() {
    let overloaded = || LlmError::Api {
        provider: "mock".into(),
        status: 529,
        message: "Overloaded".into(),
        retryable: true,
        retry_after: Some(Duration::from_millis(10)),
    };
    let h = Harness::new(MockProvider::scripted(vec![
        MockReply::Error(overloaded()),
        MockReply::Error(LlmError::Stream("connection reset".into())),
        MockReply::text("recovered"),
        MockReply::Error(LlmError::Auth("invalid key".into())),
        MockReply::text("works again"),
    ]));
    let id = h.create(PermissionMode::FullAuto);

    // A stream error without a retry-after waits for the backoff (~2s).
    let events = h.turn(&id, "go").await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    let retries: Vec<(u32, &str)> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Retrying { attempt, message, .. } => Some((*attempt, message.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 2);
    assert_eq!(retries[0].0, 1);
    assert!(retries[0].1.contains("Overloaded") && retries[1].1.contains("connection reset"));
    assert_eq!(assistant_texts(&events, ROOT_AGENT), ["recovered"]);

    let events = h.turn(&id, "again").await;
    assert_eq!(outcome(&events), TurnOutcome::Error);
    assert!(
        events
            .iter()
            .any(|event| matches!(&event.kind, EventKind::Error { message } if message.contains("invalid key")))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::Retrying { .. }))
    );
    let meta = h.manager.get(&id).unwrap().meta();
    assert_eq!(meta.status, SessionStatus::Error);
    assert!(meta.error.unwrap().contains("invalid key"));

    // The failed turn's message is still part of the conversation.
    let events = h.turn(&id, "once more").await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    let request = h.requests().pop().unwrap();
    let last_user = request.messages.last().unwrap().text();
    assert!(last_user.contains("again") && last_user.contains("once more"));
    assert_eq!(h.manager.get(&id).unwrap().meta().status, SessionStatus::Idle);
}

#[tokio::test]
async fn long_conversations_are_compacted_into_a_summary() {
    let config = Config {
        compact_threshold_tokens: 10_000,
        ..Default::default()
    };
    let provider = MockProvider::from_fn(|request| {
        if request.system.contains("hand-over summaries") {
            return MockReply::text("SUMMARY: the user wants a parser; lexer.rs is done.");
        }
        let turns = request.messages.iter().filter(|m| m.role == Role::Assistant).count();
        match turns {
            // The first answer reports a context far above the threshold.
            0 if request.messages.len() == 1 && !request.messages[0].text().contains("SUMMARY") => {
                MockReply::text("First answer.").with_usage(Usage {
                    input_tokens: 50_000,
                    output_tokens: 100,
                    ..Default::default()
                })
            }
            _ => MockReply::text("Continuing after the summary."),
        }
    });
    let h = Harness::with_config(provider, config);
    let id = h.create(PermissionMode::FullAuto);
    h.turn(&id, "Build a parser").await;
    assert_eq!(h.manager.get(&id).unwrap().meta().context_tokens, 50_100);

    let events = h.turn(&id, "Now add error recovery").await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    let compacted = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Compacted { summary, tokens_before } => Some((summary.clone(), *tokens_before)),
            _ => None,
        })
        .expect("a compaction event");
    assert_eq!(
        compacted,
        (
            "SUMMARY: the user wants a parser; lexer.rs is done.".to_string(),
            50_100
        )
    );

    let requests = h.requests();
    assert_eq!(requests.len(), 3);
    // The summariser sees the old transcript, without tools.
    assert!(requests[1].tools.is_empty());
    let transcript = requests[1].messages[0].text();
    assert!(transcript.contains("Build a parser") && transcript.contains("First answer."));
    assert!(
        !transcript.contains("Now add error recovery"),
        "the unanswered message is not summarised away"
    );
    // The next request is the summary plus the pending message, and nothing else.
    assert_eq!(requests[2].messages.len(), 1);
    let resumed = requests[2].messages[0].text();
    assert!(resumed.contains("<summary>\nSUMMARY: the user wants a parser"));
    assert!(resumed.contains("The user's latest message follows."));
    assert!(resumed.ends_with("Now add error recovery"));
    assert_eq!(requests[2].system, requests[0].system);
}

#[tokio::test]
async fn a_context_overflow_from_the_provider_triggers_compaction() {
    let calls = Arc::new(Mutex::new(0u32));
    let counter = calls.clone();
    let provider = MockProvider::from_fn(move |request| {
        if request.system.contains("hand-over summaries") {
            return MockReply::text("SUMMARY of earlier work.");
        }
        let mut calls = counter.lock().unwrap();
        *calls += 1;
        match *calls {
            1 => MockReply::text("ok"),
            2 => MockReply::Error(LlmError::ContextOverflow("prompt is too long".into())),
            _ => MockReply::text("fits now"),
        }
    });
    let h = Harness::new(provider);
    let id = h.create(PermissionMode::FullAuto);
    h.turn(&id, "first").await;
    let events = h.turn(&id, "second").await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::Compacted { .. }))
    );
    assert_eq!(assistant_texts(&events, ROOT_AGENT), ["fits now"]);
    assert!(
        h.requests().pop().unwrap().messages[0]
            .text()
            .contains("SUMMARY of earlier work.")
    );
}

#[tokio::test]
async fn messages_sent_during_a_turn_steer_it() {
    let h = Harness::scripted();
    let id = h.create(PermissionMode::FullAuto);
    let mut rx = h.subscribe(&id);
    let mut seen = Vec::new();
    let queued = h
        .manager
        .send_message(
            &id,
            UserInput::text("!tool bash {\"command\": \"sleep 0.5\"}\n!then\n!say first plan finished"),
        )
        .unwrap();
    assert!(!queued);
    wait_for(&mut rx, &mut seen, |kind| matches!(kind, EventKind::ToolStarted { .. })).await;
    let queued = h
        .manager
        .send_message(&id, UserInput::text("!say steering received"))
        .unwrap();
    assert!(queued, "a running turn queues the message instead of starting another");

    wait_for(&mut rx, &mut seen, |kind| {
        matches!(kind, EventKind::TurnFinished { .. })
    })
    .await;
    let user_messages: Vec<&str> = seen
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(user_messages.len(), 2);
    assert_eq!(user_messages[1], "!say steering received");
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event.kind, EventKind::TurnFinished { .. }))
            .count(),
        1
    );
    // The steering text rode along with the tool result of the step in flight.
    let request = h.requests()[1].clone();
    let last = request.messages.last().unwrap();
    assert!(matches!(&last.content[0], ContentBlock::ToolResult { .. }));
    assert_eq!(last.text(), "!say steering received");
    assert_eq!(assistant_texts(&seen, ROOT_AGENT), ["steering received"]);
}

#[tokio::test]
async fn sessions_survive_a_restart() {
    let h = Harness::scripted();
    let id = h.create(PermissionMode::FullAuto);
    h.turn(&id, "Remember this\n!tool todo_write {\"todos\": [{\"content\": \"Ship it\", \"status\": \"in_progress\"}]}\n!then\n!say noted").await;
    let before = h.manager.get(&id).unwrap().snapshot();
    assert_eq!(before.todos.len(), 1);
    assert_eq!(before.todos[0].status, TodoStatus::InProgress);

    // Simulate a crash mid-turn by leaving the status at "running".
    let meta_file = h.home.path().join("sessions").join(&id).join("meta.json");
    let mut meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&meta_file).unwrap()).unwrap();
    meta["status"] = json!("running");
    std::fs::write(&meta_file, meta.to_string()).unwrap();

    let reopened = SessionManager::open(Paths::at(h.home.path())).unwrap();
    let provider = MockProvider::demo("mock").with_chunk_delay(Duration::ZERO);
    let log = provider.request_log();
    reopened.registry().inject("mock", Arc::new(provider));
    let listed = reopened.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, SessionStatus::Interrupted);
    assert_eq!(listed[0].title, "Remember this");

    let session = reopened.get(&id).unwrap();
    let after = session.snapshot();
    assert_eq!(after.events, before.events);
    assert_eq!(after.todos, before.todos);

    // The conversation continues with its full history and the same system prompt.
    let mut rx = session.subscribe(after.meta.last_seq).1;
    reopened
        .send_message(&id, UserInput::text("!say welcome back"))
        .unwrap();
    let events = collect_turn(&mut rx).await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    assert_eq!(events[0].seq, before.meta.last_seq + 1, "sequence numbers continue");
    let request = log.lock().unwrap().last().unwrap().clone();
    assert_eq!(request.system, h.requests()[0].system);
    assert!(request.messages.len() >= 5);
    assert!(request.messages[0].text().contains("Remember this"));

    // Replay from a sequence number returns exactly the newer events.
    let (replay, _) = session.subscribe(before.meta.last_seq);
    assert_eq!(
        replay.len(),
        events.iter().filter(|event| !event.kind.is_transient()).count()
    );
}

#[tokio::test]
async fn sessions_can_be_listed_updated_and_deleted() {
    let h = Harness::scripted();
    let first = h.create(PermissionMode::Ask);
    let second = h.create(PermissionMode::Ask);
    h.turn(&second, "!say hi").await;
    assert_eq!(
        h.manager.list().iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        [second.clone(), first.clone()]
    );

    let mut global = h.manager.subscribe_global();
    let updated = h
        .manager
        .update(
            &first,
            UpdateSession {
                title: Some("  Renamed ".into()),
                model: Some("other".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!((updated.title.as_str(), updated.model.as_str()), ("Renamed", "other"));
    assert!(
        matches!(global.recv().await.unwrap(), swe_core::GlobalEvent::SessionUpdated { session } if session.title == "Renamed")
    );

    h.manager.delete(&first).unwrap();
    assert!(matches!(global.recv().await.unwrap(), swe_core::GlobalEvent::SessionDeleted { id } if id == first));
    assert!(h.manager.get(&first).is_err());
    assert!(!h.home.path().join("sessions").join(&first).exists());
    assert_eq!(h.manager.list().len(), 1);

    // Invalid requests are rejected up front.
    assert!(h.manager.send_message(&second, UserInput::text("   ")).is_err());
    assert!(
        h.manager
            .create(CreateSession {
                workspace: "/definitely/not/here".into(),
                provider: Some("mock".into()),
                model: Some("demo".into()),
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        h.manager
            .create(CreateSession {
                workspace: h.workspace.path().to_string_lossy().into_owned(),
                ..Default::default()
            })
            .is_err(),
        "a model is required"
    );
}

#[tokio::test]
async fn a_missing_api_key_is_reported_as_a_turn_error() {
    let h = Harness::scripted();
    let id = h
        .manager
        .create(CreateSession {
            workspace: h.workspace.path().to_string_lossy().into_owned(),
            provider: Some("no-such-provider".into()),
            model: Some("m".into()),
            ..Default::default()
        })
        .unwrap()
        .id;
    let events = h.turn(&id, "hello").await;
    assert_eq!(outcome(&events), TurnOutcome::Error);
    assert!(
        events
            .iter()
            .any(|event| matches!(&event.kind, EventKind::Error { message } if message.contains("unknown provider")))
    );
}
