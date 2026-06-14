use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{
        Path, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::{get, post},
};
use serde::Serialize;
use tokio::sync::{RwLock, mpsc};
use tower_http::cors::CorsLayer;
use tracing::{debug, info};

mod agent;
mod auth;
mod config;
mod history;
mod llm;
mod prompt;
mod session;
mod tools;

// ── GitHub OAuth Device Flow ───────────────────────────

/// Get the GitHub OAuth client ID from environment variable.
/// Users must register an OAuth App and set MOTE_GITHUB_CLIENT_ID.
fn github_client_id() -> String {
    std::env::var("MOTE_GITHUB_CLIENT_ID").unwrap_or_else(|_| {
        tracing::warn!(
            "MOTE_GITHUB_CLIENT_ID not set. GitHub login will fail."
        );
        "Iv1.placeholder".to_string()
    })
}

/// OAuth scopes for GitHub Models access.
const GITHUB_SCOPES: &str = "models:read";

/// GitHub device authorization endpoint.
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// GitHub OAuth token endpoint.
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Status of an active device login flow.
#[derive(Debug, Clone)]
enum FlowStatus {
    Pending,
    Completed,
    Failed(String), // error message
}

/// An active device login flow tracked by the server.
struct DeviceFlow {
    status: Arc<tokio::sync::Mutex<FlowStatus>>,
}

// ── App state shared across all handlers ─────────────────

struct AppState {
    config: config::Config,
    /// Runtime-updatable auth (reloaded after credential save).
    auth: RwLock<auth::Auth>,
    /// Merged agents from config.toml + separate files (file agents lower priority).
    merged_agents: HashMap<String, config::AgentConfig>,
    /// Active GitHub OAuth device flows (keyed by device_code).
    device_flows: tokio::sync::Mutex<HashMap<String, DeviceFlow>>,
    /// Runtime state partitioned by client-provided session key.
    runtime_states: tokio::sync::Mutex<HashMap<String, RuntimeSessionState>>,
}

#[derive(Debug, Clone)]
struct RollbackChangeSet {
    id: String,
    tool_name: String,
    entries: Vec<llm::RollbackEntry>,
    display_changes: Vec<marshaling_protocol::FileChange>,
}

#[derive(Debug, Default)]
struct RuntimeSessionState {
    rollback_journal: Vec<RollbackChangeSet>,
    remember_allow_tools: HashSet<String>,
}

#[derive(Debug, Clone)]
struct RequestContext {
    workspace: PathBuf,
    workspace_display: String,
    runtime_session_key: String,
    repo_agents_md: Option<String>,
}

// ── HTTP routes ─────────────────────────────────────────

/// GET /health
async fn health() -> impl IntoResponse {
    Json(marshaling_protocol::HealthResponse {
        status: "ok".into(),
        protocol_version: marshaling_protocol::PROTOCOL_VERSION.into(),
    })
}

/// GET /config  — returns UI-relevant config for the client.
async fn get_config(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let cfg = &state.config;
    let mut agent_names: Vec<String> = state
        .merged_agents
        .iter()
        .filter(|(_, a)| a.is_user_selectable())
        .map(|(n, _)| n.clone())
        .collect();
    agent_names.sort();
    let mut subagent_names: Vec<String> = state
        .merged_agents
        .iter()
        .filter(|(_, a)| a.is_subagent_callable())
        .map(|(n, _)| n.clone())
        .collect();
    subagent_names.sort();
    Json(marshaling_protocol::UiConfig {
        input_accent: cfg.input_accent().to_string(),
        user_accent: cfg.user_accent().to_string(),
        agent_names,
        subagent_names,
        model_info: format!("{}/{}", cfg.model.provider, cfg.model.model_id),
    })
}

/// GET /sessions
async fn list_sessions(
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let Some(runtime_session_key) = runtime_session_key_from_headers(&headers)
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let hist_dir = history_dir_for_session(
        &state.config.history.dir,
        &runtime_session_key,
    );
    let items = tokio::task::spawn_blocking(
        move || -> Vec<marshaling_protocol::SessionInfo> {
            match history::list_sessions(&hist_dir) {
                Ok(sessions) => sessions
                    .into_iter()
                    .map(|(meta, path)| {
                        let msg_count = history::parse_file(&path)
                            .map(|(_, msgs)| msgs.len())
                            .unwrap_or(0);
                        marshaling_protocol::SessionInfo {
                            id: meta.id,
                            created: meta.created.to_rfc3339(),
                            model: format!(
                                "{}/{}",
                                meta.model_provider, meta.model_id
                            ),
                            message_count: msg_count,
                            summary: meta.summary,
                        }
                    })
                    .collect(),
                Err(_) => Vec::new(),
            }
        },
    )
    .await
    .unwrap_or_default();
    Ok(Json(items))
}

