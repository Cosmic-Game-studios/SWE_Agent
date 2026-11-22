//! The HTTP API end to end: a real server on a local port, a real client.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use swe_core::{Paths, SessionManager};
use swe_llm::mock::MockProvider;
use swe_server::{ServeOptions, router};

struct TestServer {
    base: String,
    client: reqwest::Client,
    _home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

async fn start(options: ServeOptions) -> TestServer {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let manager = SessionManager::open(Paths::at(home.path())).unwrap();
    manager.registry().inject("mock", Arc::new(MockProvider::demo("mock").with_chunk_delay(Duration::ZERO)));
    let app = router(manager, &options);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestServer { base: format!("http://{address}"), client: reqwest::Client::new(), _home: home, workspace }
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = self.client.get(self.url(path)).send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> (u16, Value) {
        let response = self.client.request(method, self.url(path)).json(&body).send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, body).await
    }

    async fn create_session(&self, mode: &str) -> String {
        let (status, session) = self
            .post(
                "/api/sessions",
                json!({"workspace": self.workspace.path(), "provider": "mock", "model": "demo", "mode": mode}),
            )
            .await;
        assert_eq!(status, 200, "{session}");
        session["id"].as_str().unwrap().to_string()
    }

    /// Open the session's event stream and return events as they arrive.
    async fn events(&self, id: &str, after: u64) -> EventReader {
        let response = self.client.get(self.url(&format!("/api/sessions/{id}/events?after={after}"))).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
        EventReader { body: Box::pin(response.bytes_stream()), buffer: String::new() }
    }
}

struct EventReader {
    body: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buffer: String,
}

impl EventReader {
    /// Next event as `(sse id, payload)`.
    async fn next(&mut self) -> (Option<u64>, Value) {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let frame: String = self.buffer.drain(..end + 2).collect();
                let mut id = None;
                let mut data = None;
                for line in frame.lines() {
                    if let Some(value) = line.strip_prefix("id:") {
                        id = value.trim().parse().ok();
                    } else if let Some(value) = line.strip_prefix("data:") {
                        data = serde_json::from_str(value.trim()).ok();
                    }
                }
                if let Some(data) = data {
                    return (id, data);
                }
                continue; // keep-alive comment
            }
            let chunk = tokio::time::timeout(Duration::from_secs(20), self.body.next())
                .await
                .expect("timed out waiting for an event")
                .expect("stream ended")
                .unwrap();
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    async fn until(&mut self, kind: &str) -> Vec<(Option<u64>, Value)> {
        let mut seen = Vec::new();
        loop {
            let event = self.next().await;
            let done = event.1["type"] == kind;
            seen.push(event);
            if done {
                return seen;
            }
        }
    }
}

