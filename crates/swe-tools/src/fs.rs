//! File tools: read, write, edit, list.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use swe_protocol::{ToolDisplay, ToolKind};

use crate::text::{truncate_line, unified_diff};
use crate::{Tool, ToolContext, ToolOutput, arg_str, display_path, parse_input};

const DEFAULT_READ_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
/// A page of read_file output stays below the size at which the agent loop would
/// cut its middle out; the footer then names the line to continue from.
const MAX_READ_OUTPUT_BYTES: usize = 80_000;
const MAX_READ_BYTES: u64 = 20 * 1024 * 1024;
const MAX_DIR_ENTRIES: usize = 1000;

/// Read a file as UTF-8 text, with errors phrased for the model.
async fn read_text(path: &Path, shown: &str) -> Result<String, ToolOutput> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolOutput::error(format!("File not found: {shown}")));
        }
        Err(err) => return Err(ToolOutput::error(format!("Cannot access {shown}: {err}"))),
    };
    if metadata.is_dir() {
        return Err(ToolOutput::error(format!(
            "{shown} is a directory; use list_dir to see its entries."
        )));
    }
    if metadata.len() > MAX_READ_BYTES {
        return Err(ToolOutput::error(format!(
            "{shown} is {} MB, too large to read. Use grep to find the relevant part or bash to inspect it.",
            metadata.len() / (1024 * 1024)
        )));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|err| ToolOutput::error(format!("Cannot read {shown}: {err}")))?;
    String::from_utf8(bytes).map_err(|_| {
        ToolOutput::error(format!(
            "{shown} is not UTF-8 text (binary file?). Use bash to inspect it."
        ))
    })
}

pub struct ReadFile;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> String {
        format!(
            "Read a text file. Returns the content with line numbers (`N\\tline`). Reads up to \
             {DEFAULT_READ_LINES} lines from the start by default; for larger files pass `offset` (1-based \
             first line) and `limit`. Read a file before editing it. Independent reads can be issued \
             together in one turn and run in parallel."
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory."},
                "offset": {"type": "integer", "description": "1-based line number to start from."},
                "limit": {"type": "integer", "description": "Maximum number of lines to return."}
            },
            "required": ["path"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, input: &Value) -> String {
        arg_str(input, "path").to_string()
    }

    fn paths(&self, input: &Value) -> Vec<String> {
        vec![arg_str(input, "path").to_string()]
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: ReadArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        let path = ctx.resolve(&args.path);
        let shown = display_path(&ctx.cwd, &path);
        let text = match read_text(&path, &shown).await {
            Ok(text) => text,
            Err(output) => return output,
        };
        if text.is_empty() {
            return ToolOutput::ok("(file is empty)");
        }
        let total = text.lines().count();
        let start = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(DEFAULT_READ_LINES).max(1);
        if start > total {
            return ToolOutput::error(format!("offset {start} is past the end of {shown} ({total} lines)."));
        }
        let mut out = String::new();
        let mut last = start - 1;
        for (index, line) in text.lines().enumerate().skip(start - 1).take(limit) {
            let numbered = format!("{}\t{}\n", index + 1, truncate_line(line, MAX_LINE_CHARS));
            if !out.is_empty() && out.len() + numbered.len() > MAX_READ_OUTPUT_BYTES {
                break;
            }
            last = index + 1;
            out.push_str(&numbered);
        }
        if last < total {
            out.push_str(&format!(
                "\n[Showing lines {start}-{last} of {total}. Use offset={} to continue.]",
                last + 1
            ));
        }
        ToolOutput::ok(out)
    }
}

/// Say where a failed `old_string` almost matched, so the model can correct the
/// call without reading the whole file again.
fn near_miss(file: &str, old_string: &str) -> String {
    let Some(first) = old_string.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return "Read the file again to see its current content.".to_string();
    };
    match file.lines().enumerate().find(|(_, line)| line.trim() == first) {
        Some((index, line)) => format!(
            "Its first line matches line {} once leading and trailing whitespace is ignored, so the \
             difference is in the whitespace or in the lines after it. That line in the file is:\n{}",
            index + 1,
            truncate_line(line, 300)
        ),
        None => "Its first line does not occur anywhere in the file. Read the file again to see its \
                 current content."
            .to_string(),
    }
}

