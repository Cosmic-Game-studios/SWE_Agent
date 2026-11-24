//! HTTP API, live event streams and the embedded web UI.
//!
//! The server is a thin layer over [`SessionManager`]: JSON endpoints for
//! commands, Server-Sent Events for everything that happens. A client that
//! reconnects passes the last sequence number it saw and gets exactly the
//! events it missed, so the UI can come and go while agents keep working.
//!
//! This process can run shell commands on the user's machine, so the API is
//! guarded: by default it only listens on loopback, rejects requests whose
//! `Host` is not a loopback name (DNS rebinding) and requests coming from
//! another web origin (cross-site requests from a page in the browser).
//! Listening on any other interface requires an access token.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use swe_core::{CreateSession, SessionError, SessionManager, UpdateSession, UserInput};
use swe_llm::LlmError;
use swe_protocol::{ApprovalDecision, Event};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

mod workspace;

#[derive(rust_embed::RustEmbed)]
#[folder = "$SWE_UI_DIR"]
struct Ui;

#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub host: IpAddr,
    pub port: u16,
    /// Required for every API call when set. Mandatory off loopback.
    pub token: Option<String>,
    /// Extra web origin allowed to call the API (a UI dev server).
    pub dev_origin: Option<String>,
    pub open_browser: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        ServeOptions {
            host: IpAddr::from([127, 0, 0, 1]),
            port: 4477,
            token: None,
            dev_origin: None,
            open_browser: false,
        }
    }
}

#[derive(Clone)]
struct AppState {
    manager: SessionManager,
    token: Option<Arc<str>>,
    dev_origin: Option<Arc<str>>,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

impl From<SessionError> for ApiError {
    fn from(err: SessionError) -> Self {
        let status = match &err {
            SessionError::NotFound(_) => StatusCode::NOT_FOUND,
            SessionError::Invalid(_) => StatusCode::BAD_REQUEST,
            SessionError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError::new(status, err.to_string())
    }
}

impl From<LlmError> for ApiError {
    fn from(err: LlmError) -> Self {
        let status = match &err {
            LlmError::Auth(_) => StatusCode::UNAUTHORIZED,
            LlmError::Config(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };
        ApiError::new(status, err.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Build the application router (API plus UI).
pub fn router(manager: SessionManager, options: &ServeOptions) -> Router {
    let state = AppState {
        manager,
        token: options.token.as_deref().map(Arc::from),
        dev_origin: options
            .dev_origin
            .as_deref()
            .map(|origin| Arc::from(origin.trim_end_matches('/'))),
    };
    let api = Router::new()
        .route("/health", get(health))
        .route("/meta", get(meta))
        .route("/config", get(get_config).patch(patch_config))
        .route("/providers", get(list_providers))
        .route("/providers/{id}/key", put(set_provider_key))
        .route("/providers/{id}/models", get(list_models))
        .route("/mcp", get(mcp_status))
        .route("/fs/list", get(workspace::list_directories))
        .route("/events", get(global_events))
        .route("/sessions", get(list_sessions).post(create_session))
        .route(
            "/sessions/{id}",
            get(get_session).patch(update_session).delete(delete_session),
        )
        .route("/sessions/{id}/messages", post(send_message))
        .route("/sessions/{id}/cancel", post(cancel_session))
        .route("/sessions/{id}/approvals/{request_id}", post(resolve_approval))
        .route("/sessions/{id}/events", get(session_events))
        .route("/sessions/{id}/diff", get(session_diff))
        .route("/sessions/{id}/checkpoints", get(list_checkpoints))
        .route("/sessions/{id}/rewind", post(rewind_session))
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "no such endpoint") });
    Router::new()
        .nest("/api", api)
        .fallback(static_file)
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Serve until interrupted (Ctrl-C), then stop every agent and process.
pub async fn serve(manager: SessionManager, options: ServeOptions) -> anyhow::Result<()> {
    if !options.host.is_loopback() && options.token.is_none() {
        anyhow::bail!(
            "refusing to listen on {} without an access token: anyone who can reach this port could run \
             commands on this machine. Pass --token <secret>, or listen on 127.0.0.1.",
            options.host
        );
    }
    let app = router(manager.clone(), &options);
    let address = SocketAddr::new(options.host, options.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|err| anyhow::anyhow!("cannot listen on {address}: {err}"))?;
    let local = listener.local_addr()?;
    let shown_host = if options.host.is_unspecified() {
        "localhost".to_string()
    } else {
        local.ip().to_string()
    };
    let mut url = format!(
        "http://{}:{}/",
        if local.ip().is_ipv6() {
            format!("[{shown_host}]")
        } else {
            shown_host
        },
        local.port()
    );
    if let Some(token) = &options.token {
        url.push_str(&format!("?token={token}"));
    }
    println!("SWE Agent is running at {url}");
    if options.open_browser {
        open_in_browser(&url);
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    manager.shutdown();
    Ok(())
}

fn open_in_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ------------------------------------------------------------- guard

fn host_name(host: &str) -> &str {
    // `[::1]:4477` -> `[::1]`, `localhost:4477` -> `localhost`
    match host.rfind(':') {
        Some(colon) if !host[colon..].contains(']') => &host[..colon],
        _ => host,
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(
        host_name(host).to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "[::1]"
    )
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    // Without a token the server is loopback-only, so any other Host means a
    // page tricked the browser into resolving its own name to 127.0.0.1.
    if state.token.is_none() && !is_loopback_host(&host) {
        return ApiError::new(StatusCode::FORBIDDEN, "unexpected Host header").into_response();
    }
    // Browsers attach Origin to cross-site requests; only our own page (and
    // an explicitly allowed dev server) may call the API.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let same_origin = origin.split_once("://").is_some_and(|(_, authority)| authority == host);
        let allowed_dev = state.dev_origin.as_deref().is_some_and(|dev| dev == origin);
        if !same_origin && !allowed_dev {
            return ApiError::new(StatusCode::FORBIDDEN, "cross-origin requests are not allowed").into_response();
        }
    }
    if let Some(expected) = state.token.as_deref()
        && request.uri().path().starts_with("/api/")
    {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        // EventSource cannot set headers, so the token may also come as a query parameter.
        let query = request
            .uri()
            .query()
            .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("token=")));
        let presented = bearer.or(query).unwrap_or_default();
        if !constant_time_eq(presented, expected) {
            return ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid access token").into_response();
        }
    }
    next.run(request).await
}

// ---------------------------------------------------------- handlers

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}))
}

