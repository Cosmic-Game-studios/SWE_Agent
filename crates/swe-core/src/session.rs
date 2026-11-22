//! Sessions: durable conversations with an agent, and the manager that owns
//! them.
//!
//! A session lives in its own directory:
//!
//! ```text
//! meta.json      SessionMeta (title, model, status, usage, ...)
//! events.jsonl   every durable event, one JSON object per line
//! state.json     the model-facing conversation, todo list, granted rules
//! ```
//!
//! Turns run on background tasks owned by the manager, not by whoever is
//! watching: a client can disconnect, come back hours later, replay the
//! event log from the last sequence number it saw and continue live.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use swe_protocol::{
    ApprovalDecision, ContentBlock, Effort, Engine, Event, EventKind, ImageRef, Message, PermissionMode, ROOT_AGENT,
    Role, SessionMeta, SessionStatus, StreamingSnapshot, Todo, TurnOutcome, Usage,
};
use swe_tools::{ShellManager, Tool};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{self, AgentSpec, Env};
use crate::config::{Config, Paths, ProviderRegistry, write_atomic};
use crate::mcp::McpHub;
use crate::permissions::{PermissionPolicy, Rule};
use crate::prompt::{self, PromptContext};
use crate::subagent::{self, AgentIdentity};
use crate::{external, worktree};

const DEFAULT_TITLE: &str = "New session";

/// A message from the user.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserInput {
    pub text: String,
    #[serde(default)]
    pub images: Vec<ImageRef>,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        UserInput { text: text.into(), images: Vec::new() }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CreateSession {
    /// Directory the agent works in. Must exist.
    pub workspace: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub engine: Option<Engine>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub mode: Option<PermissionMode>,
    #[serde(default)]
    pub effort: Option<Effort>,
    /// Root agent profile: `coder` (default) or `orchestrator`.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct UpdateSession {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub mode: Option<PermissionMode>,
    #[serde(default)]
    pub effort: Option<Effort>,
}

/// Everything a client needs to render a session it just opened.
#[derive(Clone, Debug, Serialize)]
pub struct SessionSnapshot {
    pub meta: SessionMeta,
    pub events: Vec<Event>,
    /// Partial text of messages that are streaming right now.
    pub streaming: Vec<StreamingSnapshot>,
    pub todos: Vec<Todo>,
}

/// Changes to the set of sessions, for session lists and overviews.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GlobalEvent {
    SessionUpdated { session: SessionMeta },
    SessionDeleted { id: String },
}

pub(crate) enum ApprovalOutcome {
    Decided { decision: ApprovalDecision, feedback: Option<String> },
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session `{0}` not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// What is written to `state.json`.
#[derive(Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    system_prompt: String,
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    announced_mode: Option<PermissionMode>,
    #[serde(default)]
    todos: Vec<Todo>,
    #[serde(default)]
    rules: Vec<Rule>,
}

/// The model-facing conversation. Locked for the duration of a turn.
#[derive(Default)]
struct Conversation {
    /// Rendered once and then frozen; see `prompt`.
    system_prompt: String,
    messages: Vec<Message>,
    /// Permission mode the model was last told about.
    announced_mode: Option<PermissionMode>,
}

struct EventLog {
    file: Option<std::fs::File>,
    seq: u64,
    events: Vec<Event>,
}

struct ActiveRun {
    cancel: CancellationToken,
}

type Notify = Arc<dyn Fn(&SessionMeta) + Send + Sync>;

pub struct Session {
    pub id: String,
    dir: PathBuf,
    meta: Mutex<SessionMeta>,
    conversation: tokio::sync::Mutex<Conversation>,
    log: Mutex<EventLog>,
    events_tx: broadcast::Sender<Event>,
    streaming: Mutex<HashMap<String, StreamingSnapshot>>,
    policy: Mutex<PermissionPolicy>,
    approvals: Mutex<HashMap<String, oneshot::Sender<(ApprovalDecision, Option<String>)>>>,
    shell: Arc<ShellManager>,
    steering: Mutex<VecDeque<UserInput>>,
    run: Mutex<Option<ActiveRun>>,
    todos: Mutex<Vec<Todo>>,
    /// Tells the manager that the metadata changed.
    notify: Notify,
}