pub struct WriteFile;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> String {
        "Create a file or replace its entire content. Parent directories are created as needed. \
         Use this for new files; to change part of an existing file prefer edit_file, which is \
         cheaper and cannot clobber unrelated content."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory."},
                "content": {"type": "string", "description": "The complete new content of the file."}
            },
            "required": ["path", "content"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn summarize(&self, input: &Value) -> String {
        arg_str(input, "path").to_string()
    }

    fn paths(&self, input: &Value) -> Vec<String> {
        vec![arg_str(input, "path").to_string()]
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: WriteArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        let path = ctx.resolve(&args.path);
        let shown = display_path(&ctx.cwd, &path);
        if path.is_dir() {
            return ToolOutput::error(format!("{shown} is a directory."));
        }
        let existed = path.exists();
        // A previous version that is not text is simply replaced; the diff
        // then shows the new content as added.
        let before = if existed {
            tokio::fs::read_to_string(&path).await.unwrap_or_default()
        } else {
            String::new()
        };
        if let Some(parent) = path.parent()
            && let Err(err) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutput::error(format!("Cannot create directory {}: {err}", parent.display()));
        }
        if let Err(err) = tokio::fs::write(&path, &args.content).await {
            return ToolOutput::error(format!("Cannot write {shown}: {err}"));
        }
        let diff = unified_diff(&shown, &before, &args.content);
        let lines = args.content.lines().count();
        let message = if existed {
            format!("Overwrote {shown} ({lines} lines, +{} -{}).", diff.added, diff.removed)
        } else {
            format!("Created {shown} ({lines} lines).")
        };
        ToolOutput::ok(message).with_display(ToolDisplay::Diff {
            path: shown,
            diff: diff.unified,
            added: diff.added,
            removed: diff.removed,
            created: !existed,
        })
    }
}

pub struct EditFile;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> String {
        "Replace an exact string in a file. `old_string` must match the file content exactly, \
         including whitespace and indentation, and must occur exactly once unless `replace_all` \
         is true; include enough surrounding lines to make it unique. Do not include the line \
         number prefixes that read_file prints."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory."},
                "old_string": {"type": "string", "description": "Exact text to replace."},
                "new_string": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring a unique match."}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn summarize(&self, input: &Value) -> String {
        arg_str(input, "path").to_string()
    }

    fn paths(&self, input: &Value) -> Vec<String> {
        vec![arg_str(input, "path").to_string()]
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: EditArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        let path = ctx.resolve(&args.path);
        let shown = display_path(&ctx.cwd, &path);
        let before = match read_text(&path, &shown).await {
            Ok(text) => text,
            Err(output) => return output,
        };
        let (after, replaced) = match replace(&before, &args.old_string, &args.new_string, args.replace_all) {
            Ok(done) => done,
            Err(problem) => return ToolOutput::error(format!("In {shown}: {problem}")),
        };
        if let Err(err) = tokio::fs::write(&path, &after).await {
            return ToolOutput::error(format!("Cannot write {shown}: {err}"));
        }
        edited(shown, &before, &after, replaced)
    }
}

/// Apply one replacement to `before`. Returns the new text and how many
/// occurrences were replaced, or what the model has to correct.
fn replace(before: &str, old_string: &str, new_string: &str, replace_all: bool) -> Result<(String, usize), String> {
    if old_string.is_empty() {
        return Err("old_string is empty. To create a file use write_file.".into());
    }
    if old_string == new_string {
        return Err("old_string and new_string are identical; nothing to change.".into());
    }
    // Models write `\n`. In a CRLF file that would never match, and a replacement
    // written as it came would leave the file with mixed line endings.
    let crlf = |text: &str| text.replace("\r\n", "\n").replace('\n', "\r\n");
    let (old, new) = if before.contains("\r\n") && (before.contains(&crlf(old_string)) || !before.contains(old_string))
    {
        (crlf(old_string), crlf(new_string))
    } else {
        (old_string.to_string(), new_string.to_string())
    };

    let occurrences = before.matches(&old).count();
    if occurrences == 0 {
        return Err(format!(
            "old_string was not found. It must match exactly, including whitespace. {}",
            near_miss(before, old_string)
        ));
    }
    if occurrences > 1 && !replace_all {
        let lines: Vec<String> = before
            .match_indices(&old)
            .take(10)
            .map(|(index, _)| (before[..index].matches('\n').count() + 1).to_string())
            .collect();
        return Err(format!(
            "old_string occurs {occurrences} times (starting at line {}{}). Add surrounding context to make \
             it unique, or set replace_all to true.",
            lines.join(", "),
            if occurrences > lines.len() { ", ..." } else { "" }
        ));
    }
    if replace_all {
        Ok((before.replace(&old, &new), occurrences))
    } else {
        Ok((before.replacen(&old, &new, 1), 1))
    }
}

