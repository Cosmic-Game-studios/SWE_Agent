//! Shell tools: run commands in the foreground or keep them running in the
//! background (dev servers, watchers) and poll their output later.
//!
//! Every command gets its own process group so that a timeout, a user cancel
//! or the end of the session takes the whole process tree down, not just the
//! shell that started it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use swe_protocol::{ToolDisplay, ToolKind};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::text::truncate_middle;
use crate::{OutputSink, Tool, ToolContext, ToolOutput, arg_str, parse_input, plain_path};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// Output kept per command for the model. The head and the tail survive.
const MAX_OUTPUT_BYTES: usize = 48_000;
/// Background output retained between polls.
const MAX_BACKGROUND_BUFFER: usize = 1_000_000;

/// The shell that runs commands on this machine.
struct Shell {
    program: PathBuf,
    /// Takes `-c <command>` and POSIX syntax; otherwise it is cmd.exe.
    posix: bool,
    /// What the model is told about it.
    description: &'static str,
}

fn shell() -> &'static Shell {
    static SHELL: OnceLock<Shell> = OnceLock::new();
    SHELL.get_or_init(detect_shell)
}

#[cfg(unix)]
fn detect_shell() -> Shell {
    let bash = Path::new("/bin/bash").exists() || Path::new("/usr/bin/bash").exists();
    Shell {
        program: PathBuf::from(if bash { "bash" } else { "sh" }),
        posix: true,
        description: if bash {
            "bash"
        } else {
            "sh (POSIX shell, no bash extensions)"
        },
    }
}

#[cfg(not(unix))]
fn detect_shell() -> Shell {
    // Git for Windows ships a real bash, which is what models write for. A `bash.exe`
    // found on PATH is not trusted: in System32 it is the WSL launcher, which runs
    // in another file system.
    let git_bash = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .flat_map(|root| {
            let root = PathBuf::from(root);
            [root.join(r"Git\bin\bash.exe"), root.join(r"Programs\Git\bin\bash.exe")]
        })
        .find(|candidate| candidate.is_file());
    match git_bash {
        Some(program) => Shell {
            program,
            posix: true,
            description: "bash from Git for Windows (Unix commands and `/` paths work; Windows programs are on PATH)",
        },
        None => Shell {
            program: PathBuf::from("cmd"),
            posix: false,
            description: "Windows cmd.exe, not bash (use `dir`, `type`, `set NAME=value` and `2>NUL`; \
                          quote with double quotes only)",
        },
    }
}

/// The shell commands run in, described for the model.
pub fn shell_description() -> &'static str {
    shell().description
}

#[cfg(windows)]
fn cmd_exe_arguments(cmd: &mut Command, command: &str) {
    // cmd.exe does not understand the `\"` escaping that `arg` applies, so a quoted
    // argument would reach the program mangled. With /S it strips exactly the outer
    // pair of quotes and takes everything between them literally.
    cmd.raw_arg("/D /S /C").raw_arg(format!("\"{command}\""));
}

#[cfg(not(windows))]
fn cmd_exe_arguments(cmd: &mut Command, command: &str) {
    cmd.arg("/C").arg(command);
}

fn shell_command(command: &str, cwd: &Path) -> Command {
    let shell = shell();
    let mut cmd = Command::new(&shell.program);
    if shell.posix {
        cmd.arg("-c").arg(command);
    } else {
        cmd_exe_arguments(&mut cmd, command);
    }
    // Own process group: lets us signal the whole tree.
    own_process_group(&mut cmd);
    // A verbatim `\\?\` directory makes cmd.exe run in the Windows directory instead.
    cmd.current_dir(plain_path(cwd))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Keep tools from waiting on a pager, an editor or a credential prompt.
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        // Python otherwise writes the legacy code page into the pipe on Windows.
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .kill_on_drop(true);
    cmd
}

/// Put a command into its own process group, so [`kill_tree`] can stop
/// everything it starts. No-op on platforms without process groups.
pub fn own_process_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Terminate a command and everything it started.
pub fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: plain syscall on a process group we created ourselves.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    // Killing the shell alone would leave the build or server it started running,
    // holding its locks and ports.
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.start_kill();
}

/// Shared, size-capped capture of a command's combined output.
#[derive(Default)]
struct Capture {
    text: String,
    /// Bytes dropped from the front to honour the cap.
    dropped: usize,
}

impl Capture {
    fn push(&mut self, chunk: &str, cap: usize) {
        self.text.push_str(chunk);
        if self.text.len() > cap {
            let mut cut = self.text.len() - cap;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut;
        }
    }
}

