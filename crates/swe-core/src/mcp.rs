//! Model Context Protocol client (stdio transport).
//!
//! MCP servers listed in `config.toml` are started on first use and their
//! tools are offered to the agent next to the built-in ones, named
//! `mcp__<server>__<tool>`:
//!
//! ```toml
//! [mcp_servers.github]
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-github"]
//! env = { GITHUB_TOKEN = "..." }
//! ```
//!
//! The implementation covers what an agent needs: the initialize handshake,
//! `tools/list` and `tools/call`. Requests a server sends to the client
//! (sampling, roots, elicitation) are answered with "method not found".

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use swe_protocol::ToolKind;
use swe_tools::{Tool, ToolContext, ToolOutput};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

const PROTOCOL_VERSION: &str = "2025-06-18";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub running: bool,
    pub tools: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// One running MCP server.
struct Client {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    child: Mutex<Child>,
}

impl Client {
    async fn start(config: &McpServerConfig) -> Result<Arc<Self>, String> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| format!("cannot start `{}`: {err}", config.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let client = Arc::new(Client {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            child: Mutex::new(child),
        });

        // Route responses to their waiters; answer server-initiated requests.
        let reader = client.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let id = message.get("id").cloned();
                if message.get("method").is_some() {
                    if let Some(id) = id {
                        let reply = if message["method"] == "ping" {
                            json!({"jsonrpc": "2.0", "id": id, "result": {}})
                        } else {
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})
                        };
                        let _ = reader.write(&reply).await;
                    }
                    continue;
                }
                let Some(id) = id.and_then(|id| id.as_u64()) else {
                    continue;
                };
                if let Some(waiter) = reader.pending.lock().unwrap().remove(&id) {
                    let result = match message.get("error") {
                        Some(error) => Err(error["message"].as_str().unwrap_or("MCP error").to_string()),
                        None => Ok(message["result"].clone()),
                    };
                    let _ = waiter.send(result);
                }
            }
            // The server went away: fail everything still waiting.
            for (_, waiter) in reader.pending.lock().unwrap().drain() {
                let _ = waiter.send(Err("the MCP server exited".to_string()));
            }
        });

        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "swe-agent", "version": env!("CARGO_PKG_VERSION")},
                }),
                STARTUP_TIMEOUT,
            )
            .await?;
        if !init.is_object() {
            return Err("unexpected initialize response".to_string());
        }
        client
            .write(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        Ok(client)
    }

    async fn write(&self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|err| format!("cannot write to the MCP server: {err}"))?;
        stdin
            .flush()
            .await
            .map_err(|err| format!("cannot write to the MCP server: {err}"))
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(err) = self
            .write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await
        {
            self.pending.lock().unwrap().remove(&id);
            return Err(err);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the MCP server exited".to_string()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("`{method}` timed out after {}s", timeout.as_secs()))
            }
        }
    }

    async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let page = self.request("tools/list", params, STARTUP_TIMEOUT).await?;
            tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
            match page["nextCursor"].as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(tools)
    }

    fn kill(&self) {
        let _ = self.child.lock().unwrap().start_kill();
    }
}

struct McpTool {
    client: Arc<Client>,
    /// The configured name of the server, which is also the tool group.
    server: String,
    /// Name exposed to the model.
    exposed: String,
    /// Name on the server.
    remote: String,
    description: String,
    schema: Value,
    read_only: bool,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn kind(&self) -> ToolKind {
        // Servers may declare a tool read-only; everything else is treated
        // as acting on the outside world and asks first.
        if self.read_only {
            ToolKind::Read
        } else {
            ToolKind::Execute
        }
    }

    fn summarize(&self, input: &Value) -> String {
        swe_tools::text::truncate_line(&input.to_string(), 160)
    }

    fn group(&self) -> Option<&str> {
        Some(&self.server)
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let call = self.client.request(
            "tools/call",
            json!({"name": self.remote, "arguments": input}),
            CALL_TIMEOUT,
        );
        let result = tokio::select! {
            result = call => result,
            _ = ctx.cancel.cancelled() => return ToolOutput::error("[stopped by the user]"),
        };
        match result {
            Err(err) => ToolOutput::error(format!("MCP call failed: {err}")),
            Ok(result) => {
                let mut text = String::new();
                for part in result["content"].as_array().into_iter().flatten() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    match part["type"].as_str().unwrap_or_default() {
                        "text" => text.push_str(part["text"].as_str().unwrap_or_default()),
                        "resource" => {
                            text.push_str(part["resource"]["text"].as_str().unwrap_or("[binary resource omitted]"))
                        }
                        other => text.push_str(&format!("[{other} content omitted]")),
                    }
                }
                if text.is_empty()
                    && let Some(structured) = result.get("structuredContent")
                {
                    text = structured.to_string();
                }
                ToolOutput {
                    content: text,
                    is_error: result["isError"] == true,
                    display: None,
                }
            }
        }
    }
}

/// Tool names must fit every provider: `[A-Za-z0-9_-]`, at most 64 characters.
fn exposed_name(server: &str, tool: &str) -> String {
    let clean = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut name = format!("mcp__{}__{}", clean(server), clean(tool));
    name.truncate(64);
    name
}

struct Started {
    tools: Vec<Arc<dyn Tool>>,
    status: Vec<McpServerStatus>,
    clients: Vec<Arc<Client>>,
}

