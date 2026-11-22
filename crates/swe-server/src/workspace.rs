//! Workspace helpers for the UI: browsing directories to pick a workspace,
//! and the pending changes of a workspace as a unified diff.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::ApiError;

const MAX_ENTRIES: usize = 500;
const MAX_DIFF_BYTES: usize = 2_000_000;
const MAX_UNTRACKED_FILES: usize = 200;
const MAX_UNTRACKED_FILE_BYTES: u64 = 200_000;

#[derive(Deserialize)]
pub struct FsQuery {
    path: Option<String>,
    #[serde(default)]
    hidden: bool,
}

#[derive(Serialize)]
pub struct DirEntry {
    name: String,
    path: String,
    is_git: bool,
}

#[derive(Serialize)]
pub struct DirListing {
    path: String,
    parent: Option<String>,
    is_git: bool,
    entries: Vec<DirEntry>,
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Sub-directories of `path` (default: the home directory).
pub async fn list_directories(Query(query): Query<FsQuery>) -> Result<Json<DirListing>, ApiError> {
    let requested = match query.path.as_deref().map(str::trim).filter(|path| !path.is_empty()) {
        Some("~") => home_dir(),
        Some(path) if path.starts_with("~/") => home_dir().join(&path[2..]),
        Some(path) => PathBuf::from(path),
        None => home_dir(),
    };
    let path = requested
        .canonicalize()
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, format!("{} does not exist", requested.display())))?;
    if !path.is_dir() {
        return Err(ApiError::bad_request(format!("{} is not a directory", path.display())));
    }
    let reader = std::fs::read_dir(&path)
        .map_err(|err| ApiError::new(StatusCode::FORBIDDEN, format!("cannot read {}: {err}", path.display())))?;
    let mut entries: Vec<DirEntry> = reader
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !query.hidden && name.starts_with('.') {
                return None;
            }
            let full = entry.path();
            Some(DirEntry { is_git: full.join(".git").exists(), path: full.to_string_lossy().into_owned(), name })
        })
        .collect();
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries.truncate(MAX_ENTRIES);
    Ok(Json(DirListing {
        parent: path.parent().map(|parent| parent.to_string_lossy().into_owned()),
        is_git: path.join(".git").exists(),
        path: path.to_string_lossy().into_owned(),
        entries,
    }))
}

#[derive(Serialize)]
pub struct WorkspaceDiff {
    is_git: bool,
    branch: Option<String>,
    /// Unified diff of everything that differs from the last commit,
    /// untracked files included.
    diff: String,
    truncated: bool,
}

async fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await
        .ok()?;
    output.status.success().then_some(output.stdout)
}

pub async fn diff(workspace: &Path) -> WorkspaceDiff {
    let inside = git(workspace, &["rev-parse", "--is-inside-work-tree"]).await;
    if inside.as_deref().map(|out| out.trim_ascii()) != Some(b"true".as_slice()) {
        return WorkspaceDiff { is_git: false, branch: None, diff: String::new(), truncated: false };
    }
    let branch = git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .filter(|name| !name.is_empty());

    // Against HEAD when there is one; a repository without commits has only an index.
    let has_head = git(workspace, &["rev-parse", "--verify", "-q", "HEAD"]).await.is_some();
    let base: &[&str] = if has_head {
        &["diff", "HEAD", "--no-color", "--no-ext-diff", "--relative"]
    } else {
        &["diff", "--cached", "--no-color", "--no-ext-diff", "--relative"]
    };
    let mut diff = git(workspace, base).await.map(|out| String::from_utf8_lossy(&out).into_owned()).unwrap_or_default();

    // `git diff` ignores untracked files, which is exactly what a new file is.
    if let Some(listing) = git(workspace, &["ls-files", "--others", "--exclude-standard", "-z"]).await {
        let names = listing.split(|byte| *byte == 0).filter(|name| !name.is_empty()).take(MAX_UNTRACKED_FILES);
        for name in names {
            let name = String::from_utf8_lossy(name).into_owned();
            let file = workspace.join(&name);
            let Ok(metadata) = std::fs::metadata(&file) else { continue };
            if !metadata.is_file() {
                continue;
            }
            let header = format!("diff --git a/{name} b/{name}\nnew file mode 100644\n--- /dev/null\n+++ b/{name}\n");
            let text = if metadata.len() > MAX_UNTRACKED_FILE_BYTES {
                None
            } else {
                std::fs::read(&file).ok().and_then(|bytes| String::from_utf8(bytes).ok())
            };
            match text {
                Some(text) if !text.is_empty() => {
                    let lines: Vec<&str> = text.lines().collect();
                    diff.push_str(&header);
                    diff.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
                    for line in lines {
                        diff.push('+');
                        diff.push_str(line);
                        diff.push('\n');
                    }
                }
                Some(_) => diff.push_str(&header),
                None => {
                    diff.push_str(&format!("diff --git a/{name} b/{name}\nnew file mode 100644\nBinary or large file {name} (not shown)\n"));
                }
            }
            if diff.len() > MAX_DIFF_BYTES {
                break;
            }
        }
    }

    let truncated = diff.len() > MAX_DIFF_BYTES;
    if truncated {
        let mut cut = MAX_DIFF_BYTES;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
    }
    WorkspaceDiff { is_git: true, branch, diff, truncated }
}