/// GET /sessions/:id — load a specific session.
async fn load_session(
    Path(id): Path<String>,
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<Json<marshaling_protocol::SessionData>, StatusCode> {
    if !validate_session_id(&id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let runtime_session_key = runtime_session_key_from_headers(&headers)
        .ok_or(StatusCode::BAD_REQUEST)?;
    let path = history_dir_for_session(
        &state.config.history.dir,
        &runtime_session_key,
    )
    .join(format!("{id}.md"));
    let result =
        tokio::task::spawn_blocking(move || history::parse_file(&path))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match result {
        Ok((meta, messages)) => {
            let msgs: Vec<marshaling_protocol::HistoryMessage> = messages
                .into_iter()
                .filter_map(|m| {
                    let role = protocol_role_for_session(m.role)?;
                    Some(marshaling_protocol::HistoryMessage {
                        role: role.into(),
                        content: m.content,
                    })
                })
                .collect();
            Ok(Json(marshaling_protocol::SessionData {
                id: meta.id,
                created: meta.created.to_rfc3339(),
                model: format!("{}/{}", meta.model_provider, meta.model_id),
                messages: msgs,
            }))
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// DELETE /sessions/:id — delete a saved session.
async fn delete_session(
    Path(id): Path<String>,
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> StatusCode {
    if !validate_session_id(&id) {
        return StatusCode::BAD_REQUEST;
    }
    let Some(runtime_session_key) = runtime_session_key_from_headers(&headers)
    else {
        return StatusCode::BAD_REQUEST;
    };
    let path = history_dir_for_session(
        &state.config.history.dir,
        &runtime_session_key,
    )
    .join(format!("{id}.md"));
    let result = tokio::task::spawn_blocking(move || {
        if path.exists() {
            match std::fs::remove_file(&path) {
                Ok(_) => StatusCode::OK,
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
            }
        } else {
            StatusCode::NOT_FOUND
        }
    })
    .await
    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if result == StatusCode::OK {
        tracing::info!("Deleted session: {id}");
    }
    result
}

/// GET /models — list available models from all configured providers.
async fn list_models_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let mut all = Vec::new();
    let provider_names = ["deepseek", "github", "ollama"];
    for name in &provider_names {
        if let Ok(provider) = llm::build_provider_for(
            &state.config,
            &*state.auth.read().await,
            name,
        ) {
            if let Ok(models) = provider.list_models().await {
                for m in models {
                    all.push(marshaling_protocol::ModelInfo {
                        provider: name.to_string(),
                        model_id: m,
                    });
                }
            }
        }
    }
    Json(all)
}

/// POST /rollback/last — rollback latest tracked file change-set.
async fn rollback_last_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(payload): Json<marshaling_protocol::RollbackLastRequest>,
) -> impl IntoResponse {
    Json(apply_rollback_last(&state, &payload.runtime_session_key).await)
}

// ── GitHub OAuth Device Flow routes ─────────────────────

/// Response for polling a device login flow.
#[derive(Debug, Serialize)]
struct GithubPollResponse {
    status: String, // "pending", "completed", "failed", "expired"
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// POST /auth/github/start — initiate a GitHub OAuth device flow.
async fn github_login_start(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let client = reqwest::Client::new();
    let client_id = github_client_id();
    let resp = match client
        .post(GITHUB_DEVICE_CODE_URL)
        .form(&[("client_id", &*client_id), ("scope", GITHUB_SCOPES)])
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("Failed to contact GitHub: {e}")
                })),
            );
        }
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("Failed to parse GitHub response: {e}")
                })),
            );
        }
    };

    let device_code = match body["device_code"].as_str() {
        Some(c) => c.to_string(),
        None => {
            let err = body["error_description"]
                .as_str()
                .unwrap_or("unknown error");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": err
                })),
            );
        }
    };
    let user_code = body["user_code"]
        .as_str()
        .unwrap_or("????-????")
        .to_string();
    let verification_uri = body["verification_uri"]
        .as_str()
        .unwrap_or("https://github.com/login/device")
        .to_string();
    // Clamp GitHub API values to reasonable bounds
    let interval = body["interval"].as_u64().unwrap_or(5).clamp(1, 60);
    let expires_in = body["expires_in"].as_u64().unwrap_or(900).clamp(60, 1800);

    let status: Arc<tokio::sync::Mutex<FlowStatus>> =
        Arc::new(tokio::sync::Mutex::new(FlowStatus::Pending));
    let status_clone = Arc::clone(&status);
    let dc = device_code.clone();
    let expires_at =
        std::time::Instant::now() + std::time::Duration::from_secs(expires_in);
    let poll_interval = std::time::Duration::from_secs(interval);

    // Spawn background task to poll GitHub
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        let mut poll_interval = poll_interval;
        let client_id = github_client_id();

        loop {
            tokio::time::sleep(poll_interval).await;

            if std::time::Instant::now() > expires_at {
                let mut s = status_clone.lock().await;
                *s = FlowStatus::Failed("Authentication timed out".into());
                // Don't remove from device_flows — let the client poll get the expired status
                return;
            }

            let poll_resp = match http
                .post(GITHUB_TOKEN_URL)
                .form(&[
                    ("client_id", &*client_id),
                    ("device_code", &dc),
                    (
                        "grant_type",
                        "urn:ietf:params:oauth:grant-type:device_code",
                    ),
                ])
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let mut s = status_clone.lock().await;
                    *s = FlowStatus::Failed(format!("Network error: {e}"));
                    return;
                }
            };

            let poll_body: serde_json::Value = match poll_resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    let mut s = status_clone.lock().await;
                    *s = FlowStatus::Failed(format!("Parse error: {e}"));
                    return;
                }
            };

            if let Some(token) =
                poll_body.get("access_token").and_then(|t| t.as_str())
            {
                // Success! Save token to auth.json (blocking I/O)
                let token_str = token.to_string();
                let save_result = tokio::task::spawn_blocking(move || {
                    auth::save_token_to_auth("github", &token_str)
                })
                .await;
                match save_result {
                    Ok(Ok(())) => {
                        let mut s = status_clone.lock().await;
                        *s = FlowStatus::Completed;
                    }
                    Ok(Err(e)) => {
                        let mut s = status_clone.lock().await;
                        *s = FlowStatus::Failed(format!("Failed to save: {e}"));
                    }
                    Err(e) => {
                        let mut s = status_clone.lock().await;
                        *s = FlowStatus::Failed(format!("Task error: {e}"));
                    }
                }
                return;
            }

            if let Some(err) = poll_body.get("error").and_then(|e| e.as_str()) {
                match err {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        // GitHub asks us to slow down — increase interval by 5s
                        poll_interval += std::time::Duration::from_secs(5);
                        continue;
                    }
                    _ => {
                        let mut s = status_clone.lock().await;
                        *s = FlowStatus::Failed(err.to_string());
                        return;
                    }
                }
            }
        }
    });

    // Store the flow (tokio::sync::Mutex — safe in async context)
    state
        .device_flows
        .lock()
        .await
        .insert(device_code.clone(), DeviceFlow { status });

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "user_code": user_code,
            "verification_uri": verification_uri,
            "device_code": device_code,
            "interval": interval,
        })),
    )
}