impl Session {
    fn new(dir: PathBuf, meta: SessionMeta, state: StateFile, events: Vec<Event>, notify: Notify) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(4096);
        let mut policy = PermissionPolicy::new(meta.mode, PathBuf::from(&meta.workspace));
        policy.rules = state.rules;
        let seq = events.last().map(|event| event.seq).unwrap_or(0).max(meta.last_seq);
        Arc::new(Session {
            id: meta.id.clone(),
            dir,
            meta: Mutex::new(meta),
            conversation: tokio::sync::Mutex::new(Conversation {
                system_prompt: state.system_prompt,
                messages: state.messages,
                announced_mode: state.announced_mode,
            }),
            log: Mutex::new(EventLog { file: None, seq, events }),
            events_tx,
            streaming: Mutex::new(HashMap::new()),
            policy: Mutex::new(policy),
            approvals: Mutex::new(HashMap::new()),
            shell: Arc::new(ShellManager::new()),
            steering: Mutex::new(VecDeque::new()),
            run: Mutex::new(None),
            todos: Mutex::new(state.todos),
            notify,
        })
    }

    pub fn meta(&self) -> SessionMeta {
        self.meta.lock().unwrap().clone()
    }

    pub fn workspace(&self) -> PathBuf {
        PathBuf::from(&self.meta.lock().unwrap().workspace)
    }

    pub fn todos(&self) -> Vec<Todo> {
        self.todos.lock().unwrap().clone()
    }

    pub(crate) fn set_todos(&self, agent_id: &str, todos: Vec<Todo>) {
        *self.todos.lock().unwrap() = todos.clone();
        self.emit(agent_id, EventKind::TodosUpdated { todos });
    }

    pub(crate) fn shell(&self) -> Arc<ShellManager> {
        self.shell.clone()
    }

    pub(crate) fn policy(&self) -> MutexGuard<'_, PermissionPolicy> {
        self.policy.lock().unwrap()
    }

    pub(crate) fn add_rule(&self, rule: Rule) {
        self.policy().add_rule(rule);
    }

    /// Treat `root` as part of the workspace (worktrees of sub-agents).
    pub(crate) fn add_root(&self, root: PathBuf) {
        let mut policy = self.policy();
        if !policy.roots.contains(&root) {
            policy.roots.push(root);
        }
    }

    pub(crate) fn has_steering(&self) -> bool {
        !self.steering.lock().unwrap().is_empty()
    }

    pub(crate) fn take_steering(&self) -> Vec<UserInput> {
        self.steering.lock().unwrap().drain(..).collect()
    }

    pub(crate) fn add_usage(&self, usage: &Usage) {
        self.meta.lock().unwrap().usage.add(usage);
    }

    /// Remember the vendor CLI's own conversation id, to resume it next turn.
    pub(crate) fn set_external_session_id(&self, id: String) {
        let changed = {
            let mut meta = self.meta.lock().unwrap();
            let changed = meta.external_session_id.as_deref() != Some(id.as_str());
            meta.external_session_id = Some(id);
            changed
        };
        if changed {
            self.save_meta();
        }
    }

    pub(crate) fn set_context_tokens(&self, tokens: u64) {
        self.meta.lock().unwrap().context_tokens = tokens;
    }

    /// Record an event: number it, persist it if durable, and broadcast it.
    pub fn emit(&self, agent_id: &str, kind: EventKind) -> Event {
        let transient = kind.is_transient();
        // Keep the catch-up snapshot for late subscribers current.
        {
            let mut streaming = self.streaming.lock().unwrap();
            match &kind {
                EventKind::AssistantDelta { text } => {
                    streaming.entry(agent_id.to_string()).or_insert_with(|| snapshot_for(agent_id)).text.push_str(text);
                }
                EventKind::ReasoningDelta { text } => {
                    streaming
                        .entry(agent_id.to_string())
                        .or_insert_with(|| snapshot_for(agent_id))
                        .reasoning
                        .push_str(text);
                }
                EventKind::AssistantMessage { .. }
                | EventKind::Retrying { .. }
                | EventKind::AgentFinished { .. }
                | EventKind::Error { .. } => {
                    streaming.remove(agent_id);
                }
                EventKind::TurnFinished { .. } => streaming.clear(),
                _ => {}
            }
        }
        let mut log = self.log.lock().unwrap();
        if !transient {
            log.seq += 1;
        }
        let event = Event { seq: log.seq, ts: Utc::now(), agent_id: agent_id.to_string(), kind };
        if !transient {
            if log.file.is_none() {
                log.file = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("events.jsonl")).ok();
            }
            if let (Some(file), Ok(mut line)) = (log.file.as_mut(), serde_json::to_string(&event)) {
                line.push('\n');
                if let Err(err) = file.write_all(line.as_bytes()) {
                    tracing::error!(session = %self.id, "cannot append to the event log: {err}");
                }
            }
            log.events.push(event.clone());
            let mut meta = self.meta.lock().unwrap();
            meta.last_seq = event.seq;
            meta.updated_at = event.ts;
        }
        // Broadcast while holding the log lock, so a subscriber that takes
        // its replay under the same lock sees every event exactly once.
        let _ = self.events_tx.send(event.clone());
        event
    }

    /// Durable events after `after`, plus a receiver for everything newer.
    pub fn subscribe(&self, after: u64) -> (Vec<Event>, broadcast::Receiver<Event>) {
        let log = self.log.lock().unwrap();
        let replay = log.events.iter().filter(|event| event.seq > after).cloned().collect();
        (replay, self.events_tx.subscribe())
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let log = self.log.lock().unwrap();
        SessionSnapshot {
            meta: self.meta(),
            events: log.events.clone(),
            streaming: self.streaming.lock().unwrap().values().cloned().collect(),
            todos: self.todos(),
        }
    }

    fn save_meta(&self) {
        let meta = self.meta();
        match serde_json::to_vec_pretty(&meta) {
            Ok(bytes) => {
                if let Err(err) = write_atomic(&self.dir.join("meta.json"), &bytes) {
                    tracing::error!(session = %self.id, "cannot write meta.json: {err}");
                }
            }
            Err(err) => tracing::error!(session = %self.id, "cannot serialise session metadata: {err}"),
        }
        (self.notify)(&meta);
    }

    fn save_state(&self, system_prompt: &str, messages: &[Message], announced_mode: Option<PermissionMode>) {
        // Borrowing serializer view: avoids cloning the whole history per step.
        #[derive(Serialize)]
        struct View<'a> {
            system_prompt: &'a str,
            messages: &'a [Message],
            announced_mode: Option<PermissionMode>,
            todos: Vec<Todo>,
            rules: Vec<Rule>,
        }
        let view = View {
            system_prompt,
            messages,
            announced_mode,
            todos: self.todos(),
            rules: self.policy().rules.clone(),
        };
        match serde_json::to_vec(&view) {
            Ok(bytes) => {
                if let Err(err) = write_atomic(&self.dir.join("state.json"), &bytes) {
                    tracing::error!(session = %self.id, "cannot write state.json: {err}");
                }
            }
            Err(err) => tracing::error!(session = %self.id, "cannot serialise session state: {err}"),
        }
        self.save_meta();
    }

    fn set_status(&self, status: SessionStatus, error: Option<String>) {
        {
            let mut meta = self.meta.lock().unwrap();
            if meta.status == status && meta.error == error {
                return;
            }
            meta.status = status;
            meta.error = error;
            meta.updated_at = Utc::now();
        }
        self.save_meta();
    }

    /// Ask the user whether a tool call may run, and wait for the answer.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn request_approval(
        &self,
        cancel: &CancellationToken,
        agent_id: &str,
        tool_call_id: &str,
        tool: &dyn Tool,
        input: &Value,
        reason: String,
        always: Option<Rule>,
    ) -> ApprovalOutcome {
        let request_id = short_id();
        let (tx, rx) = oneshot::channel();
        self.approvals.lock().unwrap().insert(request_id.clone(), tx);
        self.emit(
            agent_id,
            EventKind::ApprovalRequested {
                request_id: request_id.clone(),
                tool_call_id: tool_call_id.to_string(),
                name: tool.name().to_string(),
                kind: tool.kind(),
                input: input.clone(),
                summary: tool.summarize(input),
                reason,
                always_label: always.as_ref().map(Rule::label),
            },
        );
        self.set_status(SessionStatus::WaitingApproval, None);

        let outcome = tokio::select! {
            answer = rx => match answer {
                Ok((decision, feedback)) => ApprovalOutcome::Decided { decision, feedback },
                Err(_) => ApprovalOutcome::Cancelled,
            },
            _ = cancel.cancelled() => ApprovalOutcome::Cancelled,
        };
        if matches!(outcome, ApprovalOutcome::Cancelled) && self.approvals.lock().unwrap().remove(&request_id).is_some() {
            self.emit(agent_id, EventKind::ApprovalResolved { request_id, decision: ApprovalDecision::Deny });
        }
        // Other agents of the session may still be waiting for an answer.
        if self.approvals.lock().unwrap().is_empty() && self.run.lock().unwrap().is_some() {
            self.set_status(SessionStatus::Running, None);
        }
        outcome
    }

    /// Deliver the user's answer to a pending approval. False if it is gone.
    pub fn resolve_approval(&self, request_id: &str, decision: ApprovalDecision, feedback: Option<String>) -> bool {
        let Some(tx) = self.approvals.lock().unwrap().remove(request_id) else {
            return false;
        };
        // The resolved event is attributed to the agent that asked.
        let agent_id = {
            let log = self.log.lock().unwrap();
            log.events
                .iter()
                .rev()
                .find(|event| matches!(&event.kind, EventKind::ApprovalRequested { request_id: id, .. } if id == request_id))
                .map(|event| event.agent_id.clone())
                .unwrap_or_else(|| ROOT_AGENT.to_string())
        };
        self.emit(&agent_id, EventKind::ApprovalResolved { request_id: request_id.to_string(), decision });
        let _ = tx.send((decision, feedback));
        true
    }

    /// Stop the running turn, if any.
    pub fn cancel(&self) {
        if let Some(run) = self.run.lock().unwrap().as_ref() {
            run.cancel.cancel();
        }
    }

    pub fn is_running(&self) -> bool {
        self.run.lock().unwrap().is_some()
    }
}

