//! Git worktrees for sub-agents that edit files in parallel.
//!
//! Each isolated agent gets its own checkout on its own branch, created from
//! the workspace's current commit. When the agent is done its work is
//! committed, the checkout is removed and the branch stays behind for the
//! orchestrating agent to merge.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

pub(crate) struct Worktree {
    /// The repository the worktree belongs to.
    repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// Commit the branch started from.
    base: String,
}

/// Run git and return its trimmed stdout, or its stderr as the error.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|err| format!("cannot run git: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() { format!("git {} failed", args.join(" ")) } else { stderr })
    }
}

pub(crate) async fn is_git_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"]).await.is_ok_and(|out| out == "true")
}

/// Create `<base>/<agent_id>` as a worktree of the repository containing
/// `workspace`, on a new branch `swe/<agent_id>`.
pub(crate) async fn create(workspace: &Path, base: &Path, agent_id: &str) -> Result<Worktree, String> {
    if !is_git_repo(workspace).await {
        return Err("the workspace is not a git repository".to_string());
    }
    let head = git(workspace, &["rev-parse", "HEAD"])
        .await
        .map_err(|_| "the repository has no commits yet; commit first".to_string())?;
    std::fs::create_dir_all(base).map_err(|err| format!("cannot create {}: {err}", base.display()))?;
    let path = base.join(agent_id);
    let branch = format!("swe/{agent_id}");
    let path_text = path.to_string_lossy().into_owned();
    git(workspace, &["worktree", "add", "-b", &branch, &path_text, "HEAD"]).await?;
    let repo = git(workspace, &["rev-parse", "--show-toplevel"]).await.map(PathBuf::from).unwrap_or_else(|_| workspace.to_path_buf());
    Ok(Worktree { repo, path, branch, base: head })
}

/// Commit whatever the agent left uncommitted, remove the checkout and say
/// where the work is. The text is appended to the agent's report.
pub(crate) async fn finish(tree: Worktree, description: &str) -> String {
    let mut notes = Vec::new();
    if git(&tree.path, &["add", "-A"]).await.is_ok() && git(&tree.path, &["diff", "--cached", "--quiet"]).await.is_err() {
        let message = format!("{description}\n\nCommitted automatically from a sub-agent worktree.");
        // Fall back to a neutral identity only when the user has none configured.
        let has_identity = git(&tree.path, &["config", "user.email"]).await.is_ok_and(|email| !email.is_empty());
        let result = if has_identity {
            git(&tree.path, &["commit", "-q", "-m", &message]).await
        } else {
            git(
                &tree.path,
                &["-c", "user.name=SWE Agent", "-c", "user.email=swe-agent@localhost", "commit", "-q", "-m", &message],
            )
            .await
        };
        if let Err(err) = result {
            notes.push(format!("Committing the remaining changes failed: {err}"));
        }
    }
    let range = format!("{}..HEAD", tree.base);
    let ahead: u32 = git(&tree.path, &["rev-list", "--count", &range]).await.ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    let path_text = tree.path.to_string_lossy().into_owned();
    if let Err(err) = git(&tree.repo, &["worktree", "remove", "--force", &path_text]).await {
        notes.push(format!("Removing the worktree at {path_text} failed: {err}"));
    }
    if ahead == 0 {
        let _ = git(&tree.repo, &["branch", "-D", &tree.branch]).await;
        notes.insert(0, "Worktree: the agent made no changes; its branch was deleted.".to_string());
    } else {
        notes.insert(
            0,
            format!(
                "Worktree: the agent's changes are on branch `{}` ({ahead} commit{} on top of {}). Integrate \
                 them with `git merge {}`.",
                tree.branch,
                if ahead == 1 { "" } else { "s" },
                &tree.base[..tree.base.len().min(8)],
                tree.branch
            ),
        );
    }
    notes.join("\n")
}

/// Remove every worktree below `base` (when a session is deleted).
pub(crate) async fn remove_all(workspace: &Path, base: &Path) {
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path().to_string_lossy().into_owned();
            let _ = git(workspace, &["worktree", "remove", "--force", &path]).await;
        }
    }
    let _ = std::fs::remove_dir_all(base);
    let _ = git(workspace, &["worktree", "prune"]).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo(dir: &Path) {
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            git(dir, &args).await.unwrap();
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "-A"]).await.unwrap();
        git(dir, &["commit", "-q", "-m", "init"]).await.unwrap();
    }

    #[tokio::test]
    async fn worktree_work_ends_up_on_a_mergeable_branch() {
        let repo = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        let tree = create(repo.path(), base.path(), "a1").await.unwrap();
        assert!(tree.path.join("a.txt").exists());
        assert_eq!(tree.branch, "swe/a1");
        std::fs::write(tree.path.join("b.txt"), "new\n").unwrap();
        let path = tree.path.clone();
        let note = finish(tree, "Add b").await;
        assert!(note.contains("branch `swe/a1` (1 commit"), "{note}");
        assert!(!path.exists(), "the checkout is removed");
        assert!(!repo.path().join("b.txt").exists(), "the workspace is untouched until merged");

        git(repo.path(), &["merge", "-q", "swe/a1"]).await.unwrap();
        assert_eq!(std::fs::read_to_string(repo.path().join("b.txt")).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn untouched_worktrees_leave_nothing_behind() {
        let repo = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let tree = create(repo.path(), base.path(), "a2").await.unwrap();
        let note = finish(tree, "Nothing").await;
        assert!(note.contains("made no changes"));
        assert_eq!(git(repo.path(), &["branch", "--list", "swe/*"]).await.unwrap(), "");
    }

    #[tokio::test]
    async fn refuses_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        // Guard against the temp dir sitting inside some enclosing repository.
        if is_git_repo(dir.path()).await {
            return;
        }
        assert!(create(dir.path(), base.path(), "a3").await.is_err_and(|err| err.contains("not a git repository")));
    }
}