/// All configured MCP servers of this process.
pub struct McpHub {
    configs: BTreeMap<String, McpServerConfig>,
    started: tokio::sync::OnceCell<Started>,
}

impl McpHub {
    pub fn new(configs: &BTreeMap<String, McpServerConfig>) -> Self {
        McpHub {
            configs: configs.clone(),
            started: tokio::sync::OnceCell::new(),
        }
    }

    async fn started(&self) -> &Started {
        self.started
            .get_or_init(|| async {
                let mut started = Started {
                    tools: Vec::new(),
                    status: Vec::new(),
                    clients: Vec::new(),
                };
                for (name, config) in &self.configs {
                    if config.disabled {
                        continue;
                    }
                    let outcome = async {
                        let client = Client::start(config).await?;
                        let tools = client.list_tools().await?;
                        Ok::<_, String>((client, tools))
                    };
                    match tokio::time::timeout(STARTUP_TIMEOUT * 2, outcome).await {
                        Ok(Ok((client, tools))) => {
                            for tool in &tools {
                                let Some(remote) = tool["name"].as_str() else { continue };
                                started.tools.push(Arc::new(McpTool {
                                    client: client.clone(),
                                    server: name.clone(),
                                    exposed: exposed_name(name, remote),
                                    remote: remote.to_string(),
                                    description: tool["description"].as_str().unwrap_or(remote).to_string(),
                                    schema: match &tool["inputSchema"] {
                                        schema @ Value::Object(_) => schema.clone(),
                                        _ => json!({"type": "object"}),
                                    },
                                    read_only: tool["annotations"]["readOnlyHint"] == true,
                                }));
                            }
                            started.status.push(McpServerStatus {
                                name: name.clone(),
                                running: true,
                                tools: tools.len(),
                                error: None,
                            });
                            started.clients.push(client);
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(server = %name, "MCP server failed to start: {error}");
                            started.status.push(McpServerStatus {
                                name: name.clone(),
                                running: false,
                                tools: 0,
                                error: Some(error),
                            });
                        }
                        Err(_) => {
                            tracing::warn!(server = %name, "MCP server did not finish starting in time");
                            started.status.push(McpServerStatus {
                                name: name.clone(),
                                running: false,
                                tools: 0,
                                error: Some("timed out while starting".to_string()),
                            });
                        }
                    }
                }
                started
            })
            .await
    }

    /// Tools of every server, starting the servers on first call.
    pub async fn tools(&self) -> Vec<Arc<dyn Tool>> {
        if self.configs.is_empty() {
            return Vec::new();
        }
        self.started().await.tools.clone()
    }

    pub async fn status(&self) -> Vec<McpServerStatus> {
        if self.configs.is_empty() {
            return Vec::new();
        }
        self.started().await.status.clone()
    }

    pub fn shutdown(&self) {
        if let Some(started) = self.started.get() {
            for client in &started.clients {
                client.kill();
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A minimal MCP server as a shell script: answers by matching on the request text.
    const FAKE_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id";;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo.text","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"annotations":{"readOnlyHint":true}},{"name":"fail","description":"Always fails","inputSchema":{"type":"object"}}]}}\n' "$id";;
    *'"name":"echo.text"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echoed"},{"type":"image","data":"x","mimeType":"image/png"}]}}\n' "$id";;
    *'"name":"fail"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"boom"}],"isError":true}}\n' "$id";;
  esac
done
"#;

    fn hub(command: &str, args: &[&str]) -> McpHub {
        let mut configs = BTreeMap::new();
        configs.insert(
            "fake server".to_string(),
            McpServerConfig {
                command: command.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        );
        McpHub::new(&configs)
    }

    #[tokio::test]
    async fn lists_and_calls_tools_of_a_stdio_server() {
        let hub = hub("sh", &["-c", FAKE_SERVER]);
        let tools = hub.tools().await;
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, ["mcp__fake_server__echo_text", "mcp__fake_server__fail"]);
        assert_eq!(tools[0].kind(), ToolKind::Read);
        assert_eq!(tools[1].kind(), ToolKind::Execute);
        assert_eq!(tools[0].input_schema()["properties"]["text"]["type"], "string");

        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let echoed = tools[0].run(json!({"text": "hi"}), &ctx).await;
        assert_eq!(echoed, ToolOutput::ok("echoed\n[image content omitted]"));
        let failed = tools[1].run(json!({}), &ctx).await;
        assert!(failed.is_error);
        assert_eq!(failed.content, "boom");

        assert_eq!(
            hub.status().await,
            vec![McpServerStatus {
                name: "fake server".into(),
                running: true,
                tools: 2,
                error: None
            }]
        );
        hub.shutdown();
    }

    #[tokio::test]
    async fn a_server_that_cannot_start_is_reported_not_fatal() {
        let hub = hub("/nonexistent/mcp-server", &[]);
        assert!(hub.tools().await.is_empty());
        let status = hub.status().await;
        assert!(!status[0].running);
        assert!(status[0].error.as_deref().unwrap().contains("cannot start"));

        // A process that exits right away fails the handshake instead of hanging.
        let quitter = super::tests::hub("sh", &["-c", "exit 0"]);
        assert!(quitter.tools().await.is_empty());
        assert!(quitter.status().await[0].error.is_some());
    }
}