#[tokio::test]
async fn a_session_runs_over_http_with_live_events_and_replay() {
    let server = start(ServeOptions::default()).await;
    assert_eq!(server.get("/api/health").await.1["ok"], true);

    let id = server.create_session("full_auto").await;
    let mut live = server.events(&id, 0).await;
    let (status, sent) = server
        .post(
            &format!("/api/sessions/{id}/messages"),
            json!({"text": "Say hi\n!say Writing.\n!tool write_file {\"path\": \"hi.txt\", \"content\": \"hi\"}\n!then\n!say Done."}),
        )
        .await;
    assert_eq!((status, &sent["queued"]), (200, &json!(false)));

    let events = live.until("turn_finished").await;
    let kinds: Vec<&str> = events.iter().map(|(_, event)| event["type"].as_str().unwrap()).collect();
    assert_eq!(kinds[0], "user_message");
    assert!(kinds.contains(&"assistant_delta") && kinds.contains(&"tool_started") && kinds.contains(&"tool_finished"));
    assert_eq!(events.last().unwrap().1["outcome"], "completed");
    assert_eq!(std::fs::read_to_string(server.workspace.path().join("hi.txt")).unwrap(), "hi");
    // Durable events carry their sequence number as the SSE id; transient ones none.
    for (sse_id, event) in &events {
        let transient = matches!(event["type"].as_str().unwrap(), "assistant_delta" | "reasoning_delta" | "tool_call_streaming" | "tool_output_delta");
        assert_eq!(sse_id.is_none(), transient, "{event}");
        if let Some(sse_id) = sse_id {
            assert_eq!(event["seq"], *sse_id);
        }
    }

    // The snapshot holds the durable history; replay after N returns the rest.
    let (_, snapshot) = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(snapshot["meta"]["status"], "idle");
    assert_eq!(snapshot["meta"]["title"], "Say hi");
    let durable = snapshot["events"].as_array().unwrap();
    assert_eq!(durable.len(), events.iter().filter(|(sse_id, _)| sse_id.is_some()).count());
    let last_seq = snapshot["meta"]["last_seq"].as_u64().unwrap();
    let mut replay = server.events(&id, last_seq - 2).await;
    assert_eq!(replay.next().await.0, Some(last_seq - 1));
    assert_eq!(replay.next().await.0, Some(last_seq));

    // Reconnecting clients resume from Last-Event-ID.
    let resumed = server
        .client
        .get(server.url(&format!("/api/sessions/{id}/events")))
        .header("last-event-id", (last_seq - 1).to_string())
        .send()
        .await
        .unwrap();
    let mut resumed = EventReader { body: Box::pin(resumed.bytes_stream()), buffer: String::new() };
    assert_eq!(resumed.next().await.0, Some(last_seq));

    // Listing, renaming, changes in the workspace, deleting.
    let (_, sessions) = server.get("/api/sessions").await;
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    let (_, renamed) = server.send(reqwest::Method::PATCH, &format!("/api/sessions/{id}"), json!({"title": "Renamed", "mode": "plan"})).await;
    assert_eq!((renamed["title"].as_str(), renamed["mode"].as_str()), (Some("Renamed"), Some("plan")));
    let (_, diff) = server.get(&format!("/api/sessions/{id}/diff")).await;
    assert_eq!(diff["is_git"], false);
    let (status, _) = server.send(reqwest::Method::DELETE, &format!("/api/sessions/{id}"), json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(server.get(&format!("/api/sessions/{id}")).await.0, 404);
}

#[tokio::test]
async fn approvals_are_answered_over_http() {
    let server = start(ServeOptions::default()).await;
    let id = server.create_session("ask").await;
    let mut live = server.events(&id, 0).await;
    server
        .post(&format!("/api/sessions/{id}/messages"), json!({"text": "!tool write_file {\"path\": \"a.txt\", \"content\": \"A\"}\n!then\n!say ok"}))
        .await;
    let events = live.until("approval_requested").await;
    let request = &events.last().unwrap().1;
    assert_eq!(request["name"], "write_file");
    assert_eq!(request["always_label"], "Always allow `write_file`");
    let request_id = request["request_id"].as_str().unwrap();
    assert_eq!(server.get(&format!("/api/sessions/{id}")).await.1["meta"]["status"], "waiting_approval");

    let path = format!("/api/sessions/{id}/approvals/{request_id}");
    assert_eq!(server.post(&path, json!({"decision": "allow"})).await.0, 200);
    assert_eq!(server.post(&path, json!({"decision": "allow"})).await.0, 409, "already answered");
    live.until("turn_finished").await;
    assert!(server.workspace.path().join("a.txt").exists());

    // A message sent while a turn runs is queued, and cancel stops the turn.
    server.post(&format!("/api/sessions/{id}/messages"), json!({"text": "!tool bash {\"command\": \"sleep 30\"}"})).await;
    live.until("approval_requested").await;
    let (_, queued) = server.post(&format!("/api/sessions/{id}/messages"), json!({"text": "later"})).await;
    assert_eq!(queued["queued"], true);
    assert_eq!(server.post(&format!("/api/sessions/{id}/cancel"), json!({})).await.0, 200);
    let events = live.until("turn_finished").await;
    assert_eq!(events.last().unwrap().1["outcome"], "cancelled");
}

#[tokio::test]
async fn providers_config_and_workspace_endpoints() {
    let server = start(ServeOptions::default()).await;

    let (_, providers) = server.get("/api/providers").await;
    let ids: Vec<&str> = providers.as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    for expected in ["anthropic", "openai", "google", "openrouter", "mock"] {
        assert!(ids.contains(&expected), "{expected} missing from {ids:?}");
    }
    let (status, updated) = server.send(reqwest::Method::PUT, "/api/providers/openrouter/key", json!({"key": "sk-or-test"})).await;
    assert_eq!(status, 200);
    assert!(matches!(updated["key"]["source"].as_str(), Some("stored" | "env")));
    assert!(!updated.to_string().contains("sk-or-test"), "keys are never echoed back");
    assert_eq!(server.send(reqwest::Method::PUT, "/api/providers/nope/key", json!({"key": "x"})).await.0, 400);

    let (_, models) = server.get("/api/providers/mock/models").await;
    assert_eq!(models[0]["id"], "demo");
    assert_eq!(server.get("/api/providers/unknown/models").await.0, 400);

    let (status, config) = server.send(reqwest::Method::PATCH, "/api/config", json!({"default_provider": "mock", "default_model": "demo", "default_mode": "auto_edit"})).await;
    assert_eq!(status, 200);
    assert_eq!(config["default_mode"], "auto_edit");
    assert_eq!(server.get("/api/config").await.1["default_model"], "demo");
    assert_eq!(server.send(reqwest::Method::PATCH, "/api/config", json!({"no_such_setting": 1})).await.0, 400);
    assert_eq!(server.send(reqwest::Method::PATCH, "/api/config", json!({"default_mode": "bogus"})).await.0, 400);
    // With defaults configured, a workspace is all a new session needs.
    let (status, session) = server.post("/api/sessions", json!({"workspace": server.workspace.path()})).await;
    assert_eq!((status, session["model"].as_str(), session["mode"].as_str()), (200, Some("demo"), Some("auto_edit")));
    assert_eq!(server.post("/api/sessions", json!({"workspace": "/no/such/dir"})).await.0, 400);

    let (_, meta) = server.get("/api/meta").await;
    assert_eq!(meta["engines"][0]["id"], "native");
    assert!(meta["profiles"].as_array().unwrap().iter().any(|p| p["id"] == "orchestrator"));

    // Directory browser and git diff.
    std::fs::create_dir(server.workspace.path().join("src")).unwrap();
    std::fs::create_dir(server.workspace.path().join(".hidden")).unwrap();
    std::fs::write(server.workspace.path().join("file.txt"), "x").unwrap();
    let workspace = server.workspace.path().to_string_lossy().into_owned();
    let response = server.client.get(server.url("/api/fs/list")).query(&[("path", workspace.as_str())]).send().await.unwrap();
    let listing: Value = response.json().await.unwrap();
    let names: Vec<&str> = listing["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["src"], "directories only, hidden ones excluded");
    assert!(listing["parent"].is_string());
    assert_eq!(server.get("/api/fs/list?path=/no/such/dir").await.0, 404);

    let run = |args: &[&str]| {
        let ok = std::process::Command::new("git").arg("-C").arg(server.workspace.path()).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?}");
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "T"]);
    run(&["config", "commit.gpgsign", "false"]);
    run(&["add", "file.txt"]);
    run(&["commit", "-q", "-m", "init"]);
    std::fs::write(server.workspace.path().join("file.txt"), "changed\n").unwrap();
    std::fs::write(server.workspace.path().join("src/new.rs"), "fn main() {}\n").unwrap();
    let id = session["id"].as_str().unwrap();
    let (_, diff) = server.get(&format!("/api/sessions/{id}/diff")).await;
    assert_eq!((diff["is_git"].as_bool(), diff["branch"].as_str()), (Some(true), Some("main")));
    let text = diff["diff"].as_str().unwrap();
    assert!(text.contains("+changed") && text.contains("-x"), "{text}");
    assert!(text.contains("+++ b/src/new.rs") && text.contains("+fn main() {}"), "untracked files are included: {text}");
}