/// The result of a successful edit: a summary for the model, the diff for the UI.
fn edited(shown: String, before: &str, after: &str, replaced: usize) -> ToolOutput {
    let diff = unified_diff(&shown, before, after);
    ToolOutput::ok(format!(
        "Edited {shown}: {replaced} replacement{} (+{} -{}).",
        if replaced == 1 { "" } else { "s" },
        diff.added,
        diff.removed
    ))
    .with_display(ToolDisplay::Diff {
        path: shown,
        diff: diff.unified,
        added: diff.added,
        removed: diff.removed,
        created: false,
    })
}

pub struct MultiEdit;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiEditArgs {
    path: String,
    edits: Vec<Replacement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for MultiEdit {
    fn name(&self) -> &str {
        "multi_edit"
    }

    fn description(&self) -> String {
        "Make several replacements in one file in a single step. The edits are applied in order, each \
         to the result of the one before it, and the file is written only if every edit applies: if \
         one fails, nothing changes and the error names it. Each edit follows the rules of edit_file. \
         Use this instead of several edit_file calls on the same file."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory."},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Replacements, applied in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string", "description": "Exact text to replace."},
                            "new_string": {"type": "string", "description": "Replacement text."},
                            "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring a unique match."}
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn summarize(&self, input: &Value) -> String {
        arg_str(input, "path").to_string()
    }

    fn paths(&self, input: &Value) -> Vec<String> {
        vec![arg_str(input, "path").to_string()]
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: MultiEditArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        let path = ctx.resolve(&args.path);
        let shown = display_path(&ctx.cwd, &path);
        if args.edits.is_empty() {
            return ToolOutput::error("edits is empty; nothing to change.");
        }
        let before = match read_text(&path, &shown).await {
            Ok(text) => text,
            Err(output) => return output,
        };
        let mut after = before.clone();
        let mut replaced = 0;
        for (index, edit) in args.edits.iter().enumerate() {
            match replace(&after, &edit.old_string, &edit.new_string, edit.replace_all) {
                Ok((text, count)) => {
                    after = text;
                    replaced += count;
                }
                Err(problem) => {
                    return ToolOutput::error(format!(
                        "Edit {} of {} in {shown} failed, so the file was not changed: {problem}",
                        index + 1,
                        args.edits.len()
                    ));
                }
            }
        }
        if let Err(err) = tokio::fs::write(&path, &after).await {
            return ToolOutput::error(format!("Cannot write {shown}: {err}"));
        }
        edited(shown, &before, &after, replaced)
    }
}