fn snapshot_for(agent_id: &str) -> StreamingSnapshot {
    StreamingSnapshot { agent_id: agent_id.to_string(), ..Default::default() }
}

pub(crate) fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

pub(crate) struct ManagerInner {
    pub paths: Paths,
    config: RwLock<Config>,
    pub registry: ProviderRegistry,
    pub mcp: McpHub,
    index: Mutex<HashMap<String, SessionMeta>>,
    loaded: Mutex<HashMap<String, Arc<Session>>>,
    global: broadcast::Sender<GlobalEvent>,
}

impl ManagerInner {
    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }
}

/// Owns every session and the background tasks that run their turns.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<ManagerInner>,
}

impl SessionManager {
    /// Open the agent home at `paths`, loading config and the session index.
    pub fn open(paths: Paths) -> anyhow::Result<Self> {
        std::fs::create_dir_all(paths.sessions_dir())?;
        let config = Config::load(&paths)?;
        let registry = ProviderRegistry::new(&paths, &config);
        let (global, _) = broadcast::channel(1024);

        let mut index = HashMap::new();
        for entry in std::fs::read_dir(paths.sessions_dir())?.flatten() {
            let meta_file = entry.path().join("meta.json");
            let Ok(text) = std::fs::read_to_string(&meta_file) else { continue };
            let mut meta: SessionMeta = match serde_json::from_str(&text) {
                Ok(meta) => meta,
                Err(err) => {
                    tracing::warn!("skipping {}: {err}", meta_file.display());
                    continue;
                }
            };
            // Nothing survives a restart mid-turn; say so instead of showing
            // a session as running forever.
            if matches!(meta.status, SessionStatus::Running | SessionStatus::WaitingApproval) {
                meta.status = SessionStatus::Interrupted;
                if let Ok(bytes) = serde_json::to_vec_pretty(&meta) {
                    let _ = write_atomic(&meta_file, &bytes);
                }
            }
            index.insert(meta.id.clone(), meta);
        }

        let mcp = McpHub::new(&config.mcp_servers);
        Ok(SessionManager {
            inner: Arc::new(ManagerInner {
                paths,
                config: RwLock::new(config),
                registry,
                mcp,
                index: Mutex::new(index),
                loaded: Mutex::new(HashMap::new()),
                global,
            }),
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    pub fn config(&self) -> Config {
        self.inner.config()
    }

    pub fn registry(&self) -> &ProviderRegistry {
        &self.inner.registry
    }

    /// Replace the configuration and write it to `config.toml`.
    pub fn set_config(&self, config: Config) -> anyhow::Result<()> {
        config.save(&self.inner.paths)?;
        self.inner.registry.apply_config(&config);
        *self.inner.config.write().unwrap() = config;
        Ok(())
    }

    pub fn subscribe_global(&self) -> broadcast::Receiver<GlobalEvent> {
        self.inner.global.subscribe()
    }

    /// All sessions, most recently active first.
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut sessions: Vec<SessionMeta> = self.inner.index.lock().unwrap().values().cloned().collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        sessions
    }

    fn notifier(&self) -> Notify {
        let inner = Arc::downgrade(&self.inner);
        Arc::new(move |meta: &SessionMeta| {
            if let Some(inner) = inner.upgrade() {
                let mut index = inner.index.lock().unwrap();
                // A deleted session must not reappear through a late save.
                if index.contains_key(&meta.id) {
                    index.insert(meta.id.clone(), meta.clone());
                    let _ = inner.global.send(GlobalEvent::SessionUpdated { session: meta.clone() });
                }
            }
        })
    }

    pub fn create(&self, request: CreateSession) -> Result<SessionMeta, SessionError> {
        let workspace = PathBuf::from(&request.workspace);
        let workspace = workspace
            .canonicalize()
            .map_err(|_| SessionError::Invalid(format!("workspace `{}` does not exist", request.workspace)))?;
        if !workspace.is_dir() {
            return Err(SessionError::Invalid(format!("workspace `{}` is not a directory", workspace.display())));
        }
        let config = self.config();
        let engine = request.engine.unwrap_or_default();
        let (provider, model) = match engine {
            Engine::Native => {
                let provider = request
                    .provider
                    .or(config.default_provider.clone())
                    .ok_or_else(|| SessionError::Invalid("no provider selected".into()))?;
                let model = request
                    .model
                    .or(config.default_model.clone())
                    .filter(|model| !model.trim().is_empty())
                    .ok_or_else(|| SessionError::Invalid("no model selected".into()))?;
                (provider, model)
            }
            // The vendor CLI picks its own default model unless one is named.
            other => (other.as_str().to_string(), request.model.unwrap_or_default()),
        };
        let agent = request.agent.unwrap_or_else(|| "coder".to_string());
        if !prompt::ROOT_PROFILES.iter().any(|(name, _)| *name == agent) {
            return Err(SessionError::Invalid(format!("unknown agent profile `{agent}`")));
        }

        let now = Utc::now();
        let meta = SessionMeta {
            id: short_id(),
            title: request.title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| DEFAULT_TITLE.to_string()),
            workspace: workspace.to_string_lossy().into_owned(),
            engine,
            provider,
            model,
            mode: request.mode.unwrap_or(config.default_mode),
            effort: request.effort.unwrap_or(config.default_effort),
            agent,
            status: SessionStatus::Idle,
            created_at: now,
            updated_at: now,
            usage: Usage::default(),
            context_tokens: 0,
            context_window: None,
            last_seq: 0,
            error: None,
            external_session_id: None,
        };
        let dir = self.inner.paths.sessions_dir().join(&meta.id);
        std::fs::create_dir_all(&dir)?;
        self.inner.index.lock().unwrap().insert(meta.id.clone(), meta.clone());
        let session = Session::new(dir, meta.clone(), StateFile::default(), Vec::new(), self.notifier());
        session.save_meta();
        self.inner.loaded.lock().unwrap().insert(meta.id.clone(), session);
        Ok(meta)
    }

    /// The session with this id, loading it from disk on first use.
    pub fn get(&self, id: &str) -> Result<Arc<Session>, SessionError> {
        if let Some(session) = self.inner.loaded.lock().unwrap().get(id) {
            return Ok(session.clone());
        }
        let meta = self
            .inner
            .index
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        let dir = self.inner.paths.sessions_dir().join(id);
        let state: StateFile = std::fs::read_to_string(dir.join("state.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let events = std::fs::read_to_string(dir.join("events.jsonl"))
            .map(|text| text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect())
            .unwrap_or_default();
        let session = Session::new(dir, meta, state, events, self.notifier());
        // Another caller may have loaded it in the meantime; keep one instance.
        let mut loaded = self.inner.loaded.lock().unwrap();
        Ok(loaded.entry(id.to_string()).or_insert(session).clone())
    }

    pub fn update(&self, id: &str, update: UpdateSession) -> Result<SessionMeta, SessionError> {
        let session = self.get(id)?;
        {
            let mut meta = session.meta.lock().unwrap();
            if let Some(title) = update.title.filter(|t| !t.trim().is_empty()) {
                meta.title = title.trim().to_string();
            }
            if meta.engine == Engine::Native {
                if let Some(provider) = update.provider {
                    if provider != meta.provider {
                        meta.context_window = None;
                    }
                    meta.provider = provider;
                }
            }
            if let Some(model) = update.model {
                if model != meta.model {
                    meta.context_window = None;
                }
                meta.model = model;
            }
            if let Some(effort) = update.effort {
                meta.effort = effort;
            }
            if let Some(mode) = update.mode {
                meta.mode = mode;
                // Takes effect immediately, also for a turn in progress.
                session.policy().mode = mode;
            }
            meta.updated_at = Utc::now();
        }
        session.save_meta();
        Ok(session.meta())
    }

    pub fn delete(&self, id: &str) -> Result<(), SessionError> {
        let session = self.get(id)?;
        session.cancel();
        session.shell.kill_all();
        self.inner.index.lock().unwrap().remove(id);
        self.inner.loaded.lock().unwrap().remove(id);
        let dir = self.inner.paths.sessions_dir().join(id);
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        let worktrees = self.inner.paths.worktrees_dir().join(id);
        if worktrees.exists() {
            let workspace = session.workspace();
            tokio::spawn(async move { worktree::remove_all(&workspace, &worktrees).await });
        }
        let _ = self.inner.global.send(GlobalEvent::SessionDeleted { id: id.to_string() });
        Ok(())
    }

    /// Send a user message. Starts a turn, or, when one is running, queues
    /// the message so the agent sees it at its next step. Returns whether
    /// the message was queued.
    pub fn send_message(&self, id: &str, input: UserInput) -> Result<bool, SessionError> {
        if input.text.trim().is_empty() && input.images.is_empty() {
            return Err(SessionError::Invalid("the message is empty".into()));
        }
        let session = self.get(id)?;
        let mut run = session.run.lock().unwrap();
        if run.is_some() {
            session.steering.lock().unwrap().push_back(input);
            return Ok(true);
        }
        let cancel = CancellationToken::new();
        *run = Some(ActiveRun { cancel: cancel.clone() });
        drop(run);
        tokio::spawn(drive(self.inner.clone(), session, input, cancel));
        Ok(false)
    }

    pub fn cancel(&self, id: &str) -> Result<(), SessionError> {
        self.get(id)?.cancel();
        Ok(())
    }

    /// State of the configured MCP servers (starts them if needed).
    pub async fn mcp_status(&self) -> Vec<crate::mcp::McpServerStatus> {
        self.inner.mcp.status().await
    }

    /// Stop every background process of every session (on shutdown).
    pub fn shutdown(&self) {
        for session in self.inner.loaded.lock().unwrap().values() {
            session.cancel();
            session.shell.kill_all();
        }
        self.inner.mcp.shutdown();
    }
}

/// Run turns for a session until its message queue is empty.
async fn drive(inner: Arc<ManagerInner>, session: Arc<Session>, first: UserInput, cancel: CancellationToken) {
    let mut input = first;
    loop {
        run_turn(&inner, &session, input, &cancel).await;
        // Decide under the run lock, so a message sent right now either
        // lands in the queue we are about to read or starts a new run.
        let mut run = session.run.lock().unwrap();
        let next = if cancel.is_cancelled() {
            session.steering.lock().unwrap().clear();
            None
        } else {
            session.steering.lock().unwrap().pop_front()
        };
        match next {
            Some(next) => input = next,
            None => {
                *run = None;
                break;
            }
        }
    }
}

fn usage_since(now: &Usage, before: &Usage) -> Usage {
    Usage {
        input_tokens: now.input_tokens.saturating_sub(before.input_tokens),
        output_tokens: now.output_tokens.saturating_sub(before.output_tokens),
        cache_read_tokens: now.cache_read_tokens.saturating_sub(before.cache_read_tokens),
        cache_write_tokens: now.cache_write_tokens.saturating_sub(before.cache_write_tokens),
        reasoning_tokens: now.reasoning_tokens.saturating_sub(before.reasoning_tokens),
    }
}

/// A turn that was cut short by a crash leaves tool calls without results,
/// which no provider accepts. Answer them so the conversation can go on.
fn repair_history(messages: &mut Vec<Message>) {
    let Some(last) = messages.last() else { return };
    if last.role != Role::Assistant {
        return;
    }
    let results: Vec<ContentBlock> = last
        .tool_calls()
        .map(|call| {
            ContentBlock::tool_result(
                call.id,
                "Interrupted: the agent was stopped before this tool call finished. Whether it had any effect is unknown; check before relying on it.",
                true,
            )
        })
        .collect();
    if !results.is_empty() {
        messages.push(Message { role: Role::User, content: results });
    }
}

fn title_from(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty() && !line.starts_with('!')).unwrap_or("Session");
    let mut title: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        title.push('…');
    }
    title
}

async fn run_turn(inner: &Arc<ManagerInner>, session: &Arc<Session>, input: UserInput, cancel: &CancellationToken) {
    let mut conversation = session.conversation.lock().await;
    {
        let mut meta = session.meta.lock().unwrap();
        if meta.title == DEFAULT_TITLE && !input.text.trim().is_empty() {
            meta.title = title_from(&input.text);
        }
    }
    let meta = session.meta();
    let usage_before = meta.usage;
    session.set_status(SessionStatus::Running, None);
    session.emit(ROOT_AGENT, EventKind::UserMessage { text: input.text.clone(), images: input.images.clone() });
    session.emit(ROOT_AGENT, EventKind::TurnStarted { provider: meta.provider.clone(), model: meta.model.clone() });

    let env = Env { manager: inner.clone(), session: session.clone(), cancel: cancel.clone() };
    let (outcome, error) = if meta.engine == Engine::Native {
        run_native_turn(&env, &meta, &mut conversation, input).await
    } else {
        external::run_turn(&env, &meta, input).await
    };

    // Whatever was still waiting for an answer is moot now.
    let pending: Vec<String> = session.approvals.lock().unwrap().drain().map(|(id, _)| id).collect();
    for request_id in pending {
        session.emit(ROOT_AGENT, EventKind::ApprovalResolved { request_id, decision: ApprovalDecision::Deny });
    }
    session.emit(
        ROOT_AGENT,
        EventKind::TurnFinished { outcome, usage: usage_since(&session.meta().usage, &usage_before) },
    );
    match outcome {
        TurnOutcome::Error => session.set_status(SessionStatus::Error, error),
        _ => session.set_status(SessionStatus::Idle, None),
    }
    session.save_state(&conversation.system_prompt, &conversation.messages, conversation.announced_mode);
}

async fn run_native_turn(
    env: &Env,
    meta: &SessionMeta,
    conversation: &mut Conversation,
    input: UserInput,
) -> (TurnOutcome, Option<String>) {
    let session = &env.session;
    let fail = |message: String| {
        session.emit(ROOT_AGENT, EventKind::Error { message: message.clone() });
        (TurnOutcome::Error, Some(message))
    };
    let provider = match env.manager.registry.get(&meta.provider) {
        Ok(provider) => provider,
        Err(err) => return fail(err.to_string()),
    };
    let workspace = PathBuf::from(&meta.workspace);
    if !workspace.is_dir() {
        return fail(format!("The workspace {} no longer exists.", workspace.display()));
    }
    if conversation.system_prompt.is_empty() {
        let instructions = prompt::load_project_instructions(&workspace);
        conversation.system_prompt = prompt::system_prompt(
            &meta.agent,
            false,
            &PromptContext {
                workspace: &workspace,
                is_git_repo: worktree::is_git_repo(&workspace).await,
                date: &meta.created_at.format("%Y-%m-%d").to_string(),
                project_instructions: instructions.as_deref(),
            },
        );
    }

    let config = env.manager.config();
    let context_window = context_window(env, &meta.provider, &meta.model).await;
    if context_window.is_some() && meta.context_window != context_window {
        session.meta.lock().unwrap().context_window = context_window;
    }
    let identity = AgentIdentity {
        agent_id: ROOT_AGENT.to_string(),
        provider_id: meta.provider.clone(),
        model: meta.model.clone(),
        effort: meta.effort,
        depth: 0,
        cwd: workspace.clone(),
    };
    let spec = AgentSpec {
        agent_id: ROOT_AGENT.to_string(),
        provider,
        model: meta.model.clone(),
        effort: meta.effort,
        system: conversation.system_prompt.clone(),
        tools: subagent::root_tools(env, &identity, &config).await,
        cwd: workspace,
        context_limit: context_limit(&config, context_window),
        max_steps: config.max_steps_per_turn,
    };

    repair_history(&mut conversation.messages);
    let mut blocks = Vec::new();
    if conversation.announced_mode != Some(meta.mode) {
        // The mode can change between turns; the system prompt cannot.
        blocks.push(ContentBlock::text(format!("<system-notice>\n{}\n</system-notice>", prompt::mode_notice(meta.mode))));
        conversation.announced_mode = Some(meta.mode);
    }
    blocks.extend(input.into_blocks());
    match conversation.messages.last_mut() {
        Some(last) if last.role == Role::User => last.content.extend(blocks),
        _ => conversation.messages.push(Message { role: Role::User, content: blocks }),
    }

    let mut messages = std::mem::take(&mut conversation.messages);
    let system_prompt = conversation.system_prompt.clone();
    let announced_mode = conversation.announced_mode;
    let checkpoint_session = session.clone();
    let checkpoint = move |history: &[Message]| checkpoint_session.save_state(&system_prompt, history, announced_mode);
    let result = agent::run_agent(env, &spec, &mut messages, Some(&checkpoint)).await;
    conversation.messages = messages;
    (result.outcome, result.error)
}

/// The model's context window, from the provider's model listing. The
/// listing is fetched (briefly) the first time a provider is used.
pub(crate) async fn context_window(env: &Env, provider: &str, model: &str) -> Option<u64> {
    let registry = &env.manager.registry;
    if let Some(info) = registry.cached_model(provider, model) {
        return info.context_window;
    }
    let listing = tokio::time::timeout(std::time::Duration::from_secs(4), registry.models(provider, false)).await;
    if !matches!(listing, Ok(Ok(_))) {
        return None;
    }
    registry.cached_model(provider, model).and_then(|info| info.context_window)
}

/// Compaction threshold: the configured budget, but never more than 80% of
/// what the model can actually hold.
pub(crate) fn context_limit(config: &Config, context_window: Option<u64>) -> u64 {
    let configured = config.compact_threshold_tokens.max(8_000);
    match context_window {
        Some(window) if window > 0 => configured.min(window / 10 * 8),
        _ => configured,
    }
}

/// Whether `path` exists and is a directory, for workspace pickers.
pub fn is_workspace(path: &Path) -> bool {
    path.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repair_answers_dangling_tool_calls_once() {
        let mut messages = vec![
            Message::user_text("go"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: json!({}),
                    invalid_json: None,
                    meta: None,
                }],
            },
        ];
        repair_history(&mut messages);
        assert_eq!(messages.len(), 3);
        assert!(matches!(&messages[2].content[0], ContentBlock::ToolResult { tool_call_id, is_error: true, .. } if tool_call_id == "c1"));
        repair_history(&mut messages);
        assert_eq!(messages.len(), 3, "a complete history is left alone");
    }

    #[test]
    fn context_limit_respects_small_windows() {
        let config = Config::default();
        assert_eq!(context_limit(&config, None), 200_000);
        assert_eq!(context_limit(&config, Some(1_000_000)), 200_000);
        assert_eq!(context_limit(&config, Some(128_000)), 102_400);
        assert_eq!(context_limit(&config, Some(32_000)), 25_600);
    }

    #[test]
    fn titles_come_from_the_first_meaningful_line() {
        assert_eq!(title_from("\n  Fix the login bug \nmore"), "Fix the login bug");
        assert_eq!(title_from("!tool x {}\nReal request"), "Real request");
        assert_eq!(title_from(&"x".repeat(80)).chars().count(), 61);
    }
}