#[tokio::test]
async fn the_api_rejects_foreign_hosts_origins_and_missing_tokens() {
    let server = start(ServeOptions::default()).await;
    let get = |host: Option<&'static str>, origin: Option<&'static str>| {
        let mut request = server.client.get(server.url("/api/sessions"));
        if let Some(host) = host {
            request = request.header("host", host);
        }
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        async move { request.send().await.unwrap().status().as_u16() }
    };
    assert_eq!(get(None, None).await, 200);
    // DNS rebinding: the browser thinks it talks to evil.example.
    assert_eq!(get(Some("evil.example:4477"), None).await, 403);
    // A page on another site making the browser call the local API.
    assert_eq!(get(None, Some("https://evil.example")).await, 403);
    assert_eq!(get(None, Some("http://localhost:5173")).await, 403);
    // The UI itself (same origin) is fine.
    let own_origin: &'static str = Box::leak(server.base.clone().into_boxed_str());
    assert_eq!(get(None, Some(own_origin)).await, 200);
    let post = server.client.post(server.url("/api/sessions")).header("origin", "https://evil.example").json(&json!({})).send().await.unwrap();
    assert_eq!(post.status(), 403);

    // A configured dev origin is allowed.
    let dev = start(ServeOptions { dev_origin: Some("http://localhost:5173/".into()), ..Default::default() }).await;
    let allowed = dev.client.get(dev.url("/api/sessions")).header("origin", "http://localhost:5173").send().await.unwrap();
    assert_eq!(allowed.status(), 200);

    // With a token, API calls need it (header or query); the UI shell does not.
    let secured = start(ServeOptions { token: Some("s3cret".into()), ..Default::default() }).await;
    assert_eq!(secured.get("/api/sessions").await.0, 401);
    assert_eq!(secured.get("/api/sessions?token=wrong").await.0, 401);
    assert_eq!(secured.get("/api/sessions?token=s3cret").await.0, 200);
    let with_header = secured.client.get(secured.url("/api/sessions")).bearer_auth("s3cret").send().await.unwrap();
    assert_eq!(with_header.status(), 200);
    assert_eq!(secured.client.get(secured.url("/")).send().await.unwrap().status(), 200);
}

#[tokio::test]
async fn the_ui_shell_is_served_for_every_non_api_path() {
    let server = start(ServeOptions::default()).await;
    for path in ["/", "/sessions/abc", "/index.html"] {
        let response = server.client.get(server.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        assert!(response.headers()["content-type"].to_str().unwrap().starts_with("text/html"), "{path}");
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert!(response.text().await.unwrap().contains("SWE Agent"));
    }
    let (status, body) = server.get("/api/does-not-exist").await;
    assert_eq!(status, 404);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn serving_off_loopback_requires_a_token() {
    let home = tempfile::tempdir().unwrap();
    let manager = SessionManager::open(Paths::at(home.path())).unwrap();
    let error = swe_server::serve(manager, ServeOptions { host: "0.0.0.0".parse().unwrap(), port: 0, ..Default::default() })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("without an access token"));
}
