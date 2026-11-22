//! Vendor CLI engines, exercised against fake `claude` / `codex` executables
//! that speak the same JSONL protocols.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use swe_core::{CreateSession, Paths, SessionManager, UserInput};
use swe_protocol::{Engine, Event, EventKind, PermissionMode, SessionStatus, TurnOutcome};

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn turn(manager: &SessionManager, id: &str, text: &str) -> Vec<Event> {
    let session = manager.get(id).unwrap();
    let mut rx = session.subscribe(session.meta().last_seq).1;
    manager.send_message(id, UserInput::text(text)).unwrap();
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("timed out").unwrap();
        let done = matches!(event.kind, EventKind::TurnFinished { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

fn outcome(events: &[Event]) -> TurnOutcome {
    events.iter().find_map(|e| match &e.kind { EventKind::TurnFinished { outcome, .. } => Some(*outcome), _ => None }).unwrap()
}

fn texts(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.kind { EventKind::AssistantMessage { message, .. } => Some(message.text()), _ => None })
        .collect()
}

fn errors(events: &[Event]) -> Vec<String> {
    events.iter().filter_map(|e| match &e.kind { EventKind::Error { message } => Some(message.clone()), _ => None }).collect()
}

// One test function: the binaries are selected through process-wide
// environment variables, which must not change under concurrently running tests.
#[tokio::test]
async fn sessions_can_run_on_a_vendor_cli() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let manager = SessionManager::open(Paths::at(home.path())).unwrap();
    let create = |engine: Engine, mode: PermissionMode| {
        manager
            .create(CreateSession {
                workspace: workspace.path().to_string_lossy().into_owned(),
                engine: Some(engine),
                mode: Some(mode),
                ..Default::default()
            })
            .unwrap()
            .id
    };

    // --- claude: echoes the prompt it got on stdin, its arguments and its cwd.
    let claude = script(
        bin.path(),
        "fake-claude",
        r#"prompt=$(cat)
echo '{"type":"system","subtype":"init","session_id":"sess-42","apiKeySource":"none"}'
echo 'this line is not JSON and must be ignored'
echo "{\"type\":\"assistant\",\"parent_tool_use_id\":null,\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"prompt=$prompt args=$* cwd=$(pwd -P)\"}],\"stop_reason\":\"end_turn\"}}"
echo '{"type":"result","subtype":"success","is_error":false,"session_id":"sess-42","modelUsage":{"m":{"inputTokens":10,"outputTokens":5,"cacheReadInputTokens":90}}}'"#,
    );
    // SAFETY: set before any engine process is spawned, in the only test of this binary.
    unsafe { std::env::set_var("SWE_AGENT_CLAUDE_BIN", &claude) };

    let id = create(Engine::ClaudeCli, PermissionMode::AutoEdit);
    let events = turn(&manager, &id, "hello").await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    let said = texts(&events).join("\n");
    assert!(said.contains("prompt=hello"), "{said}");
    assert!(said.contains("--output-format stream-json"), "{said}");
    assert!(said.contains("--permission-mode acceptEdits"), "{said}");
    assert!(!said.contains("--resume"), "{said}");
    assert!(said.contains(&format!("cwd={}", workspace.path().canonicalize().unwrap().display())), "{said}");
    let meta = manager.get(&id).unwrap().meta();
    assert_eq!(meta.external_session_id.as_deref(), Some("sess-42"));
    assert_eq!((meta.usage.input_tokens, meta.usage.output_tokens, meta.usage.cache_read_tokens), (100, 5, 90));
    assert_eq!(meta.status, SessionStatus::Idle);
    assert_eq!(meta.provider, "claude-cli");

    // The follow-up resumes the CLI's own conversation.
    let events = turn(&manager, &id, "again").await;
    assert!(texts(&events).join("\n").contains("--resume sess-42"));

    // Ask mode cannot be honoured interactively; the user is told.
    let ask = create(Engine::ClaudeCli, PermissionMode::Ask);
    let events = turn(&manager, &ask, "hello").await;
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Notice { text } if text.contains("cannot ask for approval"))));
    assert!(texts(&events).join("\n").contains("--permission-mode default"));

    // --- codex: thread id, a command, cumulative usage.
    let codex = script(
        bin.path(),
        "fake-codex",
        r#"prompt=$(cat)
args=$(printf '%s' "$*" | tr -d '"')
echo '{"type":"thread.started","thread_id":"thread-7"}'
echo '{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}'
printf '%s\n' '{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"ls","aggregated_output":"README.md\n","exit_code":0,"status":"completed"}}'
echo "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_1\",\"type\":\"agent_message\",\"text\":\"prompt=$prompt args=$args\"}}"
echo '{"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":50,"reasoning_output_tokens":20}}'"#,
    );
    unsafe { std::env::set_var("SWE_AGENT_CODEX_BIN", &codex) };
    let id = create(Engine::CodexCli, PermissionMode::FullAuto);
    let events = turn(&manager, &id, "list files").await;
    assert_eq!(outcome(&events), TurnOutcome::Completed);
    let said = texts(&events).join("\n");
    assert!(said.contains("prompt=list files") && said.contains("exec --json") && said.contains("--sandbox danger-full-access"), "{said}");
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::ToolFinished { name, output, is_error: false, .. } if name == "bash" && output == "README.md\n")));
    let events = turn(&manager, &id, "more").await;
    assert!(texts(&events).join("\n").contains("resume thread-7"));
    // The CLI reports thread totals; they are not double counted across turns.
    assert_eq!(manager.get(&id).unwrap().meta().usage.input_tokens, 1000);

    // --- a CLI that fails before producing anything.
    let broken = script(bin.path(), "fake-broken", "cat > /dev/null\necho 'Not logged in' >&2\nexit 3");
    unsafe { std::env::set_var("SWE_AGENT_CLAUDE_BIN", &broken) };
    let id = create(Engine::ClaudeCli, PermissionMode::AutoEdit);
    let events = turn(&manager, &id, "hello").await;
    assert_eq!(outcome(&events), TurnOutcome::Error);
    let message = errors(&events).join("\n");
    assert!(message.contains("exited with code 3") && message.contains("Not logged in"), "{message}");
    assert_eq!(manager.get(&id).unwrap().meta().status, SessionStatus::Error);

    // --- cancelling kills the process.
    let slow = script(bin.path(), "fake-slow", "cat > /dev/null\nsleep 30");
    unsafe { std::env::set_var("SWE_AGENT_CLAUDE_BIN", &slow) };
    let id = create(Engine::ClaudeCli, PermissionMode::AutoEdit);
    let session = manager.get(&id).unwrap();
    let mut rx = session.subscribe(0).1;
    manager.send_message(&id, UserInput::text("wait")).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    manager.cancel(&id).unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("timed out").unwrap();
        if let EventKind::TurnFinished { outcome, .. } = event.kind {
            assert_eq!(outcome, TurnOutcome::Cancelled);
            break;
        }
    }
    assert!(started.elapsed() < Duration::from_secs(5));

    // --- a CLI that is not installed.
    unsafe { std::env::set_var("SWE_AGENT_CLAUDE_BIN", bin.path().join("missing")) };
    let id = create(Engine::ClaudeCli, PermissionMode::AutoEdit);
    let events = turn(&manager, &id, "hello").await;
    assert!(errors(&events).join("\n").contains("was not found on PATH"));
    assert!(!swe_core::engines().iter().find(|e| e.id == Engine::ClaudeCli).unwrap().installed);
    assert!(swe_core::engines().iter().find(|e| e.id == Engine::CodexCli).unwrap().installed);
}