/// Copy a pipe into the capture (and the live sink) until it closes.
async fn pump(mut reader: impl AsyncRead + Unpin, capture: Arc<Mutex<Capture>>, sink: OutputSink, cap: usize) {
    let mut buffer = vec![0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pending.extend_from_slice(&buffer[..n]);
                // Only forward complete UTF-8; keep a split sequence for the next read.
                let valid = match std::str::from_utf8(&pending) {
                    Ok(_) => pending.len(),
                    Err(err) if err.error_len().is_none() => err.valid_up_to(),
                    Err(_) => pending.len(),
                };
                let chunk = String::from_utf8_lossy(&pending[..valid]).into_owned();
                pending.drain(..valid);
                if !chunk.is_empty() {
                    capture.lock().unwrap().push(&chunk, cap);
                    sink.send(chunk);
                }
            }
        }
    }
    if !pending.is_empty() {
        let chunk = String::from_utf8_lossy(&pending).into_owned();
        capture.lock().unwrap().push(&chunk, cap);
        sink.send(chunk);
    }
}

struct Background {
    command: String,
    capture: Arc<Mutex<Capture>>,
    /// How much of the capture (in absolute bytes) was already returned.
    read_offset: usize,
    /// `None` while running.
    exit: Arc<Mutex<Option<Option<i32>>>>,
    kill: tokio::sync::mpsc::Sender<()>,
    started: Instant,
}

/// Background processes of one session.
#[derive(Default)]
pub struct ShellManager {
    processes: Mutex<HashMap<String, Background>>,
    counter: Mutex<u32>,
}

impl ShellManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn spawn_background(&self, command: &str, cwd: &Path) -> Result<String, String> {
        let mut child = shell_command(command, cwd)
            .spawn()
            .map_err(|err| format!("Cannot start command: {err}"))?;
        let capture = Arc::new(Mutex::new(Capture::default()));
        let exit = Arc::new(Mutex::new(None));
        let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel::<()>(1);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if let Some(stdout) = stdout {
            tokio::spawn(pump(stdout, capture.clone(), OutputSink::none(), MAX_BACKGROUND_BUFFER));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(pump(stderr, capture.clone(), OutputSink::none(), MAX_BACKGROUND_BUFFER));
        }
        let exit_slot = exit.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.ok(),
                _ = kill_rx.recv() => {
                    kill_tree(&mut child);
                    child.wait().await.ok()
                }
            };
            *exit_slot.lock().unwrap() = Some(status.and_then(|s| s.code()));
        });

        let id = {
            let mut counter = self.counter.lock().unwrap();
            *counter += 1;
            format!("bg{}", *counter)
        };
        self.processes.lock().unwrap().insert(
            id.clone(),
            Background {
                command: command.to_string(),
                capture,
                read_offset: 0,
                exit,
                kill: kill_tx,
                started: Instant::now(),
            },
        );
        Ok(id)
    }

    /// Output produced since the previous poll, plus the process state.
    fn poll(&self, id: &str) -> Result<String, String> {
        let mut processes = self.processes.lock().unwrap();
        if !processes.contains_key(id) {
            return Err(unknown_process(id, &processes_list(&processes)));
        }
        let process = processes.get_mut(id).expect("checked above");
        let (fresh, skipped) = {
            let capture = process.capture.lock().unwrap();
            let end = capture.dropped + capture.text.len();
            let start = process.read_offset.max(capture.dropped);
            let skipped = start - process.read_offset;
            let fresh = capture.text[start - capture.dropped..].to_string();
            process.read_offset = end;
            (fresh, skipped)
        };
        let state = match *process.exit.lock().unwrap() {
            None => format!("running for {}s", process.started.elapsed().as_secs()),
            Some(Some(code)) => format!("exited with code {code}"),
            Some(None) => "terminated by a signal".to_string(),
        };
        let mut out = format!("[{id}: {state}] {}\n", process.command);
        if skipped > 0 {
            out.push_str(&format!("[... {skipped} bytes of older output were discarded ...]\n"));
        }
        if fresh.is_empty() {
            out.push_str("(no new output)");
        } else {
            out.push_str(&truncate_middle(&fresh, MAX_OUTPUT_BYTES));
        }
        Ok(out)
    }

    fn kill(&self, id: &str) -> Result<String, String> {
        let processes = self.processes.lock().unwrap();
        let process = processes
            .get(id)
            .ok_or_else(|| unknown_process(id, &processes_list(&processes)))?;
        if process.exit.lock().unwrap().is_some() {
            return Ok(format!("{id} had already exited."));
        }
        let _ = process.kill.try_send(());
        Ok(format!("Stopped {id} ({}).", process.command))
    }

    /// Stop every background process; called when the session ends.
    pub fn kill_all(&self) {
        for process in self.processes.lock().unwrap().values() {
            let _ = process.kill.try_send(());
        }
    }

    /// Ids and commands of processes that are still running.
    pub fn running(&self) -> Vec<(String, String)> {
        let processes = self.processes.lock().unwrap();
        let mut running: Vec<(String, String)> = processes
            .iter()
            .filter(|(_, p)| p.exit.lock().unwrap().is_none())
            .map(|(id, p)| (id.clone(), p.command.clone()))
            .collect();
        running.sort();
        running
    }
}