/// POST /auth/github/poll — poll the status of an active device flow.
async fn github_login_poll(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(body): axum::extract::Json<HashMap<String, String>>,
) -> impl IntoResponse {
    let device_code = match body.get("device_code") {
        Some(c) => c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(GithubPollResponse {
                    status: "failed".into(),
                    error: Some("Missing device_code".into()),
                }),
            );
        }
    };

    let flows = state.device_flows.lock().await;
    match flows.get(device_code) {
        Some(flow) => {
            let status_guard = flow.status.lock().await;
            match &*status_guard {
                FlowStatus::Pending => (
                    StatusCode::OK,
                    Json(GithubPollResponse {
                        status: "pending".into(),
                        error: None,
                    }),
                ),
                FlowStatus::Completed => (
                    StatusCode::OK,
                    Json(GithubPollResponse {
                        status: "completed".into(),
                        error: None,
                    }),
                ),
                FlowStatus::Failed(err) => {
                    // Map timeout message to "expired" status for the client
                    let (status_str, error_str) =
                        if err == "Authentication timed out" {
                            ("expired", err.as_str())
                        } else {
                            ("failed", err.as_str())
                        };
                    (
                        StatusCode::OK,
                        Json(GithubPollResponse {
                            status: status_str.into(),
                            error: Some(error_str.into()),
                        }),
                    )
                }
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(GithubPollResponse {
                status: "failed".into(),
                error: Some(
                    "Unknown device_code. Start a new login flow.".into(),
                ),
            }),
        ),
    }
}

// ── Generic credential save (DeepSeek, etc.) ────────────

/// POST /auth/save — save a credential to auth.json.
///
/// Request body:
///   { "provider": "deepseek", "api_key": "sk-..." }
///   { "provider": "github",   "token": "ghp_..." }
async fn auth_save(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(body): axum::extract::Json<serde_json::Value>,
) -> impl IntoResponse {
    let provider = match body.get("provider").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "Missing field: provider"
                })),
            );
        }
    };

    // Extract credential: prefer token, fall back to api_key
    let (field_name, credential) =
        if let Some(val) = body.get("token").and_then(|v| v.as_str()) {
            ("token", val.to_string())
        } else if let Some(val) = body.get("api_key").and_then(|v| v.as_str()) {
            ("api_key", val.to_string())
        } else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "Missing field: token or api_key"
                })),
            );
        };

    // Save in blocking task (file I/O)
    let provider_owned = provider.to_string();
    let field_owned = field_name.to_string();
    let value_owned = credential.clone();
    let result = tokio::task::spawn_blocking(move || {
        auth::save_credential(&provider_owned, &field_owned, &value_owned)
    })
    .await;

    match result {
        Ok(Ok(())) => {
            tracing::info!("Saved credential for provider '{provider}'");
            // Reload auth into memory so subsequent requests see the new credential
            let fresh_auth = auth::Auth::load();
            *state.auth.write().await = fresh_auth;
            (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
        }
        Ok(Err(e)) => {
            tracing::error!("Failed to save credential: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("{:#}", e)
                })),
            )
        }
        Err(e) => {
            tracing::error!("Credential save task panicked: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Internal error: {e}")
                })),
            )
        }
    }
}

// ── WebSocket chat handler ──────────────────────────────

/// Drop guard that signals cancellation when the WS handler exits for any reason.
struct CancelGuard(tokio::sync::watch::Sender<bool>);
impl Drop for CancelGuard {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// Helper: send an error event over WebSocket.
async fn send_error(socket: &mut WebSocket, msg: impl Into<String>) {
    let json =
        serde_json::to_string(&marshaling_protocol::ServerEvent::Error {
            message: msg.into(),
        })
        .unwrap();
    let _ = socket.send(Message::Text(json.into())).await;
}

/// Validate a session ID to prevent path traversal.
fn validate_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

fn validate_runtime_session_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ':')
}

fn runtime_session_key_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("x-mote-session-key")?.to_str().ok()?;
    let key = value.trim();
    if validate_runtime_session_key(key) {
        Some(key.to_string())
    } else {
        None
    }
}

fn protocol_role_for_session(role: llm::Role) -> Option<&'static str> {
    match role {
        llm::Role::User => Some("user"),
        llm::Role::Assistant => Some("assistant"),
        _ => None,
    }
}

fn apply_selected_session_id(
    session: &mut session::Session,
    selected_session_id: Option<&str>,
) {
    if let Some(sid) = selected_session_id {
        session.id = sid.to_string();
    }
}

fn history_dir_for_session(
    base_history_dir: &std::path::Path,
    runtime_session_key: &str,
) -> PathBuf {
    base_history_dir.join(runtime_session_key)
}

