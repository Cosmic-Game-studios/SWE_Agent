//! `swe`: start the web UI, run a task headless, manage providers.

use std::io::{IsTerminal, Read, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use swe_core::{CreateSession, KeyStatus, Paths, SessionManager, UserInput};
use swe_protocol::{
    ApprovalDecision, ContentBlock, Effort, Engine, EventKind, PermissionMode, ROOT_AGENT, TurnOutcome,
};
use swe_server::ServeOptions;

#[derive(Parser)]
#[command(name = "swe", version, about = "SWE Agent: an autonomous software engineer for any model")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the server and web UI (the default).
    Serve(ServeArgs),
    /// Run one task without the UI and print what the agent does.
    Run(RunArgs),
    /// List providers and where their API keys come from.
    Providers,
    /// List the models a provider offers.
    Models {
        provider: String,
    },
    /// Store or remove an API key.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// List sessions.
    Sessions,
    /// Show which engines are available on this machine.
    Engines,
    /// Print the directory that holds config, credentials and sessions.
    Home,
}

#[derive(clap::Args)]
struct ServeArgs {
    /// Address to listen on. Anything but loopback requires --token.
    #[arg(long, default_value = "127.0.0.1", env = "SWE_AGENT_HOST")]
    host: IpAddr,
    #[arg(long, default_value_t = 4477, env = "SWE_AGENT_PORT")]
    port: u16,
    /// Access token every API request must carry.
    #[arg(long, env = "SWE_AGENT_TOKEN")]
    token: Option<String>,
    /// Do not open the browser.
    #[arg(long)]
    no_open: bool,
    /// Additional web origin allowed to call the API (UI dev server).
    #[arg(long, env = "SWE_AGENT_DEV_ORIGIN")]
    dev_origin: Option<String>,
}

impl Default for ServeArgs {
    fn default() -> Self {
        ServeArgs { host: IpAddr::from([127, 0, 0, 1]), port: 4477, token: None, no_open: false, dev_origin: None }
    }
}

#[derive(clap::Args)]
struct RunArgs {
    /// The task. Read from stdin when omitted or `-`.
    prompt: Option<String>,
    /// Workspace directory.
    #[arg(short = 'C', long, default_value = ".")]
    dir: PathBuf,
    #[arg(long)]
    provider: Option<String>,
    #[arg(short, long)]
    model: Option<String>,
    /// plan | ask | auto-edit | full-auto
    #[arg(long)]
    mode: Option<PermissionMode>,
    /// low | medium | high | xhigh
    #[arg(long)]
    effort: Option<Effort>,
    /// coder | orchestrator
    #[arg(long)]
    agent: Option<String>,
    /// native | claude-cli | codex-cli
    #[arg(long)]
    engine: Option<Engine>,
    /// Print every event as a JSON line instead of readable text.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum KeyAction {
    /// Store a key (read from stdin unless --key is given).
    Set {
        provider: String,
        #[arg(long)]
        key: Option<String>,
    },
    /// Remove a stored key.
    Remove { provider: String },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SWE_AGENT_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let manager = SessionManager::open(Paths::discover()).context("cannot open the SWE Agent home directory")?;
    match cli.command.unwrap_or(Command::Serve(ServeArgs::default())) {
        Command::Serve(args) => {
            swe_server::serve(
                manager,
                ServeOptions {
                    host: args.host,
                    port: args.port,
                    token: args.token,
                    dev_origin: args.dev_origin,
                    open_browser: !args.no_open,
                },
            )
            .await?;
        }
        Command::Run(args) => return run_task(manager, args).await,
        Command::Providers => {
            for provider in manager.registry().list() {
                let key = match &provider.key {
                    KeyStatus::Env { variable } => format!("key from ${variable}"),
                    KeyStatus::Stored => "key stored".to_string(),
                    KeyStatus::NotNeeded => "no key needed".to_string(),
                    KeyStatus::Missing => match provider.key_envs.first() {
                        Some(variable) => format!("no key (set ${variable} or run `swe key set {}`)", provider.id),
                        None => "no key".to_string(),
                    },
                };
                println!("{:<12} {:<18} {}", provider.id, provider.protocol.as_str(), key);
            }
        }
        Command::Models { provider } => {
            let models = manager.registry().models(&provider, true).await?;
            for model in models {
                let window = model.context_window.map(|n| format!("{}k context", n / 1000)).unwrap_or_default();
                println!("{:<44} {window}", model.id);
            }
        }
        Command::Key { action } => match action {
            KeyAction::Set { provider, key } => {
                let key = match key {
                    Some(key) => key,
                    None => {
                        if std::io::stdin().is_terminal() {
                            eprint!("API key for {provider}: ");
                            std::io::stderr().flush().ok();
                        }
                        let mut line = String::new();
                        std::io::stdin().read_line(&mut line)?;
                        line
                    }
                };
                anyhow::ensure!(!key.trim().is_empty(), "no key given");
                manager.registry().set_key(&provider, Some(&key))?;
                println!("Stored the key for {provider} in {}", manager.paths().credentials_file().display());
            }
            KeyAction::Remove { provider } => {
                manager.registry().set_key(&provider, None)?;
                println!("Removed the stored key for {provider}");
            }
        },
        Command::Sessions => {
            for session in manager.list() {
                println!(
                    "{}  {:<10} {:<28} {}  ({})",
                    session.id,
                    format!("{:?}", session.status).to_lowercase(),
                    format!("{}/{}", session.provider, session.model),
                    session.title,
                    session.workspace
                );
            }
        }
        Command::Engines => {
            for engine in swe_core::engines() {
                let state = match (&engine.path, engine.installed) {
                    (Some(path), _) => path.clone(),
                    (None, true) => "built in".to_string(),
                    (None, false) => format!("`{}` not found on PATH", engine.binary),
                };
                println!("{:<12} {}", engine.id.as_str(), state);
            }
        }
        Command::Home => println!("{}", manager.paths().home.display()),
    }
    Ok(ExitCode::SUCCESS)
}

