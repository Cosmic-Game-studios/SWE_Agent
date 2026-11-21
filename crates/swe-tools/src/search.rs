//! Search tools: find files by name (`glob`) and by content (`grep`).
//!
//! Both walk the tree with the `ignore` crate, so `.gitignore` rules apply and
//! `.git` itself is never descended into.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use swe_protocol::ToolKind;

use crate::text::truncate_line;
use crate::{Tool, ToolContext, ToolOutput, arg_str, display_path, parse_input};

const MAX_GLOB_RESULTS: usize = 200;
const DEFAULT_GREP_RESULTS: usize = 100;
const MAX_GREP_RESULTS: usize = 1000;
const MAX_GREP_FILE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_MATCH_LINE_CHARS: usize = 400;

fn walker(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
}

/// Compile a glob. A pattern without `/` matches file names at any depth,
/// which is what `*.rs` means to most people.
fn compile_glob(pattern: &str) -> Result<GlobMatcher, String> {
    let pattern = pattern.trim_start_matches("./");
    let full = if pattern.contains('/') { pattern.to_string() } else { format!("**/{pattern}") };
    GlobBuilder::new(&full)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|err| format!("Invalid glob pattern `{pattern}`: {err}"))
}

pub struct Glob;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> String {
        format!(
            "Find files by name pattern, e.g. `**/*.rs`, `src/**/*.test.ts` or `Cargo.toml`. Respects \
             .gitignore. Returns up to {MAX_GLOB_RESULTS} paths, most recently modified first. Use grep \
             to search file contents instead."
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern. Without a `/` it matches file names at any depth."},
                "path": {"type": "string", "description": "Directory to search in; defaults to the working directory."}
            },
            "required": ["pattern"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, input: &Value) -> String {
        arg_str(input, "pattern").to_string()
    }

    fn paths(&self, input: &Value) -> Vec<String> {
        let path = arg_str(input, "path");
        vec![if path.is_empty() { ".".to_string() } else { path.to_string() }]
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: GlobArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        let root = ctx.resolve(args.path.as_deref().unwrap_or("."));
        if !root.is_dir() {
            return ToolOutput::error(format!("Directory not found: {}", display_path(&ctx.cwd, &root)));
        }
        let matcher = match compile_glob(&args.pattern) {
            Ok(matcher) => matcher,
            Err(message) => return ToolOutput::error(message),
        };
        let cwd = ctx.cwd.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut hits: Vec<(SystemTime, PathBuf)> = Vec::new();
            for entry in walker(&root).flatten() {
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    continue;
                }
                let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                if matcher.is_match(relative) {
                    let modified = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
                    hits.push((modified, entry.into_path()));
                }
            }
            hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let total = hits.len();
            let lines: Vec<String> = hits
                .into_iter()
                .take(MAX_GLOB_RESULTS)
                .map(|(_, path)| display_path(&cwd, &path))
                .collect();
            (total, lines)
        })
        .await;
        let (total, lines) = match result {
            Ok(found) => found,
            Err(err) => return ToolOutput::error(format!("glob failed: {err}")),
        };
        if total == 0 {
            return ToolOutput::ok(format!("No files match `{}`.", args.pattern));
        }
        let mut out = lines.join("\n");
        if total > MAX_GLOB_RESULTS {
            out.push_str(&format!(
                "\n[... {} more matches not shown; narrow the pattern]",
                total - MAX_GLOB_RESULTS
            ));
        }
        ToolOutput::ok(out)
    }
}

