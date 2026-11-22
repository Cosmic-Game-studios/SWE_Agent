//! Built-in tools: the hands of the agent.
//!
//! A tool is a named, schema-described action. The agent loop owns the policy
//! (may this run? does it need approval?); tools only implement the action and
//! describe themselves well enough for that policy and for the UI.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;
use swe_protocol::{ToolDisplay, ToolKind, ToolSpec};
use tokio_util::sync::CancellationToken;

pub mod fs;
pub mod search;
pub mod shell;
pub mod text;
pub mod web;

pub use shell::ShellManager;

/// Receives live output while a tool runs (shell output, progress lines).
#[derive(Clone, Default)]
pub struct OutputSink(Option<Arc<dyn Fn(String) + Send + Sync>>);

impl OutputSink {
    pub fn new(callback: impl Fn(String) + Send + Sync + 'static) -> Self {
        OutputSink(Some(Arc::new(callback)))
    }

    pub fn none() -> Self {
        OutputSink(None)
    }

    pub fn send(&self, chunk: impl Into<String>) {
        if let Some(callback) = &self.0 {
            callback(chunk.into());
        }
    }
}

/// Per-call environment handed to a tool.
#[derive(Clone)]
pub struct ToolContext {
    /// Directory relative paths resolve against; the agent's workspace root
    /// (or its git worktree when the agent runs isolated).
    pub cwd: PathBuf,
    /// Fires when the user stops the turn; long-running tools must honour it.
    pub cancel: CancellationToken,
    pub output: OutputSink,
    /// Background processes of the session.
    pub shell: Arc<ShellManager>,
    /// Id of the tool call being executed; empty outside an agent loop.
    pub call_id: String,
}

impl ToolContext {
    /// A context for one-off use (tests, CLI helpers).
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        ToolContext {
            cwd: cwd.into(),
            cancel: CancellationToken::new(),
            output: OutputSink::none(),
            shell: Arc::new(ShellManager::new()),
            call_id: String::new(),
        }
    }

    /// Resolve a tool-supplied path against the working directory.
    pub fn resolve(&self, path: &str) -> PathBuf {
        resolve_path(&self.cwd, path)
    }
}

/// Join `path` onto `base` (unless absolute) and remove `.` / `..` lexically,
/// so boundary checks cannot be defeated with `a/../../b`.
pub fn resolve_path(base: &Path, path: &str) -> PathBuf {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Show `path` relative to `base` when it is inside it, absolute otherwise.
pub fn display_path(base: &Path, path: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    /// What the model sees.
    pub content: String,
    pub is_error: bool,
    /// Optional structured rendering for the UI.
    pub display: Option<ToolDisplay>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: false,
            display: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: true,
            display: None,
        }
    }

    pub fn with_display(mut self, display: ToolDisplay) -> Self {
        self.display = Some(display);
        self
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// Shown to the model. Say *when* to use the tool, not only what it does.
    fn description(&self) -> String;

    /// JSON Schema of the arguments object.
    fn input_schema(&self) -> Value;

    fn kind(&self) -> ToolKind;

    /// May run concurrently with other calls of the same assistant turn.
    fn parallel_safe(&self) -> bool {
        matches!(self.kind(), ToolKind::Read | ToolKind::Network | ToolKind::Agent)
    }

    /// One-line description of a concrete call for approvals and the timeline.
    fn summarize(&self, input: &Value) -> String;

    /// File-system paths the call reads or writes, as given by the model.
    /// The permission policy uses them for the workspace boundary check.
    fn paths(&self, _input: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description(),
            input_schema: self.input_schema(),
        }
    }
}

/// Deserialize tool arguments, turning a mismatch into a tool error the model
/// can act on. This is also the validation step that makes it safe to accept
/// arguments that were streamed without server-side schema checks.
pub fn parse_input<T: DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input).map_err(|err| ToolOutput::error(format!("Invalid arguments: {err}")))
}

/// Read a string argument for summaries without failing on bad input.
pub fn arg_str<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// An ordered set of tools with unique names.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// File, search, shell and web tools.
    pub fn builtin() -> Self {
        let mut registry = Self::new();
        registry.add(Arc::new(fs::ReadFile));
        registry.add(Arc::new(fs::WriteFile));
        registry.add(Arc::new(fs::EditFile));
        registry.add(Arc::new(fs::ListDir));
        registry.add(Arc::new(search::Glob));
        registry.add(Arc::new(search::Grep));
        registry.add(Arc::new(shell::Bash));
        registry.add(Arc::new(shell::BashOutput));
        registry.add(Arc::new(shell::KillProcess));
        registry.add(Arc::new(web::WebFetch::new()));
        registry
    }

    /// Add a tool, replacing any existing one with the same name.
    pub fn add(&mut self, tool: Arc<dyn Tool>) {
        self.tools.retain(|existing| existing.name() != tool.name());
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.name() == name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.name().to_string()).collect()
    }

    /// Keep only tools for which `keep` returns true.
    pub fn retain(&mut self, mut keep: impl FnMut(&dyn Tool) -> bool) {
        self.tools.retain(|tool| keep(tool.as_ref()));
    }

    /// Specs sorted by name: a deterministic order keeps provider prompt
    /// caches valid across requests and restarts.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.tools.iter().map(|tool| tool.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_normalises_without_touching_the_file_system() {
        let base = Path::new("/work/repo");
        assert_eq!(
            resolve_path(base, "src/./main.rs"),
            PathBuf::from("/work/repo/src/main.rs")
        );
        assert_eq!(resolve_path(base, "a/../../outside"), PathBuf::from("/work/outside"));
        assert_eq!(resolve_path(base, "/etc/hosts"), PathBuf::from("/etc/hosts"));
        assert_eq!(resolve_path(base, "."), PathBuf::from("/work/repo"));
    }

    #[test]
    fn display_path_is_relative_inside_the_base() {
        let base = Path::new("/work/repo");
        assert_eq!(display_path(base, Path::new("/work/repo/src/a.rs")), "src/a.rs");
        assert_eq!(display_path(base, Path::new("/work/repo")), ".");
        assert_eq!(display_path(base, Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn builtin_registry_is_complete_and_sorted() {
        let registry = ToolRegistry::builtin();
        let names: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            [
                "bash",
                "bash_output",
                "edit_file",
                "glob",
                "grep",
                "kill_process",
                "list_dir",
                "read_file",
                "web_fetch",
                "write_file"
            ]
        );
        for spec in registry.specs() {
            assert_eq!(spec.input_schema["type"], "object", "{}", spec.name);
            assert!(!spec.description.is_empty());
        }
    }
}