/// Ask on the terminal whether a tool call may run.
fn prompt_for_approval(name: &str, summary: &str, reason: &str, always: Option<&str>) -> ApprovalDecision {
    eprintln!("\n? {name}: {summary}\n  {reason}");
    let options = match always {
        Some(label) => format!("  [y] allow  [a] {}  [n] deny: ", label.to_lowercase()),
        None => "  [y] allow  [n] deny: ".to_string(),
    };
    eprint!("{options}");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return ApprovalDecision::Deny;
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Allow,
        "a" | "always" if always.is_some() => ApprovalDecision::AllowAlways,
        _ => ApprovalDecision::Deny,
    }
}

async fn run_task(manager: SessionManager, args: RunArgs) -> anyhow::Result<ExitCode> {
    let prompt = match args.prompt.as_deref() {
        Some(prompt) if prompt != "-" => prompt.to_string(),
        _ => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text
        }
    };
    anyhow::ensure!(!prompt.trim().is_empty(), "the prompt is empty");
    // Approvals can only be asked for when someone is there to answer.
    let interactive = std::io::stdin().is_terminal() && args.prompt.as_deref().is_some_and(|p| p != "-") && !args.json;

    let meta = manager.create(CreateSession {
        workspace: args.dir.to_string_lossy().into_owned(),
        engine: args.engine,
        provider: args.provider,
        model: args.model,
        mode: args.mode,
        effort: args.effort,
        agent: args.agent,
        title: None,
    })?;
    let session = manager.get(&meta.id)?;
    let (_, mut events) = session.subscribe(0);
    manager.send_message(&meta.id, UserInput::text(prompt))?;

    // Ctrl-C stops the turn; a second one ends the process.
    let interrupt_manager = manager.clone();
    let interrupt_id = meta.id.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nStopping (press Ctrl-C again to quit immediately)");
            let _ = interrupt_manager.cancel(&interrupt_id);
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        }
    });

    let mut streamed_text = false;
    let outcome = loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break TurnOutcome::Error,
        };
        if args.json {
            println!("{}", serde_json::to_string(&event)?);
        }
        let root = event.agent_id == ROOT_AGENT;
        let indent = if root { "" } else { "  " };
        match &event.kind {
            EventKind::TurnFinished { outcome, .. } => break *outcome,
            EventKind::ApprovalRequested { request_id, name, summary, reason, always_label, .. } => {
                let decision = if interactive {
                    prompt_for_approval(name, summary, reason, always_label.as_deref())
                } else {
                    if !args.json {
                        eprintln!("{indent}! {name}: {summary} needs approval and was denied (not interactive; use --mode auto-edit or full-auto)");
                    }
                    ApprovalDecision::Deny
                };
                session.resolve_approval(request_id, decision, None);
            }
            _ if args.json => {}
            EventKind::AssistantDelta { text } if root => {
                print!("{text}");
                std::io::stdout().flush().ok();
                streamed_text = true;
            }
            EventKind::AssistantMessage { message, .. } if root => {
                if streamed_text {
                    println!();
                } else {
                    // Engines without streaming deliver the text in one piece.
                    for block in &message.content {
                        if let ContentBlock::Text { text, .. } = block {
                            println!("{text}");
                        }
                    }
                }
                streamed_text = false;
            }
            EventKind::ToolStarted { name, summary, .. } => eprintln!("{indent}> {name} {summary}"),
            EventKind::ToolFinished { name, output, is_error: true, .. } => {
                let first = output.lines().next().unwrap_or_default();
                eprintln!("{indent}x {name}: {first}");
            }
            EventKind::AgentSpawned { description, agent_type, .. } => eprintln!("+ sub-agent ({agent_type}): {description}"),
            EventKind::Compacted { .. } => eprintln!("~ conversation summarised to free context"),
            EventKind::Retrying { attempt, message, .. } => eprintln!("~ retry {attempt}: {message}"),
            EventKind::Notice { text } => eprintln!("{indent}i {text}"),
            EventKind::Error { message } => eprintln!("{indent}error: {message}"),
            _ => {}
        }
    };

    let usage = session.meta().usage;
    if !args.json {
        eprintln!(
            "\n[{}] {} input / {} output tokens, session {}",
            match outcome {
                TurnOutcome::Completed => "done",
                TurnOutcome::Cancelled => "cancelled",
                TurnOutcome::Error => "failed",
                TurnOutcome::Refusal => "declined by the model",
                TurnOutcome::StepLimit => "step limit reached",
            },
            usage.input_tokens,
            usage.output_tokens,
            meta.id
        );
    }
    manager.shutdown();
    Ok(match outcome {
        TurnOutcome::Completed => ExitCode::SUCCESS,
        TurnOutcome::Cancelled => ExitCode::from(130),
        _ => ExitCode::FAILURE,
    })
}
