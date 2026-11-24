//! `swe eval`: run a suite of tasks with one model and report what it solved.
//!
//! A suite is a directory with one sub-directory per task:
//!
//! ```text
//! my-suite/
//!   fix-pagination/
//!     task.toml     prompt = "..."   check = "python test.py"   timeout_secs = 600 (optional)
//!     repo/         the files the agent starts with
//!     hidden/       optional: copied in only before the check runs (tests the agent must not see)
//! ```
//!
//! Each task runs in a fresh copy of `repo/` in full-auto mode, so the agent is
//! never stopped by an approval. Afterwards the check command runs in that
//! copy; exit code 0 means solved. The agent has your user's permissions while
//! it works: run suites you trust, ideally in a container.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use swe_core::{CreateSession, SessionManager, UserInput};
use swe_protocol::{ApprovalDecision, Effort, EventKind, PermissionMode, TurnOutcome, Usage};
use swe_tools::{Tool, ToolContext};

#[derive(clap::Args)]
pub struct EvalArgs {
    /// Directory with one sub-directory per task.
    suite: PathBuf,
    #[arg(long)]
    provider: Option<String>,
    #[arg(short, long)]
    model: Option<String>,
    /// low | medium | high | xhigh
    #[arg(long)]
    effort: Option<Effort>,
    /// coder | orchestrator
    #[arg(long)]
    agent: Option<String>,
    /// Run only tasks whose name contains this text.
    #[arg(long)]
    task: Option<String>,
    /// Time limit per task in seconds; a task's own `timeout_secs` wins.
    #[arg(long, default_value_t = 900)]
    timeout_secs: u64,
    /// Write the full results as JSON to this file.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Keep the sessions and working copies for inspection.
    #[arg(long)]
    keep: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    prompt: String,
    /// Shell command run in the working copy afterwards; exit code 0 means solved.
    check: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Serialize)]
struct TaskResult {
    task: String,
    solved: bool,
    /// How the agent's turn ended: completed, error, timeout, ...
    outcome: String,
    seconds: f64,
    /// Model requests, sub-agents included.
    steps: u32,
    tool_calls: u32,
    failed_tool_calls: u32,
    usage: Usage,
    /// The end of the check command's output.
    check_output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Set with `--keep`.
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
}

const CHECK_TIMEOUT: Duration = Duration::from_secs(300);
const CHECK_OUTPUT_CHARS: usize = 2_000;

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Make the working copy a git repository with one commit, as real projects are.
/// Tasks still run when git is missing; the agent then simply sees no repository.
fn init_git(dir: &Path) {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=eval",
                "-c",
                "user.email=eval@localhost",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    let _ = git(&["init", "-q"]) && git(&["add", "-A"]) && git(&["commit", "-q", "-m", "Initial state"]);
}

fn tail(text: &str, max_chars: usize) -> String {
    let skip = text.chars().count().saturating_sub(max_chars);
    text.chars().skip(skip).collect()
}