pub struct Grep;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    files_only: bool,
    #[serde(default)]
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> String {
        format!(
            "Search file contents with a regular expression (Rust regex syntax, one line at a time). \
             Respects .gitignore and skips binary files. Returns `path:line:text` for each match, up to \
             {DEFAULT_GREP_RESULTS} by default. Use `glob` to restrict which files are searched, \
             `files_only` to list just the matching files, and `context` for surrounding lines. Prefer \
             this over running grep or rg through bash."
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for."},
                "path": {"type": "string", "description": "File or directory to search; defaults to the working directory."},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. `*.rs` or `src/**/*.ts`."},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive search."},
                "context": {"type": "integer", "description": "Lines of context to show before and after each match (0-10)."},
                "files_only": {"type": "boolean", "description": "Return only the paths of files that contain a match."},
                "max_results": {"type": "integer", "description": "Maximum number of matches to return."}
            },
            "required": ["pattern"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, input: &Value) -> String {
        let pattern = arg_str(input, "pattern");
        match (arg_str(input, "path"), arg_str(input, "glob")) {
            ("", "") => pattern.to_string(),
            (path, "") => format!("{pattern} in {path}"),
            ("", glob) => format!("{pattern} in {glob}"),
            (path, glob) => format!("{pattern} in {path}/{glob}"),
        }
    }

    fn paths(&self, input: &Value) -> Vec<String> {
        let path = arg_str(input, "path");
        vec![if path.is_empty() { ".".to_string() } else { path.to_string() }]
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: GrepArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        let regex = match RegexBuilder::new(&args.pattern).case_insensitive(args.ignore_case).build() {
            Ok(regex) => regex,
            Err(err) => return ToolOutput::error(format!("Invalid regular expression: {err}")),
        };
        let root = ctx.resolve(args.path.as_deref().unwrap_or("."));
        if !root.exists() {
            return ToolOutput::error(format!("Path not found: {}", display_path(&ctx.cwd, &root)));
        }
        let matcher = match args.glob.as_deref().map(compile_glob).transpose() {
            Ok(matcher) => matcher,
            Err(message) => return ToolOutput::error(message),
        };
        let limit = args.max_results.unwrap_or(DEFAULT_GREP_RESULTS).clamp(1, MAX_GREP_RESULTS);
        let context = args.context.unwrap_or(0).min(10);
        let files_only = args.files_only;
        let cwd = ctx.cwd.clone();
        let cancel = ctx.cancel.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut out: Vec<String> = Vec::new();
            let mut matches = 0usize;
            let mut truncated = false;
            'files: for entry in walker(&root).flatten() {
                if cancel.is_cancelled() {
                    break;
                }
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    continue;
                }
                let path = entry.path();
                if let Some(matcher) = &matcher {
                    let relative = path.strip_prefix(&root).unwrap_or(path);
                    if !matcher.is_match(relative) {
                        continue;
                    }
                }
                if entry.metadata().map(|m| m.len() > MAX_GREP_FILE_BYTES).unwrap_or(true) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(path) else { continue };
                if bytes.iter().take(8192).any(|&b| b == 0) {
                    continue; // binary
                }
                let text = String::from_utf8_lossy(&bytes);
                let shown = display_path(&cwd, path);
                let lines: Vec<&str> = text.lines().collect();
                let mut last_printed: Option<usize> = None;
                for (index, line) in lines.iter().enumerate() {
                    if !regex.is_match(line) {
                        continue;
                    }
                    if matches == limit {
                        truncated = true;
                        break 'files;
                    }
                    matches += 1;
                    if files_only {
                        out.push(shown.clone());
                        continue 'files;
                    }
                    let from = index.saturating_sub(context);
                    let to = (index + context).min(lines.len() - 1);
                    if context > 0 && last_printed.is_some_and(|last| from > last + 1) {
                        out.push("--".to_string());
                    }
                    for (number, context_line) in lines.iter().enumerate().take(to + 1).skip(from) {
                        if last_printed.is_some_and(|last| number <= last) {
                            continue;
                        }
                        let separator = if number == index || regex.is_match(context_line) { ':' } else { '-' };
                        out.push(format!(
                            "{shown}{separator}{}{separator}{}",
                            number + 1,
                            truncate_line(context_line, MAX_MATCH_LINE_CHARS)
                        ));
                        last_printed = Some(number);
                    }
                }
            }
            (out, matches, truncated)
        })
        .await;

        let (lines, matches, truncated) = match result {
            Ok(found) => found,
            Err(err) => return ToolOutput::error(format!("grep failed: {err}")),
        };
        if matches == 0 {
            return ToolOutput::ok(format!("No matches for `{}`.", args.pattern));
        }
        let mut out = lines.join("\n");
        if truncated {
            out.push_str(&format!(
                "\n[Stopped after {limit} matches; narrow the pattern or raise max_results]"
            ));
        }
        ToolOutput::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/util")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {\n    helper();\n}\n").unwrap();
        fs::write(root.join("src/util/helper.rs"), "pub fn helper() {}\n// TODO: Helper docs\n").unwrap();
        fs::write(root.join("target/debug/build.rs"), "fn helper() {}\n").unwrap();
        fs::write(root.join(".git/config"), "helper\n").unwrap();
        fs::write(root.join("blob.bin"), [b'h', 0, b'e', b'l', b'p', b'e', b'r']).unwrap();
        dir
    }

    fn sorted_lines(output: &ToolOutput) -> Vec<&str> {
        let mut lines: Vec<&str> = output.content.lines().collect();
        lines.sort();
        lines
    }

    #[tokio::test]
    async fn glob_matches_by_name_and_respects_gitignore() {
        let dir = project();
        let ctx = ToolContext::new(dir.path());
        let rs = Glob.run(json!({"pattern": "*.rs"}), &ctx).await;
        assert_eq!(sorted_lines(&rs), ["src/main.rs", "src/util/helper.rs"]);

        let nested = Glob.run(json!({"pattern": "src/util/*.rs"}), &ctx).await;
        assert_eq!(nested.content, "src/util/helper.rs");

        let scoped = Glob.run(json!({"pattern": "*.rs", "path": "src/util"}), &ctx).await;
        assert_eq!(scoped.content, "src/util/helper.rs");

        let none = Glob.run(json!({"pattern": "*.py"}), &ctx).await;
        assert!(!none.is_error);
        assert!(none.content.contains("No files match"));

        assert!(Glob.run(json!({"pattern": "[bad"}), &ctx).await.is_error);
    }

    #[tokio::test]
    async fn grep_finds_lines_and_skips_ignored_binary_and_git() {
        let dir = project();
        let ctx = ToolContext::new(dir.path());
        let out = Grep.run(json!({"pattern": "helper"}), &ctx).await;
        assert_eq!(
            sorted_lines(&out),
            ["src/main.rs:2:    helper();", "src/util/helper.rs:1:pub fn helper() {}"]
        );

        let insensitive = Grep.run(json!({"pattern": "helper", "ignore_case": true, "glob": "*.rs", "path": "src/util"}), &ctx).await;
        assert_eq!(insensitive.content.lines().count(), 2);

        let files = Grep.run(json!({"pattern": "helper", "files_only": true}), &ctx).await;
        assert_eq!(sorted_lines(&files), ["src/main.rs", "src/util/helper.rs"]);

        let none = Grep.run(json!({"pattern": "zzz_not_there"}), &ctx).await;
        assert!(none.content.contains("No matches"));
        assert!(Grep.run(json!({"pattern": "("}), &ctx).await.is_error);
    }

    #[tokio::test]
    async fn grep_context_and_result_limit() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        fs::write(dir.path().join("a.txt"), body).unwrap();
        let ctx = ToolContext::new(dir.path());

        let with_context = Grep.run(json!({"pattern": "line (5|20)$", "context": 1}), &ctx).await;
        assert_eq!(
            with_context.content,
            "a.txt-4-line 4\na.txt:5:line 5\na.txt-6-line 6\n--\na.txt-19-line 19\na.txt:20:line 20\na.txt-21-line 21"
        );

        let limited = Grep.run(json!({"pattern": "line", "max_results": 3}), &ctx).await;
        assert!(limited.content.starts_with("a.txt:1:line 1\na.txt:2:line 2\na.txt:3:line 3\n"));
        assert!(limited.content.contains("Stopped after 3 matches"));
    }
}