fn resolve_request_context(
    request: &marshaling_protocol::ChatRequest,
) -> Result<RequestContext> {
    let workspace_raw = request
        .workspace_root
        .as_ref()
        .context("Missing workspace_root in chat request")?;
    let workspace_path = PathBuf::from(workspace_raw);
    if !workspace_path.is_absolute() {
        anyhow::bail!("workspace_root must be an absolute path");
    }
    if !workspace_path.exists() {
        anyhow::bail!(
            "workspace_root does not exist: {}",
            workspace_path.display()
        );
    }
    let workspace = workspace_path.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "Failed to canonicalize workspace_root {}: {}",
            workspace_path.display(),
            e
        )
    })?;
    if !workspace.is_dir() {
        anyhow::bail!(
            "workspace_root is not a directory: {}",
            workspace.display()
        );
    }

    let runtime_session_key = request
        .runtime_session_key
        .clone()
        .context("Missing runtime_session_key in chat request")?;
    if !validate_runtime_session_key(&runtime_session_key) {
        anyhow::bail!("Invalid runtime_session_key");
    }

    Ok(RequestContext {
        workspace_display: workspace.display().to_string(),
        workspace,
        runtime_session_key,
        repo_agents_md: request.repo_agents_md.clone(),
    })
}

// ── Extracted helpers for agent setup ───────────────────

/// Resolved agent context: provider, model, system prompt, and options.
struct AgentContext {
    provider: Arc<dyn llm::LlmProvider>,
    system_layers: Vec<String>,
    opts: llm::ChatOptions,
    eff_provider: String,
    eff_model_id: String,
}

/// Resolve the agent context: provider, model, system prompt, and options.
///
/// Used by both `handle_socket` (primary agent) and `AgentSubagentRunner`.
async fn resolve_agent_context(
    config: &config::Config,
    auth: &auth::Auth,
    merged_agents: &HashMap<String, config::AgentConfig>,
    req_ctx: &RequestContext,
    agent_name: &str,
    model_override: Option<&str>,
    provider_override: Option<&str>,
) -> Result<AgentContext> {
    let agent_cfg = merged_agents.get(agent_name);
    let agent_model = agent_cfg.and_then(|a| a.model.as_deref());

    // Resolve provider:
    // 1. Explicit provider_override (set by client /model command when using "provider/model" format)
    // 2. Fallback: parse from model_override (backward compat with "provider/model" embedded in model_override)
    // 3. Default: config + agent settings
    let eff_provider = provider_override
        .map(|s| s.to_string())
        .or_else(|| {
            model_override
                .and_then(|m| m.split_once('/').map(|(p, _)| p.to_string()))
        })
        .unwrap_or_else(|| config.effective_provider(agent_model));

    // Model ID: when model_override is provided, it's just the model name (no provider/ prefix).
    // Backward compat: strip provider/ prefix if present.
    let eff_model_id = model_override
        .map(|s| {
            s.split_once('/')
                .map(|(_, m)| m.to_string())
                .unwrap_or_else(|| s.to_string())
        })
        .unwrap_or_else(|| config.effective_model_id(agent_model));

    let eff_temperature =
        config.effective_temperature(agent_cfg.and_then(|a| a.temperature));

    // Build LLM provider
    let provider: Arc<dyn llm::LlmProvider> =
        Arc::from(llm::build_provider_for(config, auth, &eff_provider)?);

    // Build system prompt (in blocking thread — reads filesystem)
    let prompt = prompt::PromptAssembler::for_agent(
        config,
        merged_agents.get(agent_name),
    )
    .with_workspace_context(
        Some(req_ctx.workspace.clone()),
        req_ctx.repo_agents_md.clone(),
    );
    let eff_provider_clone = eff_provider.clone();
    let eff_model_id_clone = eff_model_id.clone();
    let system_layers = tokio::task::spawn_blocking(move || {
        prompt.assemble(&eff_provider_clone, &eff_model_id_clone)
    })
    .await
    .map_err(|e| anyhow::anyhow!("Prompt assembly panicked: {:#}", e))??;

    // Build options
    let eff_max_tokens = config.effective_max_tokens(
        agent_cfg.and_then(|a| a.max_tokens),
        &eff_provider,
    );
    let opts = llm::ChatOptions {
        model_id: eff_model_id.clone(),
        temperature: eff_temperature,
        max_tokens: eff_max_tokens,
        tools: Vec::new(), // populated later with augmented tools
    };

    Ok(AgentContext {
        provider,
        system_layers,
        opts,
        eff_provider,
        eff_model_id,
    })
}

/// Build the permission map for a given agent.
///
/// Resolution order: agent-specific → global tool → global default.
/// Used by both `handle_socket` and `AgentSubagentRunner`.
pub fn build_permission_map(
    config: &config::Config,
    agent_cfg: Option<&config::AgentConfig>,
    tool_names: &[String],
) -> HashMap<String, config::Permission> {
    let agent_permissions = agent_cfg.map(|a| &a.permissions);
    let mut perms = HashMap::new();
    for tool_name in tool_names {
        let effective = agent_permissions
            .and_then(|ap| ap.get(tool_name))
            .or_else(|| config.permissions.tools.get(tool_name))
            .copied()
            .unwrap_or(config.permissions.default);
        perms.insert(tool_name.clone(), effective);
    }
    // use_skill is always allowed (safe read-only)
    perms.insert("use_skill".into(), config::Permission::Allow);
    // Resolve subagent permission
    let subagent_perm = agent_permissions
        .and_then(|ap| ap.get("subagent"))
        .or_else(|| config.permissions.tools.get("subagent"))
        .copied()
        .unwrap_or(config.permissions.default);
    perms.insert("subagent".into(), subagent_perm);
    perms
}