async fn meta(State(state): State<AppState>) -> Json<Value> {
    let profiles: Vec<Value> = swe_core::prompt::ROOT_PROFILES
        .iter()
        .map(|(id, description)| json!({"id": id, "description": description}))
        .collect();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "home": state.manager.paths().home,
        "cwd": std::env::current_dir().ok(),
        "user_home": std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from),
        "profiles": profiles,
        "engines": swe_core::engines(),
    }))
}

async fn get_config(State(state): State<AppState>) -> Json<swe_core::Config> {
    Json(state.manager.config())
}

async fn patch_config(State(state): State<AppState>, Json(patch): Json<Value>) -> ApiResult<Json<swe_core::Config>> {
    let Value::Object(patch) = patch else {
        return Err(ApiError::bad_request("expected a JSON object"));
    };
    let mut current = serde_json::to_value(state.manager.config())
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let fields = current.as_object_mut().expect("config serialises to an object");
    for (key, value) in patch {
        if !fields.contains_key(&key) {
            return Err(ApiError::bad_request(format!("unknown setting `{key}`")));
        }
        fields.insert(key, value);
    }
    let config: swe_core::Config =
        serde_json::from_value(current).map_err(|err| ApiError::bad_request(format!("invalid settings: {err}")))?;
    state
        .manager
        .set_config(config.clone())
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(config))
}

async fn list_providers(State(state): State<AppState>) -> Json<Vec<swe_core::ProviderInfo>> {
    Json(state.manager.registry().list())
}

#[derive(Deserialize)]
struct KeyBody {
    key: Option<String>,
}

async fn set_provider_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<KeyBody>,
) -> ApiResult<Json<swe_core::ProviderInfo>> {
    state
        .manager
        .registry()
        .set_key(&id, body.key.as_deref())
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    state
        .manager
        .registry()
        .list()
        .into_iter()
        .find(|provider| provider.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown provider"))
}

#[derive(Deserialize)]
struct ModelsQuery {
    #[serde(default)]
    refresh: bool,
}

async fn list_models(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ModelsQuery>,
) -> ApiResult<Json<Vec<swe_llm::ModelInfo>>> {
    Ok(Json(state.manager.registry().models(&id, query.refresh).await?))
}

async fn mcp_status(State(state): State<AppState>) -> Json<Vec<swe_core::mcp::McpServerStatus>> {
    Json(state.manager.mcp_status().await)
}

async fn list_sessions(State(state): State<AppState>) -> Json<Vec<swe_protocol::SessionMeta>> {
    Json(state.manager.list())
}

async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<CreateSession>,
) -> ApiResult<Json<swe_protocol::SessionMeta>> {
    Ok(Json(state.manager.create(request)?))
}