impl Drop for ShellManager {
    fn drop(&mut self) {
        self.kill_all();
    }
}

fn processes_list(processes: &HashMap<String, Background>) -> Vec<String> {
    let mut ids: Vec<String> = processes.keys().cloned().collect();
    ids.sort();
    ids
}

fn unknown_process(id: &str, known: &[String]) -> String {
    if known.is_empty() {
        format!("No background process `{id}`; none have been started.")
    } else {
        format!("No background process `{id}`. Known ids: {}.", known.join(", "))
    }
}

pub struct Bash;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    background: bool,
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> String {
        format!(
            "Run a shell command in the working directory and return its combined stdout and stderr \
             with the exit code. Use it to build, test, run programs, use git and install \
             dependencies. The shell on this machine is {}. Each call starts a fresh shell: `cd` and exported variables do not carry \
             over, so chain with `&&` or use absolute paths. The default timeout is {}s (maximum {}s); \
             pass `timeout_ms` for slow builds or test suites. Set `background` to true for commands \
             that keep running (dev servers, watchers): the call returns an id immediately, and \
             bash_output / kill_process manage the process. Commands run without a terminal, so \
             never start anything that waits for interactive input. Prefer read_file, edit_file, \
             glob and grep over cat, sed, find and grep in the shell.",
            shell_description(),
            DEFAULT_TIMEOUT_MS / 1000,
            MAX_TIMEOUT_MS / 1000
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command line to run."},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds for a foreground command."},
                "background": {"type": "boolean", "description": "Start the command and return immediately, leaving it running."}
            },
            "required": ["command"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn summarize(&self, input: &Value) -> String {
        arg_str(input, "command").to_string()
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: BashArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        if args.command.trim().is_empty() {
            return ToolOutput::error("command is empty.");
        }
        if !ctx.cwd.is_dir() {
            return ToolOutput::error(format!("Working directory does not exist: {}", ctx.cwd.display()));
        }

        if args.background {
            return match ctx.shell.spawn_background(&args.command, &ctx.cwd) {
                Ok(id) => ToolOutput::ok(format!(
                    "Started in the background as `{id}`. Use bash_output with this id to read its output \
                     and kill_process to stop it."
                ))
                .with_display(ToolDisplay::Command {
                    command: args.command,
                    exit_code: None,
                    background_id: Some(id),
                }),
                Err(message) => ToolOutput::error(message),
            };
        }

        let timeout = Duration::from_millis(
            args.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1_000, MAX_TIMEOUT_MS),
        );
        let mut child = match shell_command(&args.command, &ctx.cwd).spawn() {
            Ok(child) => child,
            Err(err) => return ToolOutput::error(format!("Cannot start command: {err}")),
        };
        let capture = Arc::new(Mutex::new(Capture::default()));
        // Keep more than we return so truncation can preserve the head as well.
        let cap = MAX_OUTPUT_BYTES * 8;
        let stdout = tokio::spawn(pump(
            child.stdout.take().expect("piped"),
            capture.clone(),
            ctx.output.clone(),
            cap,
        ));
        let stderr = tokio::spawn(pump(
            child.stderr.take().expect("piped"),
            capture.clone(),
            ctx.output.clone(),
            cap,
        ));

        enum End {
            Exited(Option<i32>),
            TimedOut,
            Cancelled,
        }
        let end = tokio::select! {
            status = child.wait() => End::Exited(status.ok().and_then(|s| s.code())),
            _ = tokio::time::sleep(timeout) => End::TimedOut,
            _ = ctx.cancel.cancelled() => End::Cancelled,
        };
        if !matches!(end, End::Exited(_)) {
            kill_tree(&mut child);
            let _ = child.wait().await;
        }
        // A grandchild may hold the pipes open after the shell exits; do not
        // wait for it forever.
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = stdout.await;
            let _ = stderr.await;
        })
        .await;

        let (text, dropped) = {
            let capture = capture.lock().unwrap();
            (capture.text.clone(), capture.dropped)
        };
        let mut body = String::new();
        if dropped > 0 {
            body.push_str(&format!("[... {dropped} bytes of earlier output discarded ...]\n"));
        }
        body.push_str(&truncate_middle(text.trim_end(), MAX_OUTPUT_BYTES));

        let (content, is_error, exit_code) = match end {
            End::Exited(Some(0)) => (
                if body.is_empty() {
                    "(no output)".to_string()
                } else {
                    body
                },
                false,
                Some(0),
            ),
            End::Exited(Some(code)) => (
                format!("{body}\n[exit code {code}]").trim_start().to_string(),
                true,
                Some(code),
            ),
            End::Exited(None) => (
                format!("{body}\n[terminated by a signal]").trim_start().to_string(),
                true,
                None,
            ),
            End::TimedOut => (
                format!(
                    "{body}\n[timed out after {}s and was stopped; pass a larger timeout_ms, or use \
                     background for long-running processes]",
                    timeout.as_secs()
                )
                .trim_start()
                .to_string(),
                true,
                None,
            ),
            End::Cancelled => (
                format!("{body}\n[stopped by the user]").trim_start().to_string(),
                true,
                None,
            ),
        };
        ToolOutput {
            content,
            is_error,
            display: None,
        }
        .with_display(ToolDisplay::Command {
            command: args.command,
            exit_code,
            background_id: None,
        })
    }
}