pub struct ListDir;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> String {
        "List the entries of one directory (not recursive). Directories end with `/`. Use glob \
         to find files by name pattern across the tree."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list; defaults to the working directory."}
            }
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, input: &Value) -> String {
        let path = arg_str(input, "path");
        if path.is_empty() {
            ".".to_string()
        } else {
            path.to_string()
        }
    }

    fn paths(&self, input: &Value) -> Vec<String> {
        vec![self.summarize(input)]
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: ListArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        let path = ctx.resolve(args.path.as_deref().unwrap_or("."));
        let shown = display_path(&ctx.cwd, &path);
        let mut reader = match tokio::fs::read_dir(&path).await {
            Ok(reader) => reader,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::error(format!("Directory not found: {shown}"));
            }
            Err(err) => return ToolOutput::error(format!("Cannot list {shown}: {err}")),
        };
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        loop {
            match reader.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    match entry.file_type().await {
                        Ok(kind) if kind.is_dir() => dirs.push(format!("{name}/")),
                        _ => files.push(name),
                    }
                }
                Ok(None) => break,
                Err(err) => return ToolOutput::error(format!("Cannot list {shown}: {err}")),
            }
        }
        dirs.sort();
        files.sort();
        let total = dirs.len() + files.len();
        if total == 0 {
            return ToolOutput::ok("(empty directory)");
        }
        let mut entries: Vec<String> = dirs.into_iter().chain(files).collect();
        entries.truncate(MAX_DIR_ENTRIES);
        let mut out = entries.join("\n");
        if total > MAX_DIR_ENTRIES {
            out.push_str(&format!("\n[... {} more entries not shown]", total - MAX_DIR_ENTRIES));
        }
        ToolOutput::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ctx(dir: &tempfile::TempDir) -> ToolContext {
        ToolContext::new(dir.path())
    }

    #[tokio::test]
    async fn read_numbers_lines_and_pages() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let ctx = ctx(&dir);

        let all = ReadFile.run(json!({"path": "a.txt"}), &ctx).await;
        assert!(!all.is_error);
        assert_eq!(all.content, "1\tone\n2\ttwo\n3\tthree\n4\tfour\n");

        let page = ReadFile
            .run(json!({"path": "a.txt", "offset": 2, "limit": 2}), &ctx)
            .await;
        assert!(page.content.starts_with("2\ttwo\n3\tthree\n"));
        assert!(page.content.contains("Showing lines 2-3 of 4. Use offset=4"));

        let past = ReadFile.run(json!({"path": "a.txt", "offset": 9}), &ctx).await;
        assert!(past.is_error);
    }

    #[tokio::test]
    async fn read_reports_missing_binary_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bin"), [0u8, 159, 146, 150]).unwrap();
        let ctx = ctx(&dir);
        assert!(
            ReadFile
                .run(json!({"path": "nope"}), &ctx)
                .await
                .content
                .contains("File not found")
        );
        assert!(
            ReadFile
                .run(json!({"path": "bin"}), &ctx)
                .await
                .content
                .contains("not UTF-8")
        );
        assert!(
            ReadFile
                .run(json!({"path": "."}), &ctx)
                .await
                .content
                .contains("is a directory")
        );
        assert!(
            ReadFile
                .run(json!({"paht": "x"}), &ctx)
                .await
                .content
                .contains("Invalid arguments")
        );
    }

    #[tokio::test]
    async fn write_creates_parents_and_reports_a_diff() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        let created = WriteFile
            .run(json!({"path": "src/deep/new.rs", "content": "fn main() {}\n"}), &ctx)
            .await;
        assert!(!created.is_error, "{}", created.content);
        assert!(created.content.contains("Created src/deep/new.rs"));
        assert!(matches!(
            created.display,
            Some(ToolDisplay::Diff {
                created: true,
                added: 1,
                removed: 0,
                ..
            })
        ));
        assert_eq!(
            fs::read_to_string(dir.path().join("src/deep/new.rs")).unwrap(),
            "fn main() {}\n"
        );

        let replaced = WriteFile
            .run(
                json!({"path": "src/deep/new.rs", "content": "fn main() { run() }\n"}),
                &ctx,
            )
            .await;
        assert!(replaced.content.contains("Overwrote"));
        assert!(matches!(
            replaced.display,
            Some(ToolDisplay::Diff {
                created: false,
                added: 1,
                removed: 1,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn edit_requires_a_unique_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        fs::write(&file, "let a = 1;\nlet b = 1;\n").unwrap();
        let ctx = ctx(&dir);

        let ambiguous = EditFile
            .run(
                json!({"path": "a.rs", "old_string": "= 1;", "new_string": "= 2;"}),
                &ctx,
            )
            .await;
        assert!(ambiguous.is_error);
        assert!(ambiguous.content.contains("occurs 2 times"));
        assert!(
            ambiguous.content.contains("starting at line 1, 2"),
            "{}",
            ambiguous.content
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "let a = 1;\nlet b = 1;\n",
            "file untouched on error"
        );

        let missing = EditFile
            .run(json!({"path": "a.rs", "old_string": "nope", "new_string": "x"}), &ctx)
            .await;
        assert!(missing.content.contains("not found"));
        assert!(
            missing.content.contains("does not occur anywhere"),
            "{}",
            missing.content
        );

        // Wrong indentation is the usual reason; the error points at the line.
        let indented = EditFile
            .run(
                json!({"path": "a.rs", "old_string": "    let b = 1;\nlet c", "new_string": "x"}),
                &ctx,
            )
            .await;
        assert!(indented.is_error);
        assert!(indented.content.contains("matches line 2"), "{}", indented.content);

        let unique = EditFile
            .run(
                json!({"path": "a.rs", "old_string": "let a = 1;", "new_string": "let a = 5;"}),
                &ctx,
            )
            .await;
        assert!(!unique.is_error, "{}", unique.content);
        assert_eq!(fs::read_to_string(&file).unwrap(), "let a = 5;\nlet b = 1;\n");

        let all = EditFile
            .run(
                json!({"path": "a.rs", "old_string": "let", "new_string": "const", "replace_all": true}),
                &ctx,
            )
            .await;
        assert!(all.content.contains("2 replacements"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "const a = 5;\nconst b = 1;\n");
    }

    #[tokio::test]
    async fn edit_matches_across_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("win.txt");
        fs::write(&file, "first\r\nsecond\r\nthird\r\n").unwrap();
        let out = EditFile
            .run(
                json!({"path": "win.txt", "old_string": "first\nsecond", "new_string": "1\n2"}),
                &ctx(&dir),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(fs::read_to_string(&file).unwrap(), "1\r\n2\r\nthird\r\n");

        // A one-line old_string matches as it is; the new lines still get the file's endings.
        let out = EditFile
            .run(
                json!({"path": "win.txt", "old_string": "third", "new_string": "3\n4"}),
                &ctx(&dir),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(fs::read_to_string(&file).unwrap(), "1\r\n2\r\n3\r\n4\r\n");
    }

    #[tokio::test]
    async fn list_dir_sorts_directories_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("b.txt"), "").unwrap();
        fs::write(dir.path().join("a.txt"), "").unwrap();
        let out = ListDir.run(json!({}), &ctx(&dir)).await;
        assert_eq!(out.content, "src/\na.txt\nb.txt");
        assert!(ListDir.run(json!({"path": "missing"}), &ctx(&dir)).await.is_error);
    }
}

#[cfg(test)]
mod budget_and_multi_edit_tests {
    use std::fs;

    use super::*;

    #[tokio::test]
    async fn a_large_page_ends_at_a_line_and_says_where_to_continue() {
        let dir = tempfile::tempdir().unwrap();
        let line = "x".repeat(99);
        fs::write(dir.path().join("big.txt"), format!("{line}\n").repeat(3000)).unwrap();
        let ctx = ToolContext::new(dir.path());

        let page = ReadFile.run(json!({"path": "big.txt"}), &ctx).await;
        assert!(!page.is_error);
        assert!(
            page.content.len() <= MAX_READ_OUTPUT_BYTES + 200,
            "{}",
            page.content.len()
        );
        let shown = page.content.lines().filter(|l| l.contains('\t')).count();
        assert!(shown < 2000, "the byte budget, not the line limit, ended the page");
        assert!(
            page.content.ends_with(&format!(
                "[Showing lines 1-{shown} of 3000. Use offset={} to continue.]",
                shown + 1
            )),
            "{}",
            &page.content[page.content.len() - 120..]
        );

        let next = ReadFile
            .run(json!({"path": "big.txt", "offset": shown + 1}), &ctx)
            .await;
        assert!(next.content.starts_with(&format!("{}\t", shown + 1)));
    }

    #[tokio::test]
    async fn multi_edit_applies_every_edit_or_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        fs::write(&file, "let a = 1;\nlet b = 2;\nlet c = 3;\n").unwrap();
        let ctx = ToolContext::new(dir.path());

        let failed = MultiEdit
            .run(
                json!({"path": "a.rs", "edits": [
                    {"old_string": "a = 1", "new_string": "a = 10"},
                    {"old_string": "missing", "new_string": "x"}
                ]}),
                &ctx,
            )
            .await;
        assert!(failed.is_error);
        assert!(failed.content.contains("Edit 2 of 2"), "{}", failed.content);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "let a = 1;\nlet b = 2;\nlet c = 3;\n",
            "a failed edit leaves the file as it was"
        );

        let done = MultiEdit
            .run(
                json!({"path": "a.rs", "edits": [
                    {"old_string": "a = 1", "new_string": "a = 10"},
                    // Applies to the result of the first edit.
                    {"old_string": "a = 10;\nlet b = 2", "new_string": "a = 10;\nlet b = 20"},
                    {"old_string": "let", "new_string": "const", "replace_all": true}
                ]}),
                &ctx,
            )
            .await;
        assert!(!done.is_error, "{}", done.content);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "const a = 10;\nconst b = 20;\nconst c = 3;\n"
        );
        assert!(done.content.contains("5 replacements"), "{}", done.content);
    }

    #[tokio::test]
    async fn unknown_arguments_are_named_instead_of_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        let out = ReadFile
            .run(json!({"path": "a.txt", "lines": 5}), &ToolContext::new(dir.path()))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown field `lines`"), "{}", out.content);
        assert!(
            out.content.contains("limit"),
            "the valid names are listed: {}",
            out.content
        );
    }
}