async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<swe_core::SessionSnapshot>> {
    Ok(Json(state.manager.get(&id)?.snapshot()))
}

async fn update_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(update): Json<UpdateSession>,
) -> ApiResult<Json<swe_protocol::SessionMeta>> {
    Ok(Json(state.manager.update(&id, update)?))
}

async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    state.manager.delete(&id)?;
    Ok(Json(json!({"ok": true})))
}

async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<UserInput>,
) -> ApiResult<Json<Value>> {
    let queued = state.manager.send_message(&id, input)?;
    Ok(Json(json!({"queued": queued})))
}

async fn list_checkpoints(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<swe_core::Checkpoint>>> {
    Ok(Json(state.manager.checkpoints(&id)?))
}

#[derive(Deserialize)]
struct RewindBody {
    /// Sequence number of the user message to rewind the files to.
    seq: u64,
}

async fn rewind_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RewindBody>,
) -> ApiResult<Json<Value>> {
    let restored = state.manager.rewind(&id, body.seq)?;
    Ok(Json(json!({"restored": restored})))
}

async fn cancel_session(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    state.manager.cancel(&id)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ApprovalBody {
    decision: ApprovalDecision,
    #[serde(default)]
    feedback: Option<String>,
}

async fn resolve_approval(
    State(state): State<AppState>,
    Path((id, request_id)): Path<(String, String)>,
    Json(body): Json<ApprovalBody>,
) -> ApiResult<Json<Value>> {
    let session = state.manager.get(&id)?;
    if session.resolve_approval(&request_id, body.decision, body.feedback) {
        Ok(Json(json!({"ok": true})))
    } else {
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "this approval is no longer pending",
        ))
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<u64>,
}

fn to_sse(event: &Event) -> SseEvent {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    let sse = SseEvent::default().data(data);
    // Only durable events advance the resume position.
    if event.kind.is_transient() {
        sse
    } else {
        sse.id(event.seq.to_string())
    }
}

async fn session_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    let session = state.manager.get(&id)?;
    // A reconnecting EventSource reports where it was via Last-Event-ID.
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let after = resume.or(query.after).unwrap_or(0);
    let (replay, receiver) = session.subscribe(after);
    let live = BroadcastStream::new(receiver).map(|item| match item {
        Ok(event) => to_sse(&event),
        // The client fell too far behind; it reconnects from its last id.
        Err(BroadcastStreamRecvError::Lagged(missed)) => SseEvent::default().event("resync").data(missed.to_string()),
    });
    let replayed: Vec<SseEvent> = replay.iter().map(to_sse).collect();
    let stream = stream::iter(replayed).chain(live).map(Ok);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn global_events(State(state): State<AppState>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(state.manager.subscribe_global()).map(|item| {
        Ok(match item {
            Ok(event) => SseEvent::default().data(serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string())),
            Err(BroadcastStreamRecvError::Lagged(missed)) => {
                SseEvent::default().event("resync").data(missed.to_string())
            }
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn session_diff(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<workspace::WorkspaceDiff>> {
    let session = state.manager.get(&id)?;
    Ok(Json(workspace::diff(&session.workspace()).await))
}

// ------------------------------------------------------------ static

async fn static_file(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (file, name) = match Ui::get(path) {
        Some(file) if !path.is_empty() => (file, path),
        // Client-side routes all resolve to the app shell.
        _ => match Ui::get("index.html") {
            Some(file) => (file, "index.html"),
            None => {
                return ApiError::new(StatusCode::NOT_FOUND, "the web UI is not part of this build").into_response();
            }
        },
    };
    let mime = mime_guess::from_path(FsPath::new(name)).first_or_octet_stream();
    // Bundled assets carry a content hash in their name; the shell must stay fresh.
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_recognised_with_and_without_ports() {
        for host in [
            "localhost",
            "localhost:4477",
            "127.0.0.1:80",
            "[::1]:4477",
            "[::1]",
            "LOCALHOST:1",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:4477",
            "127.0.0.1.evil.example",
            "192.168.1.5:4477",
            "",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn token_comparison_needs_an_exact_match() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
        assert!(!constant_time_eq("secret", "secret2"));
        assert!(!constant_time_eq("", "secret"));
    }
}