pub struct BashOutput;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessArgs {
    id: String,
}

#[async_trait]
impl Tool for BashOutput {
    fn name(&self) -> &str {
        "bash_output"
    }

    fn description(&self) -> String {
        "Read the output a background command (started with bash and `background: true`) has \
         produced since the last time you checked, and whether it is still running."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "string", "description": "Background process id, e.g. `bg1`."}},
            "required": ["id"]
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, input: &Value) -> String {
        arg_str(input, "id").to_string()
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: ProcessArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        match ctx.shell.poll(&args.id) {
            Ok(output) => ToolOutput::ok(output),
            Err(message) => ToolOutput::error(message),
        }
    }
}

pub struct KillProcess;

#[async_trait]
impl Tool for KillProcess {
    fn name(&self) -> &str {
        "kill_process"
    }

    fn description(&self) -> String {
        "Stop a background command started with bash and `background: true`, including its child processes.".to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "string", "description": "Background process id, e.g. `bg1`."}},
            "required": ["id"]
        })
    }

    fn kind(&self) -> ToolKind {
        // Only ever affects processes the agent itself started.
        ToolKind::Meta
    }

    fn summarize(&self, input: &Value) -> String {
        arg_str(input, "id").to_string()
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let args: ProcessArgs = match parse_input(input) {
            Ok(args) => args,
            Err(output) => return output,
        };
        match ctx.shell.kill(&args.id) {
            Ok(output) => ToolOutput::ok(output),
            Err(message) => ToolOutput::error(message),
        }
    }
}

/// Behaviour that must hold in whichever shell the machine has, Windows included.
#[cfg(test)]
mod portable_tests {
    use super::*;

    #[tokio::test]
    async fn quoted_arguments_reach_the_program_intact() {
        let dir = tempfile::tempdir().unwrap();
        let out = Bash
            .run(json!({"command": "echo \"a b\""}), &ToolContext::new(dir.path()))
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a b"), "{}", out.content);
        assert!(
            !out.content.contains('\\'),
            "quotes must not arrive escaped: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn runs_in_a_canonical_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        // The raw canonical form: verbatim (`\\?\C:\...`) on Windows.
        let cwd = dir.path().canonicalize().unwrap();
        let list = if shell().posix { "ls" } else { "dir /b" };
        let out = Bash.run(json!({"command": list}), &ToolContext::new(&cwd)).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("marker.txt"), "{}", out.content);
    }

