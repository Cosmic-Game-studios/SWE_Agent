//! The project check that runs after a step that edited files, so compiler
//! and type errors reach the model with the edit that caused them instead of
//! many steps later.
//!
//! The check is an ordinary shell command and is subject to the session's
//! permission policy like any other: it only runs where the agent could have
//! run it without asking (full-auto, or an "always allow" rule that covers it).

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use swe_tools::text::truncate_middle;
use swe_tools::{OutputSink, ToolContext};

use crate::agent::{AgentSpec, Env};
use crate::config::Config;
use crate::permissions::Verdict;

/// Longer checks are not worth waiting for after every step.
const CHECK_TIMEOUT: Duration = Duration::from_secs(90);
/// How much of a failing check's output the model gets.
const MAX_CHECK_OUTPUT: usize = 6_000;

/// The command to check the project in `cwd` with: the configured one, or one
/// chosen from the files in the directory.
pub(crate) fn check_command(config: &Config, cwd: &Path) -> Option<String> {
    if let Some(command) = config.check_command.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        return Some(command.to_string());
    }
    if !config.auto_check {
        return None;
    }
    let has = |name: &str| cwd.join(name).is_file();
    let command = if has("Cargo.toml") {
        "cargo check --quiet --message-format=short"
    } else if has("tsconfig.json") && cwd.join("node_modules/typescript").is_dir() {
        // Only with a local compiler: npx must not go and download one.
        "npx --no-install tsc --noEmit"
    } else if has("go.mod") {
        "go vet ./..."
    } else {
        return None;
    };
    Some(command.to_string())
}

pub(crate) enum CheckOutcome {
    Passed,
    Failed(String),
    /// Too slow to run after every step; the caller stops asking for it.
    TimedOut,
}

/// Run the agent's check command if the permission policy lets it run unasked.
pub(crate) async fn run_check(env: &Env, spec: &AgentSpec) -> Option<CheckOutcome> {
    let command = spec.check_command.as_deref()?;
    let bash = spec.tools.get("bash")?;
    let input = json!({"command": command, "timeout_ms": CHECK_TIMEOUT.as_millis() as u64});
    if env.session.policy().check(bash.as_ref(), &input, &spec.cwd) != Verdict::Allow {
        return None;
    }
    let ctx = ToolContext {
        cwd: spec.cwd.clone(),
        cancel: env.cancel.clone(),
        output: OutputSink::none(),
        shell: env.session.shell(),
        call_id: String::new(),
    };
    let output = bash.run(input, &ctx).await;
    if env.cancel.is_cancelled() {
        return None;
    }
    Some(if !output.is_error {
        CheckOutcome::Passed
    } else if output.content.contains("timed out after") {
        CheckOutcome::TimedOut
    } else {
        CheckOutcome::Failed(truncate_middle(&output.content, MAX_CHECK_OUTPUT))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_check_is_chosen_from_the_project_unless_configured() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        assert_eq!(check_command(&config, dir.path()), None);

        std::fs::write(dir.path().join("go.mod"), "module x").unwrap();
        assert_eq!(check_command(&config, dir.path()).as_deref(), Some("go vet ./..."));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        assert!(check_command(&config, dir.path()).unwrap().starts_with("cargo check"));

        config.auto_check = false;
        assert_eq!(check_command(&config, dir.path()), None);
        config.check_command = Some(" make lint ".into());
        assert_eq!(check_command(&config, dir.path()).as_deref(), Some("make lint"));
    }
}