/// Build the augmented tool set (builtins + use_skill + subagent tool).
fn build_augmented_tools(
    workspace: &std::path::Path,
    repo_agents_md: Option<String>,
    provider: &Arc<dyn llm::LlmProvider>,
    config: &config::Config,
    merged_agents: &HashMap<String, config::AgentConfig>,
    cancel_rx: &tokio::sync::watch::Receiver<bool>,
    agent_tx: &mpsc::UnboundedSender<Result<agent::AgentEvent>>,
) -> Arc<Vec<Box<dyn llm::Tool>>> {
    let mut augmented: Vec<Box<dyn llm::Tool>> =
        llm::builtin_tools(workspace.to_path_buf());
    augmented.push(Box::new(tools::UseSkillTool));

    // Subagent tool set: builtins + use_skill (no subagent tool to prevent recursion).
    let subagent_tools: Arc<Vec<Box<dyn llm::Tool>>> = {
        let mut v = llm::builtin_tools(workspace.to_path_buf());
        v.push(Box::new(tools::UseSkillTool));
        Arc::new(v)
    };

    augmented.push(Box::new(tools::SubagentTool::new(
        tools::ToolContext {
            workspace: workspace.to_path_buf(),
        },
        Box::new(tools::AgentSubagentRunner {
            provider: Arc::clone(provider),
            tools: subagent_tools,
            config: config.clone(),
            merged_agents: merged_agents.clone(),
            repo_agents_md,
            cancel_rx: cancel_rx.clone(),
            depth: 0,
            max_depth: 3,
            parent_events_tx: agent_tx.clone(),
        }),
    )));
    Arc::new(augmented)
}
async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    debug!("WebSocket connected");

    // Wait for the first message: ChatRequest
    let request = match socket.recv().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<
            marshaling_protocol::ChatRequest,
        >(&text)
        {
            Ok(req) => req,
            Err(e) => {
                send_error(&mut socket, format!("Invalid chat request: {e}"))
                    .await;
                return;
            }
        },
        _ => return,
    };
    debug!(
        "Chat request: agent={}, message_len={}",
        request.agent,
        request.message.len()
    );

    let req_ctx = match resolve_request_context(&request) {
        Ok(v) => v,
        Err(e) => {
            send_error(&mut socket, format!("{:#}", e)).await;
            return;
        }
    };

    let selected_session_id = if let Some(sid) = request.session_id.clone() {
        if !validate_session_id(&sid) {
            send_error(&mut socket, "Invalid session_id").await;
            return;
        }
        Some(sid)
    } else {
        None
    };

    // Resolve agent: empty agent → "default" for safety
    let agent_name = if request.agent.is_empty() {
        "default".to_string()
    } else {
        request.agent.clone()
    };

    // Validate agent is user-selectable
    if let Some(agent) = state.merged_agents.get(&agent_name) {
        if !agent.is_user_selectable() {
            send_error(&mut socket, format!("Agent '{agent_name}' is a subagent-only agent and cannot be used directly. Use a primary agent and delegate via the subagent tool.")).await;
            return;
        }
    }

    // Resolve agent context (provider, model, system prompt, options)
    let auth_guard = state.auth.read().await;
    let ctx = match resolve_agent_context(
        &state.config,
        &*auth_guard,
        &state.merged_agents,
        &req_ctx,
        &agent_name,
        request.model_override.as_deref(),
        request.provider_override.as_deref(),
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(e) => {
            send_error(&mut socket, format!("{:#}", e)).await;
            return;
        }
    };
    // Drop auth guard before the long-running agent loop
    drop(auth_guard);

    // Build permission map
    let preview_tools = llm::builtin_tools(req_ctx.workspace.clone());
    let tool_names: Vec<String> = preview_tools
        .iter()
        .map(|t| t.def().function.name.clone())
        .collect();
    let agent_cfg = state.merged_agents.get(&agent_name);
    let mut perms = build_permission_map(&state.config, agent_cfg, &tool_names);
    let remembered_allows = {
        let sessions = state.runtime_states.lock().await;
        sessions
            .get(&req_ctx.runtime_session_key)
            .map(|s| s.remember_allow_tools.clone())
            .unwrap_or_default()
    };
    for tool in remembered_allows {
        if perms.get(&tool) == Some(&config::Permission::Ask) {
            perms.insert(tool, config::Permission::Allow);
        }
    }

    // Run the agent loop — pass channels for events and permission responses
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let _cancel_guard = CancelGuard(cancel_tx.clone());
    let (permission_tx, permission_rx) =
        mpsc::unbounded_channel::<(String, bool)>();
    let mut pending_permission_tools: HashMap<String, String> = HashMap::new();

    // Build augmented tools (builtins + use_skill + subagent)
    let augmented_tools = build_augmented_tools(
        &req_ctx.workspace,
        req_ctx.repo_agents_md.clone(),
        &ctx.provider,
        &state.config,
        &state.merged_agents,
        &cancel_rx,
        &agent_tx,
    );

    // Reconstruct conversation history from the client's display messages
    let user_msg = request.message.clone();
    let mut history: Vec<llm::ChatMessage> = Vec::new();
    for hm in &request.history {
        match hm.role.as_str() {
            "user" => history.push(llm::ChatMessage::user(&hm.content)),
            "assistant" => {
                history.push(llm::ChatMessage::assistant_text(&hm.content))
            }
            _ => {}
        }
    }

    // Finalize options with augmented tool definitions
    let mut opts = ctx.opts;
    opts.tools = augmented_tools.iter().map(|t| t.def()).collect();

    let eff_model_id_save = ctx.eff_model_id.clone();
    let eff_provider = ctx.eff_provider;
    let max_steps = state.config.server.max_steps;
    let augmented_tools_spawn = augmented_tools.clone();
    let prov_spawn = ctx.provider;
    let workspace_display = req_ctx.workspace_display.clone();

    let agent_handle = tokio::spawn(async move {
        agent::run_loop(
            prov_spawn,
            augmented_tools_spawn,
            ctx.system_layers,
            user_msg,
            history,
            opts,
            agent_tx,
            cancel_rx,
            permission_rx,
            perms,
            max_steps,
            workspace_display,
        )
        .await;
    });

    // Forward agent events to WebSocket
    loop {
        tokio::select! {
            agent_event = agent_rx.recv() => {
                match agent_event {
                    Some(Ok(agent::AgentEvent::Done { content, tokens_input, tokens_output, history })) => {
                        // Save session to disk (in blocking thread)
                        let mut session = session::Session::from_chat_history(
                            &eff_model_id_save,
                            &eff_provider,
                            &agent_name,
                            tokens_input,
                            tokens_output,
                            &history,
                        );
                        apply_selected_session_id(
                            &mut session,
                            selected_session_id.as_deref(),
                        );
                        let hist_dir = history_dir_for_session(
                            &state.config.history.dir,
                            &req_ctx.runtime_session_key,
                        );
                        tokio::task::spawn_blocking(move || {
                            if let Err(e) = history::save_session(&hist_dir, &session) {
                                tracing::warn!("Failed to save session: {e}");
                            }
                        });

                        // Forward Done to client (without internal history)
                        let server_event = marshaling_protocol::ServerEvent::Done {
                            content,
                            tokens_input,
                            tokens_output,
                        };
                        let json = serde_json::to_string(&server_event).unwrap();
                        let _ = socket.send(Message::Text(json.into())).await;
                        break;
                    }
                    Some(Ok(event)) => {
                        if let agent::AgentEvent::ToolCompleted { id, name, result, changes, rollback_entries } = event {
                            if !rollback_entries.is_empty() {
                                let mut sessions = state.runtime_states.lock().await;
                                let session_state = sessions.entry(req_ctx.runtime_session_key.clone()).or_default();
                                session_state.rollback_journal.push(RollbackChangeSet {
                                    id: id.clone(),
                                    tool_name: name,
                                    entries: rollback_entries,
                                    display_changes: changes.clone(),
                                });
                            }
                            let server_event = marshaling_protocol::ServerEvent::ToolCompleted { id, result, changes };
                            let json = serde_json::to_string(&server_event).unwrap();
                            if socket.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                            continue;
                        }

                        if let agent::AgentEvent::PermissionRequest { id, tool_name, .. } = &event {
                            pending_permission_tools.insert(id.clone(), tool_name.clone());
                        }

                        let server_event = agent_event_to_server_event(event);
                        let json = serde_json::to_string(&server_event).unwrap();
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                        // If done, the agent loop finished. Send nothing else.
                        if matches!(server_event, marshaling_protocol::ServerEvent::Done { .. }) {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        send_error(&mut socket, format!("{:#}", e)).await;
                        break;
                    }
                    None => break,
                }
            }
            ws_msg = socket.recv() => {
                match ws_msg {
                    Some(Ok(Message::Close(_))) | None => {
                        let _ = cancel_tx.send(true);
                        break;
                    }
                    Some(Ok(Message::Text(text))) => {
                        // Check if this is a ClientEvent (e.g., permission response, cancel)
                        if let Ok(client_event) = serde_json::from_str::<marshaling_protocol::ClientEvent>(&text) {
                            match client_event {
                                marshaling_protocol::ClientEvent::PermissionResponse { id, allowed, remember } => {
                                    if remember && allowed {
                                        if let Some(tool_name) = pending_permission_tools.get(&id).cloned() {
                                            let mut sessions = state.runtime_states.lock().await;
                                            let sess = sessions.entry(req_ctx.runtime_session_key.clone()).or_default();
                                            sess.remember_allow_tools.insert(tool_name);
                                        }
                                    }
                                    let _ = permission_tx.send((id, allowed));
                                }
                                marshaling_protocol::ClientEvent::Cancel => {
                                    debug!("Client requested cancellation");
                                    let _ = cancel_tx.send(true);
                                }
                                marshaling_protocol::ClientEvent::RollbackLast { runtime_session_key } => {
                                    let key = runtime_session_key.unwrap_or_else(|| req_ctx.runtime_session_key.clone());
                                    let payload = apply_rollback_last(&state, &key).await;
                                    let evt = marshaling_protocol::ServerEvent::RollbackResult {
                                        success: payload.success,
                                        message: payload.message,
                                        changes: payload.changes,
                                    };
                                    let json = serde_json::to_string(&evt).unwrap();
                                    let _ = socket.send(Message::Text(json.into())).await;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Check if the agent task panicked
    if let Err(e) = agent_handle.await {
        tracing::error!("Agent loop panicked: {:#}", e);
    }
}

fn agent_event_to_server_event(
    event: agent::AgentEvent,
) -> marshaling_protocol::ServerEvent {
    use agent::AgentEvent;
    match event {
        AgentEvent::TextDelta(text) => {
            marshaling_protocol::ServerEvent::TextDelta { data: text }
        }
        AgentEvent::ReasoningDelta(text) => {
            marshaling_protocol::ServerEvent::ReasoningDelta { data: text }
        }
        AgentEvent::ToolStarted { id, name } => {
            marshaling_protocol::ServerEvent::ToolStarted { id, name }
        }
        AgentEvent::ToolCompleted {
            id,
            result,
            changes,
            ..
        } => marshaling_protocol::ServerEvent::ToolCompleted {
            id,
            result,
            changes,
        },
        AgentEvent::ToolFailed { id, error } => {
            marshaling_protocol::ServerEvent::ToolFailed { id, error }
        }
        AgentEvent::PermissionRequest {
            id,
            tool_name,
            args,
        } => marshaling_protocol::ServerEvent::PermissionRequest {
            id,
            tool_name,
            args,
        },
        AgentEvent::SkillsLoaded { names } => {
            marshaling_protocol::ServerEvent::SkillsLoaded { names }
        }
        AgentEvent::SkillSelected { name } => {
            marshaling_protocol::ServerEvent::SkillSelected { name }
        }
        AgentEvent::SubagentStarted { id, name } => {
            marshaling_protocol::ServerEvent::SubagentStarted { id, name }
        }
        AgentEvent::SubagentTextDelta { id, data } => {
            marshaling_protocol::ServerEvent::SubagentTextDelta { id, data }
        }
        AgentEvent::SubagentReasoningDelta { id, data } => {
            marshaling_protocol::ServerEvent::SubagentReasoningDelta {
                id,
                data,
            }
        }
        AgentEvent::SubagentToolStarted {
            id,
            sub_id,
            tool_name,
        } => marshaling_protocol::ServerEvent::SubagentToolStarted {
            id,
            sub_id,
            tool_name,
        },
        AgentEvent::SubagentToolCompleted {
            id,
            sub_id,
            result,
            changes,
        } => marshaling_protocol::ServerEvent::SubagentToolCompleted {
            id,
            sub_id,
            result,
            changes,
        },
        AgentEvent::SubagentToolFailed { id, sub_id, error } => {
            marshaling_protocol::ServerEvent::SubagentToolFailed {
                id,
                sub_id,
                error,
            }
        }
        AgentEvent::SubagentDone { id, content } => {
            marshaling_protocol::ServerEvent::SubagentDone { id, content }
        }
        AgentEvent::TurnDone { text, tool_calls } => {
            marshaling_protocol::ServerEvent::TurnDone { text, tool_calls }
        }
        AgentEvent::Done {
            content,
            tokens_input,
            tokens_output,
            ..
        } => marshaling_protocol::ServerEvent::Done {
            content,
            tokens_input,
            tokens_output,
        },
    }
}

fn hash64(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    h.finish()
}

async fn apply_rollback_last(
    state: &Arc<AppState>,
    runtime_session_key: &str,
) -> marshaling_protocol::RollbackResultPayload {
    let cs = {
        let mut sessions = state.runtime_states.lock().await;
        let Some(session) = sessions.get_mut(runtime_session_key) else {
            return marshaling_protocol::RollbackResultPayload {
                success: false,
                message: "No reversible changes available in this session."
                    .into(),
                changes: Vec::new(),
            };
        };
        match session.rollback_journal.pop() {
            Some(v) => v,
            None => {
                return marshaling_protocol::RollbackResultPayload {
                    success: false,
                    message: "No reversible changes available in this session."
                        .into(),
                    changes: Vec::new(),
                };
            }
        }
    };

    // Preflight conflict checks
    for entry in &cs.entries {
        match entry.kind {
            llm::RollbackKind::Modified => {
                let content = match tokio::fs::read_to_string(&entry.path).await
                {
                    Ok(c) => c,
                    Err(e) => {
                        return marshaling_protocol::RollbackResultPayload {
                            success: false,
                            message: format!(
                                "Rollback blocked: failed to read {}: {e}",
                                entry.path.display()
                            ),
                            changes: Vec::new(),
                        };
                    }
                };
                if Some(hash64(&content)) != entry.expected_after_hash {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback blocked: {} changed since the original edit.",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
            }
            llm::RollbackKind::Added => {
                if !entry.path.exists() {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback blocked: {} is already missing.",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
                let content = match tokio::fs::read_to_string(&entry.path).await
                {
                    Ok(c) => c,
                    Err(e) => {
                        return marshaling_protocol::RollbackResultPayload {
                            success: false,
                            message: format!(
                                "Rollback blocked: failed to read {}: {e}",
                                entry.path.display()
                            ),
                            changes: Vec::new(),
                        };
                    }
                };
                if Some(hash64(&content)) != entry.expected_after_hash {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback blocked: {} changed since it was added.",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
            }
            llm::RollbackKind::Removed => {
                if entry.path.exists() {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback blocked: {} already exists.",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
            }
        }
    }

    // Apply rollback
    for entry in &cs.entries {
        match entry.kind {
            llm::RollbackKind::Modified => {
                if let Some(before) = &entry.before_content {
                    if let Err(e) = tokio::fs::write(&entry.path, before).await
                    {
                        return marshaling_protocol::RollbackResultPayload {
                            success: false,
                            message: format!(
                                "Rollback failed writing {}: {e}",
                                entry.path.display()
                            ),
                            changes: Vec::new(),
                        };
                    }
                }
            }
            llm::RollbackKind::Added => {
                if let Err(e) = tokio::fs::remove_file(&entry.path).await {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback failed removing {}: {e}",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
            }
            llm::RollbackKind::Removed => {
                if let Some(before) = &entry.before_content {
                    if let Some(parent) = entry.path.parent() {
                        if let Err(e) = tokio::fs::create_dir_all(parent).await
                        {
                            return marshaling_protocol::RollbackResultPayload {
                                success: false,
                                message: format!("Rollback failed creating {}: {e}", parent.display()),
                                changes: Vec::new(),
                            };
                        }
                    }
                    if let Err(e) = tokio::fs::write(&entry.path, before).await
                    {
                        return marshaling_protocol::RollbackResultPayload {
                            success: false,
                            message: format!(
                                "Rollback failed restoring {}: {e}",
                                entry.path.display()
                            ),
                            changes: Vec::new(),
                        };
                    }
                }
            }
        }
    }

    marshaling_protocol::RollbackResultPayload {
        success: true,
        message: format!("Rolled back {} ({})", cs.id, cs.tool_name),
        changes: cs.display_changes,
    }
}

// ── Session listing helper ──────────────────────────────

fn find_config() -> Result<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        let cfg_path = home.join(".config").join("mote").join("config.toml");
        if cfg_path.exists() {
            return Ok(cfg_path);
        }
    }
    let cwd_config = PathBuf::from("config.toml");
    if cwd_config.exists() {
        return Ok(cwd_config);
    }
    Ok(cwd_config)
}

// ── Main ────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Load config early so logging path can come from config.
    let config_path = find_config()?;
    if !config_path.exists() {
        anyhow::bail!(
            "No config.toml found at {} or CWD.",
            config_path.display()
        );
    }
    let mut config = config::Config::load(&config_path)?;
    if config.history.dir.is_relative() {
        let base = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        config.history.dir = base.join(&config.history.dir);
    }
    if config.logging.dir.is_relative() {
        let base = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        config.logging.dir = base.join(&config.logging.dir);
    }

    // Logging setup: debug/trace → file, otherwise → stderr
    let env_log = std::env::var("RUST_LOG").unwrap_or_default();
    let wants_debug = env_log.eq_ignore_ascii_case("debug")
        || env_log.eq_ignore_ascii_case("trace")
        || env_log.contains("mote=debug");

    if wants_debug {
        let log_dir = config.logging.dir.clone();
        std::fs::create_dir_all(&log_dir).ok();
        let log_path = log_dir.join("mote.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap_or_else(|_| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .unwrap()
            });
        let (non_blocking, _guard) = tracing_appender::non_blocking(log_file);
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "debug".into()),
            )
            .with_writer(non_blocking)
            .with_ansi(false)
            .init();
        Box::leak(Box::new(_guard));
        tracing::info!(
            "Verbose logging enabled, writing to {}",
            log_path.display()
        );
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();
    }
    info!("Config loaded from {}", config_path.display());

    // Load auth secrets
    let auth = auth::Auth::load();
    info!("Auth loaded from {}", auth::auth_path().display());

    info!("Server started (workspace is per-request)");

    let state = Arc::new(AppState {
        merged_agents: config::all_agents(&config.agents),
        auth: RwLock::new(auth),
        config,
        device_flows: tokio::sync::Mutex::new(HashMap::new()),
        runtime_states: tokio::sync::Mutex::new(HashMap::new()),
    });

    let port = state.config.server.port;

    // Build router
    let app = Router::new()
        .route("/health", get(health))
        .route("/config", get(get_config))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(load_session).delete(delete_session))
        .route("/models", get(list_models_handler))
        .route("/rollback/last", post(rollback_last_handler))
        .route("/chat", get(ws_handler))
        .route("/auth/github/start", post(github_login_start))
        .route("/auth/github/poll", post(github_login_poll))
        .route("/auth/save", post(auth_save))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    info!("Starting mote-server on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_session_id_allows_valid() {
        assert!(validate_session_id("chat-20260526-184530123456"));
        assert!(validate_session_id("abc123"));
    }

    #[test]
    fn test_validate_session_id_rejects_traversal() {
        assert!(!validate_session_id(""));
        assert!(!validate_session_id("../etc/passwd"));
        assert!(!validate_session_id("a/b"));
        assert!(!validate_session_id("a\\b"));
    }

    #[test]
    fn test_validate_runtime_session_key() {
        assert!(validate_runtime_session_key("abc-123_def:1"));
        assert!(!validate_runtime_session_key(""));
        assert!(!validate_runtime_session_key("../../bad"));
        assert!(!validate_runtime_session_key("bad key"));
    }

    #[test]
    fn test_build_permission_map_basic() {
        let cfg: config::Config = toml::from_str(
            r#"
[model]
provider = "ollama"
model_id = "m"

[providers.ollama]
base_url = "http://localhost:11434"

[permissions]
default = "ask"
read = "allow"
"#,
        )
        .unwrap();
        let tools = vec!["read".to_string(), "bash".to_string()];
        let perms = build_permission_map(&cfg, None, &tools);
        assert_eq!(perms.get("read"), Some(&config::Permission::Allow));
        assert_eq!(perms.get("bash"), Some(&config::Permission::Ask));
        assert_eq!(perms.get("use_skill"), Some(&config::Permission::Allow));
    }

    #[test]
    fn test_runtime_session_key_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-mote-session-key", "abc-123".parse().unwrap());
        assert_eq!(
            runtime_session_key_from_headers(&headers).as_deref(),
            Some("abc-123")
        );

        let mut bad = HeaderMap::new();
        bad.insert("x-mote-session-key", "bad key".parse().unwrap());
        assert!(runtime_session_key_from_headers(&bad).is_none());

        let empty = HeaderMap::new();
        assert!(runtime_session_key_from_headers(&empty).is_none());
    }

    #[test]
    fn test_protocol_role_for_session_filters_non_conversation_roles() {
        assert_eq!(protocol_role_for_session(llm::Role::User), Some("user"));
        assert_eq!(
            protocol_role_for_session(llm::Role::Assistant),
            Some("assistant")
        );
        assert_eq!(protocol_role_for_session(llm::Role::System), None);
        assert_eq!(protocol_role_for_session(llm::Role::Tool), None);
    }

    #[test]
    fn test_apply_selected_session_id_overrides_when_present() {
        let mut sess = session::Session::new("p".to_string(), "m".to_string());
        let original = sess.id.clone();
        apply_selected_session_id(&mut sess, Some("sess-picked"));
        assert_eq!(sess.id, "sess-picked");

        apply_selected_session_id(&mut sess, None);
        assert_eq!(sess.id, "sess-picked");
        assert_ne!(sess.id, original);
    }
}
