//! Sub-agents and the tools that only exist inside a session: `task`
//! (delegate to a sub-agent) and `todo_write` (the visible plan).
//!
//! A sub-agent is the same agent loop with a fresh context, its own system
//! prompt and a tool set that fits its type. Calls issued together run in
//! parallel, which is what makes orchestration worthwhile: the lead agent
//! keeps its context for decisions while workers do the reading and typing.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use swe_protocol::{Effort, EventKind, Message, Todo, ToolKind, TurnOutcome};
use swe_tools::{Tool, ToolContext, ToolOutput, ToolRegistry, arg_str, parse_input};

use crate::agent::{self, AgentSpec, Env};
use crate::config::Config;
use crate::prompt::{self, PromptContext, SUBAGENT_TYPES};
use crate::session::{self, Session};
use crate::worktree;

/// Who an agent is; what a `task` tool needs to know about its owner.
#[derive(Clone)]
pub(crate) struct AgentIdentity {
    pub agent_id: String,
    pub provider_id: String,
    pub model: String,
    pub effort: Effort,
    /// 0 for the root agent.
    pub depth: u8,
    pub cwd: PathBuf,
}

/// Tools of the root agent: everything.
pub(crate) async fn root_tools(env: &Env, me: &AgentIdentity, config: &Config) -> ToolRegistry {
    let mut tools = ToolRegistry::builtin();
    tools.add(Arc::new(TodoWrite {
        session: env.session.clone(),
        agent_id: me.agent_id.clone(),
    }));
    for tool in env.manager.mcp.tools().await {
        tools.add(tool);
    }
    if me.depth < config.max_agent_depth {
        tools.add(Arc::new(TaskTool {
            env: env.clone(),
            parent: me.clone(),
        }));
    }
    tools
}

/// Tools of a sub-agent of the given type.
async fn subagent_tools(env: &Env, agent_type: &str, me: &AgentIdentity, config: &Config) -> ToolRegistry {
    let mut tools = ToolRegistry::builtin();
    match agent_type {
        "explore" => {
            tools.retain(|tool| matches!(tool.name(), "read_file" | "list_dir" | "glob" | "grep" | "web_fetch"))
        }
        "review" => tools.retain(|tool| !matches!(tool.kind(), ToolKind::Edit)),
        _ => {
            for tool in env.manager.mcp.tools().await {
                tools.add(tool);
            }
            if me.depth < config.max_agent_depth {
                tools.add(Arc::new(TaskTool {
                    env: env.clone(),
                    parent: me.clone(),
                }));
            }
        }
    }
    tools
}

pub(crate) struct TaskTool {
    env: Env,
    parent: AgentIdentity,
}

#[derive(Deserialize)]
struct TaskArgs {
    description: String,
    prompt: String,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    isolation: Option<String>,
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> String {
        let types: Vec<String> = SUBAGENT_TYPES
            .iter()
            .map(|(name, about)| format!("`{name}`: {about}"))
            .collect();
        format!(
            "Start a sub-agent that works on a task on its own and returns a report. The sub-agent starts \
             with an empty context: it sees only `prompt`, so put everything it needs there (goal, relevant \
             paths, constraints, what to report back). Several task calls issued in the same step run in \
             parallel. Use it for work that is separable from what you are doing: broad codebase searches, \
             independent reviews, or self-contained pieces of implementation. Do not use it for something \
             you can do in a few tool calls.\n\nAgent types: {}\n\nSet `isolation` to `worktree` for a \
             `general` agent that edits files while other agents do too: it then works in its own git \
             worktree on a new branch created from the last commit (uncommitted changes in the workspace \
             are not visible to it) and returns the branch name for you to merge.",
            types.join(" ")
        )
    }