async fn run_one(
    manager: &SessionManager,
    args: &EvalArgs,
    name: &str,
    dir: &Path,
    run_id: &str,
) -> anyhow::Result<TaskResult> {
    let file = dir.join("task.toml");
    let task: TaskFile =
        toml::from_str(&std::fs::read_to_string(&file).with_context(|| format!("{}", file.display()))?)
            .with_context(|| format!("{}", file.display()))?;

    let workspace = std::env::temp_dir().join(format!("swe-eval-{run_id}-{name}"));
    if workspace.exists() {
        std::fs::remove_dir_all(&workspace)?;
    }
    let repo = dir.join("repo");
    if repo.is_dir() {
        copy_dir(&repo, &workspace)?;
    } else {
        std::fs::create_dir_all(&workspace)?;
    }
    init_git(&workspace);

    let meta = manager.create(CreateSession {
        workspace: workspace.to_string_lossy().into_owned(),
        engine: None,
        provider: args.provider.clone(),
        model: args.model.clone(),
        mode: Some(PermissionMode::FullAuto),
        effort: args.effort,
        agent: args.agent.clone(),
        title: Some(format!("eval: {name}")),
    })?;
    let session = manager.get(&meta.id)?;
    let (_, mut events) = session.subscribe(0);
    let started = Instant::now();
    manager.send_message(&meta.id, UserInput::text(task.prompt))?;

    let limit = Duration::from_secs(task.timeout_secs.unwrap_or(args.timeout_secs));
    let (mut steps, mut tool_calls, mut failed_tool_calls) = (0u32, 0u32, 0u32);
    let mut error = None;
    let mut timed_out = false;
    let outcome = loop {
        let remaining = limit.saturating_sub(started.elapsed());
        // Once the limit has passed the turn is cancelled; give it a moment to wind down.
        let wait = if timed_out { Duration::from_secs(30) } else { remaining };
        let event = match tokio::time::timeout(wait, events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break "error".to_string(),
            Err(_) if timed_out => break "timeout".to_string(),
            Err(_) => {
                timed_out = true;
                let _ = manager.cancel(&meta.id);
                continue;
            }
        };
        match event.kind {
            EventKind::AssistantMessage { .. } => steps += 1,
            EventKind::ToolFinished { is_error, .. } => {
                tool_calls += 1;
                failed_tool_calls += u32::from(is_error);
            }
            // Full-auto does not ask; anything that still does is not answered by a person here.
            EventKind::ApprovalRequested { request_id, .. } => {
                session.resolve_approval(&request_id, ApprovalDecision::Deny, None);
            }
            EventKind::Error { message } => error = Some(message),
            EventKind::TurnFinished { outcome, .. } if timed_out && outcome == TurnOutcome::Cancelled => {
                break "timeout".to_string();
            }
            EventKind::TurnFinished { outcome, .. } => break format!("{outcome:?}").to_lowercase(),
            _ => {}
        }
    };
    let seconds = started.elapsed().as_secs_f64();

    let hidden = dir.join("hidden");
    if hidden.is_dir() {
        copy_dir(&hidden, &workspace)?;
    }
    let check = swe_tools::shell::Bash
        .run(
            json!({"command": task.check, "timeout_ms": CHECK_TIMEOUT.as_millis() as u64}),
            &ToolContext::new(&workspace),
        )
        .await;

    let usage = session.meta().usage;
    let mut result = TaskResult {
        task: name.to_string(),
        solved: !check.is_error,
        outcome,
        seconds,
        steps,
        tool_calls,
        failed_tool_calls,
        usage,
        check_output: tail(&check.content, CHECK_OUTPUT_CHARS),
        error,
        workspace: None,
        session: None,
    };
    if args.keep {
        result.workspace = Some(workspace);
        result.session = Some(meta.id);
    } else {
        let _ = manager.delete(&meta.id);
        let _ = std::fs::remove_dir_all(&workspace);
    }
    Ok(result)
}

fn thousands(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

pub async fn run(manager: SessionManager, args: EvalArgs) -> anyhow::Result<ExitCode> {
    let mut tasks: Vec<(String, PathBuf)> = std::fs::read_dir(&args.suite)
        .with_context(|| format!("cannot read the suite directory {}", args.suite.display()))?
        .flatten()
        .filter(|entry| entry.path().join("task.toml").is_file())
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path()))
        .filter(|(name, _)| args.task.as_deref().is_none_or(|wanted| name.contains(wanted)))
        .collect();
    tasks.sort();
    anyhow::ensure!(
        !tasks.is_empty(),
        "no tasks found in {}: a task is a directory with a task.toml",
        args.suite.display()
    );

    let run_id = std::process::id().to_string();
    let mut results = Vec::new();
    for (name, dir) in &tasks {
        eprint!("{name} ... ");
        let result = match run_one(&manager, &args, name, dir, &run_id).await {
            Ok(result) => result,
            Err(err) => {
                eprintln!("skipped: {err:#}");
                continue;
            }
        };
        eprintln!(
            "{}  {:.0}s  {} steps  {} tool calls ({} failed)  {} in / {} out  [{}]",
            if result.solved { "PASS" } else { "FAIL" },
            result.seconds,
            result.steps,
            result.tool_calls,
            result.failed_tool_calls,
            thousands(result.usage.input_tokens),
            thousands(result.usage.output_tokens),
            result.outcome,
        );
        results.push(result);
    }

    let solved = results.iter().filter(|result| result.solved).count();
    let mut usage = Usage::default();
    for result in &results {
        usage.add(&result.usage);
    }
    println!(
        "\nSolved {solved} of {} ({} of {} tasks ran).  Tokens: {} in ({} from cache), {} out.",
        results.len(),
        results.len(),
        tasks.len(),
        thousands(usage.input_tokens),
        thousands(usage.cache_read_tokens),
        thousands(usage.output_tokens),
    );
    if let Some(out) = &args.out {
        let report = json!({
            "suite": args.suite,
            "provider": args.provider,
            "model": args.model,
            "effort": args.effort,
            "agent": args.agent,
            "solved": solved,
            "tasks": results.len(),
            "usage": usage,
            "results": results,
        });
        std::fs::write(out, serde_json::to_vec_pretty(&report)?).with_context(|| format!("{}", out.display()))?;
        println!("Results written to {}", out.display());
    }
    Ok(if solved == tasks.len() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