    #[tokio::test]
    async fn timeout_stops_the_command_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let wait = if shell().posix {
            "sleep 30"
        } else {
            "ping -n 30 127.0.0.1"
        };
        let started = Instant::now();
        let out = Bash
            .run(
                json!({"command": wait, "timeout_ms": 1000}),
                &ToolContext::new(dir.path()),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("timed out after 1s"), "{}", out.content);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the command was not stopped"
        );
    }

    #[test]
    fn verbatim_prefixes_are_stripped() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\work\app")), Path::new(r"C:\work\app"));
        assert_eq!(
            plain_path(Path::new(r"\\?\UNC\host\share\app")),
            Path::new(r"\\host\share\app")
        );
        assert_eq!(plain_path(Path::new("/home/me/app")), Path::new("/home/me/app"));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn ctx(dir: &tempfile::TempDir) -> ToolContext {
        ToolContext::new(dir.path())
    }

    #[tokio::test]
    async fn runs_in_cwd_and_reports_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let ok = Bash.run(json!({"command": "ls && echo err >&2"}), &ctx(&dir)).await;
        assert!(!ok.is_error, "{}", ok.content);
        assert!(ok.content.contains("marker.txt"));
        assert!(ok.content.contains("err"), "stderr is captured too");
        assert!(matches!(
            ok.display,
            Some(ToolDisplay::Command { exit_code: Some(0), .. })
        ));

        let failed = Bash.run(json!({"command": "echo boom; exit 3"}), &ctx(&dir)).await;
        assert!(failed.is_error);
        assert_eq!(failed.content, "boom\n[exit code 3]");

        let quiet = Bash.run(json!({"command": "true"}), &ctx(&dir)).await;
        assert_eq!(quiet.content, "(no output)");
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        // The backgrounded grandchild would keep running if only the shell was killed.
        let out = Bash
            .run(
                json!({"command": "(sleep 30; touch survived) & echo started; sleep 30", "timeout_ms": 1000}),
                &ctx(&dir),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("started"));
        assert!(out.content.contains("timed out after 1s"));
        assert!(started.elapsed() < Duration::from_secs(10));
        let leftovers = std::process::Command::new("pgrep")
            .args(["-f", "touch survived"])
            .output();
        if let Ok(output) = leftovers {
            assert!(output.stdout.is_empty(), "grandchild still alive");
        }
    }

    #[tokio::test]
    async fn cancel_stops_a_running_command() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        });
        let started = Instant::now();
        let out = Bash.run(json!({"command": "sleep 30"}), &ctx).await;
        assert!(out.content.contains("stopped by the user"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn streams_live_output_to_the_sink() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let sink_seen = seen.clone();
        let mut ctx = ctx(&dir);
        ctx.output = OutputSink::new(move |chunk| sink_seen.lock().unwrap().push_str(&chunk));
        Bash.run(json!({"command": "echo one; echo two"}), &ctx).await;
        assert_eq!(seen.lock().unwrap().as_str(), "one\ntwo\n");
    }

    #[tokio::test]
    async fn long_output_keeps_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let out = Bash.run(json!({"command": "seq 1 40000"}), &ctx(&dir)).await;
        assert!(out.content.starts_with("1\n2\n"));
        assert!(out.content.ends_with("40000"));
        assert!(out.content.contains("bytes truncated"));
        assert!(out.content.len() < MAX_OUTPUT_BYTES + 200);
    }

    #[tokio::test]
    async fn background_processes_can_be_polled_and_killed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        let started = Bash
            .run(
                json!({"command": "echo ready; sleep 30; echo never", "background": true}),
                &ctx,
            )
            .await;
        assert!(!started.is_error, "{}", started.content);
        assert!(matches!(&started.display, Some(ToolDisplay::Command { background_id: Some(id), .. }) if id == "bg1"));

        let mut first = String::new();
        for _ in 0..50 {
            first = BashOutput.run(json!({"id": "bg1"}), &ctx).await.content;
            if first.contains("ready") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(first.contains("running"), "{first}");
        assert!(first.contains("ready"), "{first}");
        let second = BashOutput.run(json!({"id": "bg1"}), &ctx).await.content;
        assert!(
            second.contains("(no new output)"),
            "output is only returned once: {second}"
        );
        assert_eq!(ctx.shell.running().len(), 1);

        let killed = KillProcess.run(json!({"id": "bg1"}), &ctx).await;
        assert!(killed.content.contains("Stopped bg1"));
        for _ in 0..50 {
            if ctx.shell.running().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ctx.shell.running().is_empty());
        assert!(
            BashOutput
                .run(json!({"id": "bg1"}), &ctx)
                .await
                .content
                .contains("terminated by a signal")
        );
        assert!(BashOutput.run(json!({"id": "bg9"}), &ctx).await.is_error);
    }
}