    fn input_schema(&self) -> Value {
        let types: Vec<&str> = SUBAGENT_TYPES.iter().map(|(name, _)| *name).collect();
        json!({
            "type": "object",
            "properties": {
                "description": {"type": "string", "description": "Three to six words naming the task, shown to the user."},
                "prompt": {"type": "string", "description": "The complete task for the sub-agent."},
                "agent_type": {"type": "string", "enum": types, "description": "Defaults to `general`."},
                "isolation": {"type": "string", "enum": ["none", "worktree"], "description": "Defaults to `none`."}
            },
            "required": ["description", "prompt"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Agent
    }

    fn summarize(&self, input: &Value) -> String {
        arg_str(input, "description").to_string()
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: TaskArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        let agent_type = args.agent_type.as_deref().unwrap_or("general");
        if !SUBAGENT_TYPES.iter().any(|(name, _)| *name == agent_type) {
            let known: Vec<&str> = SUBAGENT_TYPES.iter().map(|(name, _)| *name).collect();
            return ToolOutput::error(format!(
                "Unknown agent_type `{agent_type}`. Use one of: {}.",
                known.join(", ")
            ));
        }
        if args.prompt.trim().is_empty() {
            return ToolOutput::error("prompt is empty.");
        }
        let env = &self.env;
        let session = &env.session;
        let config = env.manager.config();

        let provider_id = config
            .subagent_provider
            .clone()
            .unwrap_or_else(|| self.parent.provider_id.clone());
        let model = config
            .subagent_model
            .clone()
            .unwrap_or_else(|| self.parent.model.clone());
        let provider = match env.manager.registry.get(&provider_id) {
            Ok(provider) => provider,
            Err(err) => return ToolOutput::error(format!("Cannot start the sub-agent: {err}")),
        };

        let agent_id = format!("a{}", &session::short_id()[..8]);
        let mut cwd = self.parent.cwd.clone();
        let mut isolated = None;
        if args.isolation.as_deref() == Some("worktree") {
            if agent_type != "general" {
                return ToolOutput::error("Only `general` agents change files; isolation does not apply to this type.");
            }
            let base = env.manager.paths.worktrees_dir().join(&session.id);
            match worktree::create(&self.parent.cwd, &base, &agent_id).await {
                Ok(tree) => {
                    session.add_root(tree.path.clone());
                    cwd = tree.path.clone();
                    isolated = Some(tree);
                }
                Err(err) => return ToolOutput::error(format!("Cannot create a worktree: {err}")),
            }
        }

        let me = AgentIdentity {
            agent_id: agent_id.clone(),
            provider_id: provider_id.clone(),
            model: model.clone(),
            effort: self.parent.effort,
            depth: self.parent.depth + 1,
            cwd: cwd.clone(),
        };
        let instructions = prompt::load_project_instructions(&cwd);
        let system = prompt::system_prompt(
            agent_type,
            true,
            &PromptContext {
                workspace: &cwd,
                is_git_repo: worktree::is_git_repo(&cwd).await,
                date: &session.meta().created_at.format("%Y-%m-%d").to_string(),
                project_instructions: instructions.as_deref(),
            },
        );
        let context_window = session::context_window(env, &provider_id, &model).await;
        let spec = AgentSpec {
            agent_id: agent_id.clone(),
            provider,
            model: model.clone(),
            effort: me.effort,
            system,
            tools: subagent_tools(env, agent_type, &me, &config).await,
            cwd,
            context_limit: session::context_limit(&config, context_window),
            max_steps: config.max_steps_per_turn,
        };

        session.emit(
            &agent_id,
            EventKind::AgentSpawned {
                parent_agent_id: self.parent.agent_id.clone(),
                tool_call_id: ctx.call_id.clone(),
                description: args.description.clone(),
                agent_type: agent_type.to_string(),
                provider: provider_id,
                model,
                worktree: isolated.as_ref().map(|tree| tree.path.to_string_lossy().into_owned()),
            },
        );
        let mut history = vec![Message::user_text(args.prompt)];
        let result = agent::run_agent(env, &spec, &mut history, None).await;
        session.emit(
            &agent_id,
            EventKind::AgentFinished {
                outcome: result.outcome,
                usage: result.usage,
            },
        );

        let mut report = result.final_text.trim().to_string();
        if report.is_empty() {
            report = "(The sub-agent finished without writing a report.)".to_string();
        }
        if let Some(tree) = isolated {
            report.push_str("\n\n");
            report.push_str(&worktree::finish(tree, &args.description).await);
        }
        match result.outcome {
            TurnOutcome::Completed => ToolOutput::ok(report),
            TurnOutcome::Cancelled => ToolOutput::error(format!("[stopped by the user]\n\n{report}")),
            TurnOutcome::Error => ToolOutput::error(format!(
                "The sub-agent failed: {}\n\nIts last output:\n{report}",
                result.error.unwrap_or_else(|| "unknown error".into())
            )),
            TurnOutcome::Refusal => ToolOutput::error(format!(
                "The model declined to continue the sub-agent's task.\n\n{report}"
            )),
            TurnOutcome::StepLimit => ToolOutput::error(format!(
                "The sub-agent hit its step limit before finishing. Its last output:\n{report}"
            )),
        }
    }
}

pub(crate) struct TodoWrite {
    session: Arc<Session>,
    agent_id: String,
}

#[derive(Deserialize)]
struct TodoArgs {
    todos: Vec<Todo>,
}

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> String {
        "Create or update the todo list for this session. The list is shown to the user as your plan and \
         progress, and it survives context compaction. Use it for any task with three or more steps: \
         write the plan first, then update it as you go. Each call replaces the whole list, so always \
         send every item. Keep exactly one item `in_progress` and mark items `completed` as soon as they \
         are done, not in batches."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete list, in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "What needs to be done, in the imperative."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Meta
    }

    fn summarize(&self, input: &Value) -> String {
        let count = input.get("todos").and_then(Value::as_array).map_or(0, Vec::len);
        format!("{count} item{}", if count == 1 { "" } else { "s" })
    }

    async fn run(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
        let args: TodoArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        let total = args.todos.len();
        let done = args
            .todos
            .iter()
            .filter(|todo| todo.status == swe_protocol::TodoStatus::Completed)
            .count();
        self.session.set_todos(&self.agent_id, args.todos);
        ToolOutput::ok(format!("Todo list updated ({done}/{total} completed)."))
    }
}
