use std::collections::{HashMap, HashSet, VecDeque};
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
use tokio::sync::{RwLock, broadcast, mpsc, watch};
use tower_http::cors::CorsLayer;
use tracing::{Instrument, debug, info};

mod agent;
mod audio;
mod auth;
mod compaction;
mod config;
#[cfg(test)]
mod e2e_tests;
mod history;
mod llm;
mod prompt;
mod rollback;
mod runs;
mod session;
mod sessions;
mod store;
mod tools;

use compaction::*;
use rollback::*;
use runs::*;
use sessions::*;

// ── App state shared across all handlers ─────────────────

struct AppState {
    config: config::Config,
    /// Runtime-updatable auth (reloaded after credential save).
    auth: RwLock<auth::Auth>,
    /// Merged agents from config.toml + separate files (file agents lower priority).
    merged_agents: HashMap<String, config::AgentConfig>,
    /// Runtime state partitioned by client-provided session key.
    runtime_states: tokio::sync::Mutex<HashMap<String, RuntimeSessionState>>,
    /// Long-running agent tasks that outlive websocket subscribers.
    runs: tokio::sync::Mutex<HashMap<String, ActiveRun>>,
    /// Terminal runs retained for short-lived reconnect/replay only.
    completed_run_ids: tokio::sync::Mutex<VecDeque<String>>,
    /// Per-transcript locks serializing writes (runs, compaction,
    /// legacy conversion) to the same session file.
    session_locks:
        tokio::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// Session list entries by transcript path (see `session_infos`).
    session_info_cache: Arc<std::sync::Mutex<SessionInfoCache>>,
}

impl AppState {
    async fn session_lock(
        &self,
        path: &std::path::Path,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.session_locks.lock().await;
        Arc::clone(locks.entry(path.to_path_buf()).or_default())
    }
}

#[derive(Debug, Default)]
struct RuntimeSessionState {
    rollback_journal: Vec<RollbackChangeSet>,
    remember_allow_tools: HashSet<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RequestContext {
    pub(crate) workspace: PathBuf,
    pub(crate) workspace_display: String,
    pub(crate) runtime_session_key: String,
    pub(crate) repo_agents_md: Option<String>,
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
    let mut agent_model_info = HashMap::new();
    agent_model_info.insert(
        cfg.server.default_agent.clone(),
        cfg.effective_model_info(None),
    );
    for (name, agent_cfg) in state
        .merged_agents
        .iter()
        .filter(|(_, a)| a.is_user_selectable())
    {
        agent_model_info.insert(
            name.clone(),
            cfg.effective_model_info(agent_cfg.model.as_deref()),
        );
    }
    Json(marshaling_protocol::UiConfig {
        input_accent: cfg.input_accent().to_string(),
        user_accent: cfg.user_accent().to_string(),
        agent_names,
        subagent_names,
        model_info: format!("{}/{}", cfg.model.provider, cfg.model.model_id),
        agent_model_info,
        default_agent: cfg.server.default_agent.clone(),
        audio: marshaling_protocol::AudioUiConfig {
            model: cfg.audio.model.clone(),
            sample_rate: cfg.audio.sample_rate,
            channels: cfg.audio.channels,
        },
    })
}

/// GET /models — list available models from all configured providers.
async fn list_models_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<Vec<marshaling_protocol::ModelInfo>> {
    let mut all = Vec::new();
    let auth_guard = state.auth.read().await;
    let provider_names = ["deepseek", "glm", "kimi", "minimax", "ollama"];
    for name in &provider_names {
        match llm::build_provider_for(&state.config, &auth_guard, name) {
            Ok(provider) => match provider.list_models().await {
                Ok(models) => {
                    for m in models {
                        all.push(marshaling_protocol::ModelInfo {
                            provider: name.to_string(),
                            model_id: m,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "model listing failed for provider {name}: {e:#}"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    "provider {name} unavailable for model listing: {e:#}"
                );
            }
        }
    }
    Json(all)
}

async fn audio_transcribe_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    tracing::debug!("audio transcription websocket upgrade requested");
    let audio_config = state.config.audio.clone();
    let api_key = {
        let auth_guard = state.auth.read().await;
        match state.config.resolve_audio_api_key(&auth_guard) {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!("audio transcription unavailable: {e:#}");
                return (
                    StatusCode::BAD_REQUEST,
                    format!("Audio transcription unavailable: {e:#}"),
                )
                    .into_response();
            }
        }
    };
    ws.on_upgrade(move |socket| {
        audio::handle_transcription_socket(socket, audio_config, api_key)
    })
    .into_response()
}

// ── Generic credential save (DeepSeek, etc.) ────────────

/// POST /auth/save — save a credential to auth.json.
///
/// Request body:
///   { "provider": "deepseek", "api_key": "sk-..." }
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

/// Helper: send an error event over WebSocket.
async fn send_error(socket: &mut WebSocket, msg: impl Into<String>) {
    let Ok(json) =
        serde_json::to_string(&marshaling_protocol::ServerEvent::Error {
            message: msg.into(),
        })
    else {
        return;
    };
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
pub(crate) struct AgentContext {
    pub(crate) provider: Arc<dyn llm::LlmProvider>,
    pub(crate) system_layers: Vec<String>,
    pub(crate) opts: llm::ChatOptions,
    pub(crate) eff_provider: String,
    pub(crate) eff_model_id: String,
    /// Optional role-loop configuration. When present, the agent loop
    /// enters role-switching mode. The first role is the orchestrator.
    pub(crate) role_loop_config: Option<agent::RoleLoopConfig>,
}

/// Resolve agent context for role-aware loop mode.
/// Creates providers for all unique provider names, builds ResolvedRole structs,
/// and assembles shared system layers using the orchestrator's identity.
#[allow(clippy::too_many_arguments)]
async fn resolve_role_aware_context(
    config: &config::Config,
    auth: &auth::Auth,
    _merged_agents: &HashMap<String, config::AgentConfig>,
    req_ctx: &RequestContext,
    agent_cfg: &config::AgentConfig,
    roles: &[config::RoleConfig],
    agent_model: Option<&str>,
    model_override: Option<&str>,
    provider_override: Option<&str>,
) -> Result<AgentContext> {
    // Validate roles
    agent_cfg
        .validate_roles()
        .map_err(|e| anyhow::anyhow!("Invalid role config for agent: {e}"))?;

    // Resolve overridden agent-level model (for role fallback chain)
    // When model_override and/or provider_override are set, build the
    // effective agent model to reflect both overrides for provider resolution.
    let effective_agent_model: Option<String> = {
        let prov = provider_override.or_else(|| {
            model_override.and_then(|m| m.split_once('/').map(|(p, _)| p))
        });
        let mid = model_override.map(|s| {
            s.split_once('/')
                .map(|(_, m)| m.to_string())
                .unwrap_or_else(|| s.to_string())
        });
        if prov.is_some() || mid.is_some() {
            let prov = prov
                .map(|s| s.to_owned())
                .unwrap_or_else(|| config.effective_provider(agent_model));
            let mid =
                mid.unwrap_or_else(|| config.effective_model_id(agent_model));
            Some(format!("{prov}/{mid}"))
        } else {
            agent_model.map(|s| s.to_string())
        }
    };

    let mut provider_cache: HashMap<String, Arc<dyn llm::LlmProvider>> =
        HashMap::new();
    let mut resolved_roles: Vec<agent::ResolvedRole> =
        Vec::with_capacity(roles.len());

    for role in roles {
        // Resolve provider + model_id for this role
        let (role_prov_name, role_model_id) = config.effective_role_model(
            role.model.as_deref(),
            effective_agent_model.as_deref(),
        );

        // Deduplicate provider instances by provider name
        let provider = match provider_cache.get(&role_prov_name) {
            Some(p) => Arc::clone(p),
            None => {
                let p: Arc<dyn llm::LlmProvider> = Arc::from(
                    llm::build_provider_for(config, auth, &role_prov_name)
                        .with_context(|| {
                            format!(
                                "Failed to build provider '{}' for role '{}'",
                                role_prov_name, role.name
                            )
                        })?,
                );
                provider_cache.insert(role_prov_name.clone(), Arc::clone(&p));
                p
            }
        };

        // Resolve instructions with fallback: role -> agent -> ""
        let instructions = agent_cfg
            .effective_role_instructions(role)
            .unwrap_or_default();

        // Resolve temperature: role -> agent -> config default
        let temperature = config
            .effective_temperature(role.temperature.or(agent_cfg.temperature));

        // Resolve max_tokens: role -> agent -> provider default -> global
        let max_tokens = config.effective_max_tokens(
            role.max_tokens.or(agent_cfg.max_tokens),
            &role_prov_name,
        );

        resolved_roles.push(agent::ResolvedRole {
            name: role.name.clone(),
            instructions,
            provider,
            model_id: role_model_id,
            temperature: Some(temperature),
            max_tokens: Some(max_tokens),
        });
    }

    // Orchestrator is always the first role
    let orchestrator = &resolved_roles[0];

    // Determine the orchestrator's effective provider name from overrides + role config
    let orc_provider_name = provider_override
        .map(|s| s.to_string())
        .or_else(|| {
            model_override
                .and_then(|m| m.split_once('/').map(|(p, _)| p.to_string()))
        })
        .unwrap_or_else(|| {
            let (pn, _) = config
                .effective_role_model(roles[0].model.as_deref(), agent_model);
            pn
        });

    // Apply model_override to orchestrator model_id for prompt/env
    let orc_model_id_for_prompt = model_override
        .map(|mo| {
            mo.split_once('/')
                .map(|(_, m)| m.to_string())
                .unwrap_or_else(|| mo.to_string())
        })
        .unwrap_or_else(|| orchestrator.model_id.clone());

    // Assemble shared layers (1-4, 6) using orchestrator identity
    let prompt_assembler =
        prompt::PromptAssembler::for_agent(config, Some(agent_cfg))
            .with_workspace_context(
                Some(req_ctx.workspace.clone()),
                req_ctx.repo_agents_md.clone(),
            );
    let orc_provider_name_for_closure = orc_provider_name.clone();
    let system_layers = tokio::task::spawn_blocking(move || {
        prompt_assembler.assemble_shared_layers(
            &orc_provider_name_for_closure,
            &orc_model_id_for_prompt,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("Prompt assembly panicked: {:#}", e))??;

    // Build canonical options from orchestrator (used for compaction, session metadata)
    let opts = llm::ChatOptions {
        model_id: orchestrator.model_id.clone(),
        temperature: orchestrator
            .temperature
            .unwrap_or(config.model.temperature),
        max_tokens: orchestrator.max_tokens.unwrap_or(config.model.max_tokens),
        tools: Vec::new(),
    };

    Ok(AgentContext {
        provider: Arc::clone(&orchestrator.provider),
        system_layers,
        opts,
        eff_provider: orc_provider_name,
        eff_model_id: orchestrator.model_id.clone(),
        role_loop_config: Some(agent::RoleLoopConfig {
            roles: resolved_roles,
        }),
    })
}

/// Resolve the agent context: provider, model, system prompt, and options.
///
/// Used by both `handle_socket` (primary agent) and `AgentSubagentRunner`.
pub(crate) async fn resolve_agent_context(
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

    // ── Role-aware mode ──────────────────────────────────
    if let Some((agent, roles)) =
        agent_cfg.and_then(|a| a.roles.as_ref().map(|roles| (a, roles)))
    {
        return resolve_role_aware_context(
            config,
            auth,
            merged_agents,
            req_ctx,
            agent,
            roles,
            agent_model,
            model_override,
            provider_override,
        )
        .await;
    }

    // ── Legacy mode (unchanged) ──────────────────────────
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
        role_loop_config: None,
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
    // finish_task is an internal completion marker handled by the loop.
    perms.insert("finish_task".into(), config::Permission::Allow);
    // switch_role is always allowed when the agent defines roles.
    // This tool is handled internally by the loop (not as a normal Tool trait object).
    if agent_cfg.is_some_and(|a| a.roles.is_some()) {
        perms.insert("switch_role".into(), config::Permission::Allow);
    }
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
#[allow(clippy::too_many_arguments)]
fn build_augmented_tools(
    workspace: &std::path::Path,
    repo_agents_md: Option<String>,
    auth: &auth::Auth,
    config: &config::Config,
    merged_agents: &HashMap<String, config::AgentConfig>,
    cancel_rx: &tokio::sync::watch::Receiver<bool>,
    agent_tx: &mpsc::UnboundedSender<Result<agent::AgentEvent>>,
    permission_broker: &agent::PermissionBroker,
    remembered_allow_tools: HashSet<String>,
) -> Arc<llm::ToolRegistry> {
    let mut augmented: Vec<Box<dyn llm::Tool>> =
        llm::builtin_tools(workspace.to_path_buf());
    augmented.push(Box::new(tools::UseSkillTool));
    augmented.push(Box::new(tools::FinishTaskTool));

    // Subagent tool set: builtins + use_skill (no subagent tool to prevent recursion).
    let subagent_tools: Arc<llm::ToolRegistry> = {
        let mut v = llm::builtin_tools(workspace.to_path_buf());
        v.push(Box::new(tools::UseSkillTool));
        v.push(Box::new(tools::FinishTaskTool));
        Arc::new(llm::ToolRegistry::new(v))
    };

    augmented.push(Box::new(tools::SubagentTool::new(
        tools::ToolContext {
            workspace: workspace.to_path_buf(),
        },
        Box::new(tools::AgentSubagentRunner {
            tools: subagent_tools,
            config: config.clone(),
            auth: auth.clone(),
            merged_agents: merged_agents.clone(),
            repo_agents_md,
            cancel_rx: cancel_rx.clone(),
            depth: 0,
            max_depth: 3,
            parent_events_tx: agent_tx.clone(),
            permission_broker: permission_broker.clone(),
            remembered_allow_tools,
        }),
    )));
    Arc::new(llm::ToolRegistry::new(augmented))
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

    // Attaching to an existing run needs none of the agent setup below (and
    // must not fail because of it, e.g. after a /model change mid-run).
    if let Some(run_id) = request.run_id.clone() {
        attach_socket_to_run(
            socket,
            state,
            run_id,
            request.replay_from.unwrap_or(0),
        )
        .await;
        return;
    }

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
    let session_dir = history_dir_for_session(
        &state.config.history.dir,
        &req_ctx.runtime_session_key,
    );
    let is_new_session = selected_session_id.is_none();
    let session_id = selected_session_id.unwrap_or_else(store::new_session_id);
    let session_path = store::transcript_path(&session_dir, &session_id);

    // Resolve agent: empty agent → default agent for safety
    let agent_name = if request.agent.is_empty() {
        state.config.server.default_agent.clone()
    } else {
        request.agent.clone()
    };

    // Validate agent is user-selectable
    if let Some(agent) = state.merged_agents.get(&agent_name)
        && !agent.is_user_selectable()
    {
        send_error(&mut socket, format!("Agent '{agent_name}' is a subagent-only agent and cannot be used directly. Use a primary agent and delegate via the subagent tool.")).await;
        return;
    }

    // Resolve agent context (provider, model, system prompt, options)
    let auth_guard = state.auth.read().await;
    let ctx = match resolve_agent_context(
        &state.config,
        &auth_guard,
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
    let auth_snapshot = auth_guard.clone();
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
    for tool in &remembered_allows {
        if perms.get(tool) == Some(&config::Permission::Ask) {
            perms.insert(tool.clone(), config::Permission::Allow);
        }
    }

    // Run the agent loop — pass channels for events and permission responses.
    // The run is owned by AppState, not by this websocket. Disconnecting this
    // websocket only detaches the subscriber; explicit ClientEvent::Cancel is
    // required to stop the agent.
    let (agent_tx, agent_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let permission_broker = agent::PermissionBroker::default();
    let (notes_tx, notes_rx) = mpsc::unbounded_channel();
    let run_id = new_run_id();

    let client_instance_id = match request.client_instance_id.clone() {
        Some(id) if !validate_runtime_session_key(&id) => {
            send_error(&mut socket, "Invalid client_instance_id").await;
            return;
        }
        other => other,
    };
    let claimed = {
        let mut runs = state.runs.lock().await;
        claim_run_slot(
            &mut runs,
            &run_id,
            ActiveRun::new(
                req_ctx.runtime_session_key.clone(),
                client_instance_id,
                Some(session_path.clone()),
                cancel_tx.clone(),
                permission_broker.clone(),
                notes_tx,
            ),
        )
    };
    if let Err(RunSlotConflict::SessionBusy(_)) = claimed {
        send_error(
            &mut socket,
            "This session has an active run in another window. Wait for it to finish, or start a new session.",
        )
        .await;
        return;
    }
    if let Err(RunSlotConflict::SameClient(busy_run_id)) = claimed {
        tracing::info!(
            busy_run_id = %busy_run_id,
            "client already has an active run; attaching instead of starting"
        );
        let busy = marshaling_protocol::ServerEvent::SessionBusy {
            run_id: busy_run_id.clone(),
        };
        if let Ok(json) = serde_json::to_string(&busy)
            && socket.send(Message::Text(json.into())).await.is_err()
        {
            return;
        }
        attach_socket_to_run(socket, state, busy_run_id, 0).await;
        return;
    }

    // Build augmented tools (builtins + use_skill + subagent)
    let augmented_tools = build_augmented_tools(
        &req_ctx.workspace,
        req_ctx.repo_agents_md.clone(),
        &auth_snapshot,
        &state.config,
        &state.merged_agents,
        &cancel_rx,
        &agent_tx,
        &permission_broker,
        remembered_allows,
    );

    // The conversation comes from the server-side transcript, not from the
    // client: it holds the full history the model saw (tool calls, results,
    // reasoning). `request.history` / `request.compaction` are ignored.
    let user_msg = request.message.clone();
    let session_lock = state.session_lock(&session_path).await;
    let opened = {
        let _guard = session_lock.lock().await;
        let dir = session_dir.clone();
        let id = session_id.clone();
        let path = session_path.clone();
        let provider = ctx.eff_provider.clone();
        let model_id = ctx.eff_model_id.clone();
        tokio::task::spawn_blocking(move || {
            open_run_session(dir, id, path, is_new_session, provider, model_id)
        })
        .await
        .map_err(|e| anyhow::anyhow!("session open task failed: {e}"))
        .and_then(|result| result)
    };
    let OpenedSession { writer, history } = match opened {
        Ok(opened) => opened,
        Err(e) => {
            // The run never started: release its slot.
            state.runs.lock().await.remove(&run_id);
            send_error(&mut socket, format!("{e:#}")).await;
            return;
        }
    };

    // Finalize options. `agent::run_loop` derives the advertised tool list from
    // the effective permissions so denied tools stay invisible to the model.
    let mut opts = ctx.opts;
    opts.tools = Vec::new();

    let eff_model_id = ctx.eff_model_id.clone();
    let eff_provider = ctx.eff_provider;
    let max_steps = state.config.server.max_steps;
    let augmented_tools_spawn = augmented_tools.clone();
    let prov_spawn = ctx.provider;
    let workspace_display = req_ctx.workspace_display.clone();
    let context_chars = store::history_chars(&history);

    record_run_event(
        &state,
        &run_id,
        marshaling_protocol::ServerEvent::RunStarted {
            run_id: run_id.clone(),
            session_id: Some(session_id.clone()),
        },
    )
    .await;
    record_run_event(
        &state,
        &run_id,
        marshaling_protocol::ServerEvent::ContextSize {
            chars: context_chars,
        },
    )
    .await;

    let reminder_at_end = state.config.server.system_reminder_at_end;
    let agent_span = tracing::info_span!("agent_run", run_id = %run_id);
    let run_handle = tokio::spawn(
        async move {
            agent::run_loop(
                agent::RunConfig {
                    provider: prov_spawn,
                    tools: augmented_tools_spawn,
                    system_layers: ctx.system_layers,
                    options: opts,
                    permissions: perms,
                    max_steps,
                    working_directory: workspace_display,
                    role_config: ctx.role_loop_config,
                    reminder_at_end,
                },
                agent::RunChannels {
                    events_tx: agent_tx,
                    cancel_rx,
                    permission_broker,
                    notes_rx: Some(notes_rx),
                },
                user_msg,
                history,
            )
            .await;
        }
        .instrument(agent_span),
    );

    // In role mode, the model identity is the orchestrator's (roles[0]) —
    // intentional canonical metadata for session lists.
    let save_ctx = RunPersistence {
        session_lock,
        writer: Some(writer),
        runtime_session_key: req_ctx.runtime_session_key.clone(),
        model_provider: eff_provider,
        model_id: eff_model_id,
        context_chars,
    };
    tokio::spawn(supervise_forwarder(
        Arc::clone(&state),
        run_id.clone(),
        tokio::spawn(forward_agent_events(
            Arc::clone(&state),
            run_id.clone(),
            agent_rx,
            run_handle,
            save_ctx,
        )),
    ));

    attach_socket_to_run(socket, state, run_id, 0).await;
}

// ── Startup helpers ─────────────────────────────────────

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

async fn bind_available_listener(
    start_port: u16,
) -> std::io::Result<(tokio::net::TcpListener, u16)> {
    let mut port = start_port;
    loop {
        let addr = format!("127.0.0.1:{port}");
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => return Ok((listener, port)),
            Err(e)
                if e.kind() == std::io::ErrorKind::AddrInUse
                    && port < u16::MAX =>
            {
                tracing::warn!("Port {port} is in use; trying {}", port + 1);
                port += 1;
            }
            Err(e) => return Err(e),
        }
    }
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
    if let Some(port) = server_port_override()? {
        config.server.port = port;
    }
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
        // If the log file cannot be opened, discard logs rather than fail
        // to start.
        let log_file: Box<dyn std::io::Write + Send> =
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                Ok(file) => Box::new(file),
                Err(_) => Box::new(std::io::sink()),
            };
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
        runtime_states: tokio::sync::Mutex::new(HashMap::new()),
        runs: tokio::sync::Mutex::new(HashMap::new()),
        completed_run_ids: tokio::sync::Mutex::new(VecDeque::new()),
        session_locks: tokio::sync::Mutex::new(HashMap::new()),
        session_info_cache: Arc::default(),
    });

    let configured_port = state.config.server.port;

    let app = build_router(state);

    let (listener, port) = bind_available_listener(configured_port).await?;
    if port != configured_port {
        info!(
            "Configured port {} was unavailable; using {} instead",
            configured_port, port
        );
    }
    let addr = format!("127.0.0.1:{port}");
    info!("Starting mote-server on http://{addr}");
    axum::serve(listener, app).await?;

    Ok(())
}

/// All HTTP and WebSocket routes of the server.
fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/config", get(get_config))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(load_session).delete(delete_session))
        .route("/models", get(list_models_handler))
        .route("/compact", post(compact_handler))
        .route("/audio/transcribe", get(audio_transcribe_handler))
        .route("/rollback/last", post(rollback_last_handler))
        .route("/chat", get(ws_handler))
        .route("/auth/save", post(auth_save))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

fn server_port_override() -> Result<Option<u16>> {
    let Some(raw) = std::env::var("MOTE_SERVER_PORT")
        .ok()
        .or_else(|| std::env::var("MOTE_PORT").ok())
    else {
        return Ok(None);
    };
    let port = raw
        .parse::<u16>()
        .with_context(|| format!("Invalid server port override: {raw}"))?;
    Ok(Some(port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(history_dir: std::path::PathBuf) -> config::Config {
        let mut cfg: config::Config = toml::from_str(
            r#"
[model]
provider = "ollama"
model_id = "m"

[providers.ollama]
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();
        cfg.history.dir = history_dir;
        cfg
    }

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

    fn empty_test_state(dir: &std::path::Path) -> Arc<AppState> {
        Arc::new(AppState {
            config: test_config(dir.join("history")),
            auth: RwLock::new(auth::Auth::default()),
            merged_agents: HashMap::new(),
            runtime_states: tokio::sync::Mutex::new(HashMap::new()),
            runs: tokio::sync::Mutex::new(HashMap::new()),
            completed_run_ids: tokio::sync::Mutex::new(VecDeque::new()),
            session_locks: tokio::sync::Mutex::new(HashMap::new()),
            session_info_cache: Arc::default(),
        })
    }

    fn test_save_ctx(dir: &std::path::Path) -> RunPersistence {
        RunPersistence {
            session_lock: Arc::default(),
            writer: Some(store::TranscriptWriter::create(
                store::transcript_path(
                    &dir.join("history").join("sess"),
                    "chat-test",
                ),
                "chat-test".into(),
                "p".into(),
                "m".into(),
            )),
            runtime_session_key: "sess".into(),
            model_provider: "p".into(),
            model_id: "m".into(),
            context_chars: 0,
        }
    }

    fn test_run(
        client_instance_id: Option<&str>,
    ) -> (ActiveRun, watch::Receiver<bool>) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        (
            ActiveRun::new(
                "sess".into(),
                client_instance_id.map(str::to_string),
                None,
                cancel_tx,
                agent::PermissionBroker::default(),
                mpsc::unbounded_channel().0,
            ),
            cancel_rx,
        )
    }

    async fn insert_test_run(
        state: &Arc<AppState>,
        run_id: &str,
    ) -> watch::Receiver<bool> {
        let (run, cancel_rx) = test_run(None);
        state.runs.lock().await.insert(run_id.to_string(), run);
        cancel_rx
    }

    #[test]
    fn test_claim_run_slot_rejects_second_run_for_same_client_instance() {
        let mut runs = HashMap::new();
        assert!(
            claim_run_slot(&mut runs, "run_1", test_run(Some("tui-a")).0)
                .is_ok()
        );
        assert_eq!(
            claim_run_slot(&mut runs, "run_2", test_run(Some("tui-a")).0),
            Err(RunSlotConflict::SameClient("run_1".to_string()))
        );
        assert!(!runs.contains_key("run_2"));
        // Other instances and requests without an instance id are unaffected.
        assert!(
            claim_run_slot(&mut runs, "run_3", test_run(Some("tui-b")).0)
                .is_ok()
        );
        assert!(claim_run_slot(&mut runs, "run_4", test_run(None).0).is_ok());
        assert!(claim_run_slot(&mut runs, "run_5", test_run(None).0).is_ok());
    }

    const LEGACY_MD: &str = "---\nid: chat-old\ncreated: 2026-05-25T23:36:58Z\nupdated: 2026-05-25T23:40:00Z\nmodel_provider: ollama\nmodel_id: qwen\ntokens_input: 10\ntokens_output: 5\nversion: 0.1.0\nsummary: What is 99-1?\n---\n\n## User — 23:36:58\nWhat is 99-1?\n\n## Assistant — 23:36:59\n98\n";

    fn write_transcript(
        dir: &std::path::Path,
        id: &str,
        messages: &[llm::ChatMessage],
    ) {
        let mut writer = store::TranscriptWriter::create(
            store::transcript_path(dir, id),
            id.into(),
            "p".into(),
            "m".into(),
        );
        writer.append_messages(messages).unwrap();
    }

    #[test]
    fn test_claim_run_slot_rejects_second_run_on_same_session() {
        let mut runs = HashMap::new();
        let with_session = |instance: &str| {
            let (mut run, _) = test_run(Some(instance));
            run.session_path = Some(PathBuf::from("/h/k/chat-1.jsonl"));
            run
        };
        claim_run_slot(&mut runs, "run_1", with_session("tui-a")).unwrap();
        assert_eq!(
            claim_run_slot(&mut runs, "run_2", with_session("tui-b")),
            Err(RunSlotConflict::SessionBusy("run_1".into()))
        );
        runs.get_mut("run_1").unwrap().finished = true;
        assert!(
            claim_run_slot(&mut runs, "run_3", with_session("tui-b")).is_ok()
        );
    }

    #[test]
    fn test_open_run_session_new_starts_empty_and_writes_lazily() {
        let dir = tempfile::tempdir().unwrap();
        let path = store::transcript_path(dir.path(), "chat-new");
        let OpenedSession {
            writer, history, ..
        } = open_run_session(
            dir.path().to_path_buf(),
            "chat-new".into(),
            path.clone(),
            true,
            "p".into(),
            "m".into(),
        )
        .unwrap();
        assert!(history.is_empty());
        assert_eq!(writer.path(), path.as_path());
        assert!(!path.exists(), "nothing written before the first commit");
    }

    #[test]
    fn test_open_run_session_unknown_id_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_run_session(
            dir.path().to_path_buf(),
            "chat-missing".into(),
            store::transcript_path(dir.path(), "chat-missing"),
            false,
            "p".into(),
            "m".into(),
        )
        .err()
        .unwrap();
        assert!(format!("{err:#}").contains("Unknown session"));
    }

    #[test]
    fn test_legacy_md_session_converts_once_and_keeps_md() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("chat-old.md");
        std::fs::write(&md, LEGACY_MD).unwrap();
        let OpenedSession { history, .. } = open_run_session(
            dir.path().to_path_buf(),
            "chat-old".into(),
            store::transcript_path(dir.path(), "chat-old"),
            false,
            "p".into(),
            "m".into(),
        )
        .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content.as_deref(), Some("98"));
        assert!(store::transcript_path(dir.path(), "chat-old").exists());
        assert_eq!(std::fs::read_to_string(&md).unwrap(), LEGACY_MD);
        // Loading again uses the transcript, not a second conversion.
        let transcript =
            load_or_convert_session(dir.path(), "chat-old").unwrap();
        assert_eq!(transcript.messages.len(), 2);
        assert_eq!(transcript.tokens_input, 10);
        // The session is listed once, from the transcript.
        let infos = session_infos(dir.path(), &Default::default());
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, "chat-old");
        assert_eq!(infos[0].message_count, 2);
    }

    #[test]
    fn test_session_listing_and_loading_cover_transcripts_and_legacy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("chat-old.md"), LEGACY_MD).unwrap();
        write_transcript(
            dir.path(),
            "chat-new",
            &[
                llm::ChatMessage::user("add a test"),
                llm::ChatMessage::assistant_tool_calls_with_content(
                    vec![llm::ToolCall {
                        id: "c1".into(),
                        call_type: "function".into(),
                        function: llm::ToolFunction {
                            name: "read".into(),
                            arguments: "{}".into(),
                        },
                    }],
                    None,
                ),
                llm::ChatMessage::tool_result("c1", "contents"),
                llm::ChatMessage::assistant_text("Added."),
            ],
        );
        let mut ids: Vec<String> =
            session_infos(dir.path(), &Default::default())
                .into_iter()
                .map(|i| i.id)
                .collect();
        ids.sort();
        assert_eq!(ids, ["chat-new", "chat-old"]);

        let data = load_session_data(dir.path(), "chat-new").unwrap();
        let shown: Vec<(&str, &str)> = data
            .messages
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(shown, [("user", "add a test"), ("assistant", "Added.")]);
        assert!(data.compaction.is_none());

        let legacy = load_session_data(dir.path(), "chat-old").unwrap();
        assert_eq!(legacy.messages.len(), 2);
        assert!(load_session_data(dir.path(), "chat-none").is_none());
    }

    #[test]
    fn test_session_list_cache_follows_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = std::sync::Mutex::new(SessionInfoCache::new());
        write_transcript(
            dir.path(),
            "chat-1",
            &[llm::ChatMessage::user("one")],
        );
        assert_eq!(session_infos(dir.path(), &cache)[0].message_count, 1);
        assert_eq!(cache.lock().unwrap().len(), 1);

        // An unchanged file is served from the cache.
        let path = store::transcript_path(dir.path(), "chat-1");
        cache.lock().unwrap().get_mut(&path).unwrap().2.summary =
            Some("from cache".into());
        assert_eq!(
            session_infos(dir.path(), &cache)[0].summary.as_deref(),
            Some("from cache")
        );

        // A changed file is parsed again.
        let transcript = store::load(&path).unwrap();
        store::TranscriptWriter::resume(path.clone(), &transcript)
            .append_messages(&[llm::ChatMessage::assistant_text("two")])
            .unwrap();
        let infos = session_infos(dir.path(), &cache);
        assert_eq!(infos[0].message_count, 2);
        assert_eq!(infos[0].summary.as_deref(), Some("one"));

        // A deleted file is forgotten.
        std::fs::remove_file(&path).unwrap();
        assert!(session_infos(dir.path(), &cache).is_empty());
        assert!(cache.lock().unwrap().is_empty());
    }

    #[test]
    fn test_model_history_replaces_compacted_range_with_summary() {
        let dir = tempfile::tempdir().unwrap();
        write_transcript(
            dir.path(),
            "chat-1",
            &[
                llm::ChatMessage::user("first"),
                llm::ChatMessage::assistant_text("one"),
                llm::ChatMessage::user("second"),
                llm::ChatMessage::assistant_text("two"),
            ],
        );
        let path = store::transcript_path(dir.path(), "chat-1");
        store::append(
            &path,
            &[store::Record::Compaction {
                ts: chrono::Utc::now(),
                upto_seq: 1,
                summary: "they said first/one".into(),
                model_provider: "p".into(),
                model_id: "m".into(),
            }],
            false,
        )
        .unwrap();
        let transcript = store::load(&path).unwrap();
        let history = model_history(&transcript, None);
        assert_eq!(history.len(), 3);
        let summary = history[0].content.as_deref().unwrap();
        assert!(summary.starts_with(COMPACTION_CONTEXT_MARKER));
        assert!(summary.contains("they said first/one"));
        assert_eq!(history[1].content.as_deref(), Some("second"));
        assert_eq!(
            protocol_compaction(&transcript)
                .unwrap()
                .compacted_message_count,
            2
        );
    }

    #[test]
    fn test_model_history_answers_calls_lost_to_a_truncated_step() {
        let dir = tempfile::tempdir().unwrap();
        // A step whose tool result never reached the disk.
        write_transcript(
            dir.path(),
            "chat-1",
            &[
                llm::ChatMessage::user("go"),
                llm::ChatMessage::assistant_tool_calls_with_content(
                    vec![llm::ToolCall {
                        id: "c1".into(),
                        call_type: "function".into(),
                        function: llm::ToolFunction {
                            name: "read".into(),
                            arguments: "{}".into(),
                        },
                    }],
                    None,
                ),
            ],
        );
        let transcript =
            store::load(&store::transcript_path(dir.path(), "chat-1")).unwrap();
        let history = model_history(&transcript, None);
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn test_compaction_cut_stops_at_step_boundary_within_cap() {
        let big = "y".repeat(MAX_COMPACT_CONVERSATION_CHARS / 2);
        let call = |id: &str| {
            llm::ChatMessage::assistant_tool_calls_with_content(
                vec![llm::ToolCall {
                    id: id.into(),
                    call_type: "function".into(),
                    function: llm::ToolFunction {
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                }],
                None,
            )
        };
        let messages = [
            llm::ChatMessage::user("first"),
            call("a"),
            llm::ChatMessage::tool_result("a", "ok"),
            llm::ChatMessage::user(format!("second {big}")),
            call("b"),
            llm::ChatMessage::tool_result("b", "ok"),
            llm::ChatMessage::user(format!("third {big}")),
            llm::ChatMessage::assistant_text("done"),
        ];
        // Steps 1-2 fit; step 3 would exceed the cap, so it stays verbatim.
        assert_eq!(compaction_cut(&messages), 6);
        // Everything fits: compact it all.
        assert_eq!(compaction_cut(&messages[..3]), 3);
        // A single oversized step is still compacted, but never split.
        let huge = [
            llm::ChatMessage::user(
                "x".repeat(MAX_COMPACT_CONVERSATION_CHARS * 2),
            ),
            call("c"),
            llm::ChatMessage::tool_result("c", "ok"),
            llm::ChatMessage::user("next"),
        ];
        assert_eq!(compaction_cut(&huge), 3);
    }

    #[test]
    fn test_repair_happens_before_compaction_can_cover_the_step() {
        let dir = tempfile::tempdir().unwrap();
        write_transcript(
            dir.path(),
            "chat-1",
            &[
                llm::ChatMessage::user("go"),
                llm::ChatMessage::assistant_tool_calls_with_content(
                    vec![llm::ToolCall {
                        id: "c1".into(),
                        call_type: "function".into(),
                        function: llm::ToolFunction {
                            name: "read".into(),
                            arguments: "{}".into(),
                        },
                    }],
                    None,
                ),
            ],
        );
        // Compaction loads the session first: the repair lands then, so the
        // compacted range includes the call *and* its result.
        let transcript = load_or_convert_session(dir.path(), "chat-1").unwrap();
        let last = transcript.messages.last().unwrap();
        assert_eq!(last.message.tool_call_id.as_deref(), Some("c1"));
        store::append(
            &store::transcript_path(dir.path(), "chat-1"),
            &[store::Record::Compaction {
                ts: chrono::Utc::now(),
                upto_seq: last.seq,
                summary: "s".into(),
                model_provider: "p".into(),
                model_id: "m".into(),
            }],
            false,
        )
        .unwrap();
        // The next run sees only the summary: no orphaned tool result.
        let OpenedSession { history, .. } = open_run_session(
            dir.path().to_path_buf(),
            "chat-1".into(),
            store::transcript_path(dir.path(), "chat-1"),
            false,
            "p".into(),
            "m".into(),
        )
        .unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].tool_call_id.is_none());
    }

    #[test]
    fn test_open_run_session_persists_repair_of_truncated_step() {
        let dir = tempfile::tempdir().unwrap();
        let call = llm::ChatMessage::assistant_tool_calls_with_content(
            vec![
                llm::ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: llm::ToolFunction {
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                },
                llm::ToolCall {
                    id: "c2".into(),
                    call_type: "function".into(),
                    function: llm::ToolFunction {
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                },
            ],
            None,
        );
        // c2's result never reached the disk.
        write_transcript(
            dir.path(),
            "chat-1",
            &[
                llm::ChatMessage::user("go"),
                call,
                llm::ChatMessage::tool_result("c1", "ok"),
            ],
        );
        let path = store::transcript_path(dir.path(), "chat-1");
        let OpenedSession {
            mut writer,
            history,
            ..
        } = open_run_session(
            dir.path().to_path_buf(),
            "chat-1".into(),
            path.clone(),
            false,
            "p".into(),
            "m".into(),
        )
        .unwrap();
        assert_eq!(history.last().unwrap().tool_call_id.as_deref(), Some("c2"));
        // A later run appends more steps; the repair is already on disk, in
        // the right place, so the history stays valid on every later load.
        writer
            .append_messages(&[
                llm::ChatMessage::user("more"),
                llm::ChatMessage::assistant_text("sure"),
            ])
            .unwrap();
        let transcript = store::load(&path).unwrap();
        let ids: Vec<Option<&str>> = transcript
            .messages
            .iter()
            .map(|m| m.message.tool_call_id.as_deref())
            .collect();
        assert_eq!(ids, [None, None, Some("c1"), Some("c2"), None, None]);
    }

    #[tokio::test]
    async fn test_forwarder_reports_context_size_after_commits() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let _cancel_rx = insert_test_run(&state, "run_1").await;
        let mut save_ctx = test_save_ctx(dir.path());
        save_ctx.context_chars = 100;
        forward_agent_event(
            &state,
            "run_1",
            &mut save_ctx,
            Ok(agent::AgentEvent::MessagesCommitted(vec![
                llm::ChatMessage::user("12345"),
            ])),
        )
        .await;
        let runs = state.runs.lock().await;
        assert!(matches!(
            runs["run_1"].events.last(),
            Some(marshaling_protocol::ServerEvent::ContextSize { chars: 105 })
        ));
    }

    #[test]
    fn test_model_history_drops_reasoning_after_a_provider_switch() {
        let dir = tempfile::tempdir().unwrap();
        let mut thinking = llm::ChatMessage::assistant_text("answer");
        thinking.reasoning_content = Some("provider-specific thoughts".into());
        write_transcript(
            dir.path(),
            "chat-1",
            &[llm::ChatMessage::user("q"), thinking],
        );
        let transcript =
            store::load(&store::transcript_path(dir.path(), "chat-1")).unwrap();
        // `write_transcript` records provider "p".
        let same = model_history(&transcript, Some("p"));
        assert!(same[1].reasoning_content.is_some());
        let switched = model_history(&transcript, Some("other"));
        assert!(switched[1].reasoning_content.is_none());
        assert_eq!(switched[1].content.as_deref(), Some("answer"));
    }

    #[test]
    fn test_compact_transcript_text_includes_tool_activity() {
        let long_result = "x".repeat(COMPACT_TOOL_RESULT_CHARS + 100);
        let messages = [
            llm::ChatMessage::user("fix main.rs"),
            llm::ChatMessage::assistant_tool_calls_with_content(
                vec![llm::ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: llm::ToolFunction {
                        name: "read".into(),
                        arguments: r#"{"file_path":"src/main.rs"}"#.into(),
                    },
                }],
                Some("Reading it.".into()),
            ),
            llm::ChatMessage::tool_result("c1", long_result.clone()),
            llm::ChatMessage::role_task("review it"),
        ];
        let text = compact_transcript_text(Some("earlier summary"), &messages);
        assert!(text.contains("<previous_compaction>\nearlier summary"));
        assert!(text.contains("USER:\nfix main.rs"));
        assert!(text.contains("ASSISTANT:\nReading it."));
        assert!(
            text.contains(
                r#"ASSISTANT called read({"file_path":"src/main.rs"})"#
            )
        );
        assert!(text.contains("TOOL RESULT:\n"));
        assert!(!text.contains(&long_result), "tool results are excerpted");
        assert!(text.contains("INTERNAL NOTE:\nreview it"));
    }

    #[test]
    fn test_claim_run_slot_allows_new_run_after_previous_finished() {
        let mut runs = HashMap::new();
        claim_run_slot(&mut runs, "run_1", test_run(Some("tui-a")).0).unwrap();
        runs.get_mut("run_1").unwrap().finished = true;
        assert!(
            claim_run_slot(&mut runs, "run_2", test_run(Some("tui-a")).0)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_record_run_event_marks_finished_and_clears_pending_permissions()
     {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let _cancel_rx = insert_test_run(&state, "run_1").await;
        // Keep a subscriber so no detached watchdog is armed.
        let _sub = state.runs.lock().await["run_1"].tx.subscribe();
        record_run_event(
            &state,
            "run_1",
            marshaling_protocol::ServerEvent::PermissionRequest {
                id: "perm_1".into(),
                tool_name: "bash".into(),
                args: serde_json::json!({}),
            },
        )
        .await;
        assert_eq!(
            state.runs.lock().await["run_1"]
                .pending_permission_tools
                .len(),
            1
        );
        record_run_event(
            &state,
            "run_1",
            marshaling_protocol::ServerEvent::Cancelled {
                content: String::new(),
                tokens_input: 0,
                tokens_output: 0,
            },
        )
        .await;
        let runs = state.runs.lock().await;
        assert!(runs["run_1"].finished);
        assert!(runs["run_1"].pending_permission_tools.is_empty());
    }

    #[tokio::test]
    async fn test_detached_permission_request_cancels_run_after_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path().join("history"));
        config.server.detached_permission_timeout_secs = 0;
        let state = Arc::new(AppState {
            config,
            auth: RwLock::new(auth::Auth::default()),
            merged_agents: HashMap::new(),
            runtime_states: tokio::sync::Mutex::new(HashMap::new()),
            runs: tokio::sync::Mutex::new(HashMap::new()),
            completed_run_ids: tokio::sync::Mutex::new(VecDeque::new()),
            session_locks: tokio::sync::Mutex::new(HashMap::new()),
            session_info_cache: Arc::default(),
        });
        let mut cancel_rx = insert_test_run(&state, "run_1").await;
        record_run_event(
            &state,
            "run_1",
            marshaling_protocol::ServerEvent::PermissionRequest {
                id: "perm_1".into(),
                tool_name: "bash".into(),
                args: serde_json::json!({}),
            },
        )
        .await;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cancel_rx.changed(),
        )
        .await
        .expect("watchdog should cancel the detached run")
        .unwrap();
        assert!(*cancel_rx.borrow());
    }

    fn permission_request(id: &str) -> marshaling_protocol::ServerEvent {
        marshaling_protocol::ServerEvent::PermissionRequest {
            id: id.into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"command": "ls"}),
        }
    }

    #[tokio::test]
    async fn test_attach_replays_from_offset_and_reannounces_pending_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let _cancel_rx = insert_test_run(&state, "run_1").await;
        let _sub = state.runs.lock().await["run_1"].tx.subscribe();
        for event in [
            marshaling_protocol::ServerEvent::RunStarted {
                run_id: "run_1".into(),
                session_id: None,
            },
            marshaling_protocol::ServerEvent::TextDelta { data: "a".into() },
            permission_request("perm_0_c"),
        ] {
            record_run_event(&state, "run_1", event).await;
        }

        let mut runs = state.runs.lock().await;
        let run = runs.get_mut("run_1").unwrap();
        let (_rx, replay, pending) = run.attach(3);
        assert!(replay.is_empty(), "client already has all 3 events");
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            &pending[0],
            marshaling_protocol::ServerEvent::PermissionPending { id, tool_name, args }
                if id == "perm_0_c" && tool_name == "bash" && args["command"] == "ls"
        ));
        let (_rx, replay, _) = run.attach(1);
        assert_eq!(replay.len(), 2);
        assert!(!pending[0].is_run_log_event());
        assert_eq!(run.attach_generation, 2);
    }

    #[tokio::test]
    async fn test_attach_to_finished_run_always_replays_terminal_event() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let _cancel_rx = insert_test_run(&state, "run_1").await;
        record_run_event(
            &state,
            "run_1",
            marshaling_protocol::ServerEvent::TextDelta { data: "a".into() },
        )
        .await;
        record_run_event(
            &state,
            "run_1",
            marshaling_protocol::ServerEvent::Error {
                message: "boom".into(),
            },
        )
        .await;
        let mut runs = state.runs.lock().await;
        let run = runs.get_mut("run_1").unwrap();
        for replay_from in [2, 5] {
            let (_rx, replay, pending) = run.attach(replay_from);
            assert_eq!(replay.len(), 1);
            assert!(is_terminal_event(&replay[0]));
            assert!(pending.is_empty());
        }
    }

    #[tokio::test]
    async fn test_forwarder_persists_commits_and_records_failure() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let _cancel_rx = insert_test_run(&state, "run_1").await;
        let mut save_ctx = test_save_ctx(dir.path());
        let path = save_ctx.writer.as_ref().unwrap().path().to_path_buf();
        let committed = forward_agent_event(
            &state,
            "run_1",
            &mut save_ctx,
            Ok(agent::AgentEvent::MessagesCommitted(vec![
                llm::ChatMessage::user("do the thing"),
                llm::ChatMessage::assistant_text("working on it"),
            ])),
        )
        .await;
        assert!(!committed);
        let terminal = forward_agent_event(
            &state,
            "run_1",
            &mut save_ctx,
            Ok(agent::AgentEvent::Failed {
                error: anyhow::anyhow!("provider API error (401)"),
                tokens_input: 3,
                tokens_output: 4,
            }),
        )
        .await;
        assert!(terminal);
        {
            let runs = state.runs.lock().await;
            assert!(runs["run_1"].finished);
            assert!(matches!(
                runs["run_1"].events.last(),
                Some(marshaling_protocol::ServerEvent::Error { message })
                    if message.contains("401")
            ));
        }
        // Writes are awaited, so the transcript is complete right away.
        let transcript = store::load(&path).unwrap();
        assert_eq!(transcript.messages.len(), 2);
        assert_eq!(
            transcript.messages[1].message.content.as_deref(),
            Some("working on it")
        );
        assert_eq!((transcript.tokens_input, transcript.tokens_output), (3, 4));
    }

    #[tokio::test]
    async fn test_subagent_edits_are_journaled_for_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let _cancel_rx = insert_test_run(&state, "run_1").await;
        let mut save_ctx = test_save_ctx(dir.path());
        forward_agent_event(
            &state,
            "run_1",
            &mut save_ctx,
            Ok(agent::AgentEvent::SubagentToolCompleted {
                id: "sub_1".into(),
                sub_id: "call_9".into(),
                tool_name: "edit".into(),
                result: "Edited a.txt".into(),
                changes: Vec::new(),
                rollback_entries: vec![llm::RollbackEntry {
                    path: dir.path().join("a.txt"),
                    kind: llm::RollbackKind::Modified,
                    before_content: Some("before".into()),
                    expected_after_hash: Some(hash64("after")),
                }],
            }),
        )
        .await;
        let sessions = state.runtime_states.lock().await;
        let journal = &sessions["sess"].rollback_journal;
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].tool_name, "edit");
        assert!(matches!(
            state.runs.lock().await["run_1"].events.last(),
            Some(
                marshaling_protocol::ServerEvent::SubagentToolCompleted { .. }
            )
        ));
    }

    #[tokio::test]
    async fn test_late_rollback_note_is_parked_then_written_by_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let mut save_ctx = test_save_ctx(dir.path());
        let path = save_ctx.writer.as_ref().unwrap().path().to_path_buf();
        // A run on this session whose loop no longer takes notes (the
        // receiver is gone, as after `finish()`).
        let (mut run, _cancel_rx) = test_run(None);
        run.session_path = Some(path.clone());
        state.runs.lock().await.insert("run_1".into(), run);
        let change = RollbackChangeSet {
            id: "call_1".into(),
            tool_name: "edit".into(),
            entries: vec![llm::RollbackEntry {
                path: dir.path().join("a.txt"),
                kind: llm::RollbackKind::Modified,
                before_content: None,
                expected_after_hash: None,
            }],
            display_changes: Vec::new(),
            transcript: Some(path.clone()),
        };

        note_rollback_in_transcript(&state, &change).await;
        assert_eq!(state.runs.lock().await["run_1"].pending_notes.len(), 1);

        // The run ends: the forwarder writes the parked note itself.
        save_ctx.flush_pending_notes(&state, "run_1").await;
        let transcript = store::load(&path).unwrap();
        assert_eq!(transcript.messages.len(), 1);
        assert!(transcript.messages[0].message.internal_role_task);
        assert!(!state.runs.lock().await["run_1"].accepting_notes);

        // Later notes go straight to the transcript, with the next seq.
        note_rollback_in_transcript(&state, &change).await;
        let transcript = store::load(&path).unwrap();
        let seqs: Vec<u64> =
            transcript.messages.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, [0, 1]);
    }

    #[tokio::test]
    async fn test_permission_resolved_clears_pending_and_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let _cancel_rx = insert_test_run(&state, "run_1").await;
        let _sub = state.runs.lock().await["run_1"].tx.subscribe();
        record_run_event(&state, "run_1", permission_request("perm_0_c")).await;
        let mut save_ctx = test_save_ctx(dir.path());
        let terminal = forward_agent_event(
            &state,
            "run_1",
            &mut save_ctx,
            Ok(agent::AgentEvent::PermissionResolved {
                id: "perm_0_c".into(),
            }),
        )
        .await;
        assert!(!terminal);
        let runs = state.runs.lock().await;
        assert!(runs["run_1"].pending_permission_tools.is_empty());
        assert_eq!(runs["run_1"].events.len(), 1, "internal event not logged");
    }

    #[test]
    fn test_detached_watchdog_is_armed_once_per_generation() {
        let (mut run, _cancel_rx) = test_run(None);
        run.pending_permission_tools.insert(
            "perm_0_c".into(),
            PendingPermission {
                tool_name: "bash".into(),
                args: serde_json::json!({}),
            },
        );
        assert_eq!(run.arm_detached_permission_watchdog(), Some(0));
        assert_eq!(run.arm_detached_permission_watchdog(), None);
        run.attach_generation = 1;
        assert_eq!(run.arm_detached_permission_watchdog(), Some(1));
    }

    #[tokio::test]
    async fn test_forwarder_panic_cancels_run_and_records_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let mut cancel_rx = insert_test_run(&state, "run_1").await;
        let forwarder = tokio::spawn(async {
            panic!("save exploded");
        });
        supervise_forwarder(Arc::clone(&state), "run_1".into(), forwarder)
            .await;
        assert!(cancel_rx.has_changed().unwrap());
        assert!(*cancel_rx.borrow_and_update());
        let runs = state.runs.lock().await;
        assert!(runs["run_1"].finished);
        assert!(matches!(
            runs["run_1"].events.last(),
            Some(marshaling_protocol::ServerEvent::Error { message })
                if message.contains("save exploded")
        ));
    }

    #[tokio::test]
    async fn test_detached_watchdog_skips_run_that_was_reattached() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path().join("history"));
        config.server.detached_permission_timeout_secs = 0;
        let state = Arc::new(AppState {
            config,
            auth: RwLock::new(auth::Auth::default()),
            merged_agents: HashMap::new(),
            runtime_states: tokio::sync::Mutex::new(HashMap::new()),
            runs: tokio::sync::Mutex::new(HashMap::new()),
            completed_run_ids: tokio::sync::Mutex::new(VecDeque::new()),
            session_locks: tokio::sync::Mutex::new(HashMap::new()),
            session_info_cache: Arc::default(),
        });
        let cancel_rx = insert_test_run(&state, "run_1").await;
        {
            let mut runs = state.runs.lock().await;
            let run = runs.get_mut("run_1").unwrap();
            run.pending_permission_tools.insert(
                "perm_1".into(),
                PendingPermission {
                    tool_name: "bash".into(),
                    args: serde_json::json!({}),
                },
            );
            // A client attached after the watchdog for generation 0 was armed.
            run.attach_generation = 1;
        }
        spawn_detached_permission_watchdog(&state, "run_1", 0);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!*cancel_rx.borrow());
    }

    #[tokio::test]
    async fn test_forwarder_records_error_when_run_task_panics() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let _cancel_rx = insert_test_run(&state, "run_1").await;

        // Keep an extra sender alive (like the subagent tool's clone) so the
        // channel never closes; the forwarder must notice via the handle.
        let (agent_tx, agent_rx) = mpsc::unbounded_channel();
        let _extra_sender = agent_tx.clone();
        let run_handle = tokio::spawn(async move {
            let _ =
                agent_tx.send(Ok(agent::AgentEvent::TextDelta("hi".into())));
            panic!("loop exploded");
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            forward_agent_events(
                Arc::clone(&state),
                "run_1".into(),
                agent_rx,
                run_handle,
                test_save_ctx(dir.path()),
            ),
        )
        .await
        .expect("forwarder must stop once the run task dies");

        let runs = state.runs.lock().await;
        let events = &runs["run_1"].events;
        assert!(matches!(
            events.first(),
            Some(marshaling_protocol::ServerEvent::TextDelta { .. })
        ));
        match events.last() {
            Some(marshaling_protocol::ServerEvent::Error { message }) => {
                assert!(message.contains("loop exploded"), "{message}");
            }
            other => panic!("expected terminal Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_forwarder_drains_terminal_event_sent_before_task_exit() {
        let dir = tempfile::tempdir().unwrap();
        let state = empty_test_state(dir.path());
        let _cancel_rx = insert_test_run(&state, "run_1").await;

        let (agent_tx, agent_rx) = mpsc::unbounded_channel();
        let _extra_sender = agent_tx.clone();
        let run_handle = tokio::spawn(async move {
            let _ = agent_tx.send(Ok(agent::AgentEvent::Cancelled {
                content: "(cancelled)".into(),
                tokens_input: 0,
                tokens_output: 0,
            }));
        });
        // Let the task finish before the forwarder polls, so the handle can
        // win the race against the buffered terminal event.
        tokio::task::yield_now().await;

        forward_agent_events(
            Arc::clone(&state),
            "run_1".into(),
            agent_rx,
            run_handle,
            test_save_ctx(dir.path()),
        )
        .await;

        let runs = state.runs.lock().await;
        let events = &runs["run_1"].events;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            marshaling_protocol::ServerEvent::Cancelled { .. }
        ));
    }

    #[tokio::test]
    async fn test_rollback_conflict_preserves_journal_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("edited.txt");
        tokio::fs::write(&file_path, "changed by user")
            .await
            .unwrap();

        let state = Arc::new(AppState {
            config: test_config(dir.path().join("history")),
            auth: RwLock::new(auth::Auth::default()),
            merged_agents: HashMap::new(),
            runtime_states: tokio::sync::Mutex::new(HashMap::from([(
                "sess".to_string(),
                RuntimeSessionState {
                    rollback_journal: vec![RollbackChangeSet {
                        id: "tool_1".into(),
                        tool_name: "edit".into(),
                        entries: vec![llm::RollbackEntry {
                            path: file_path,
                            kind: llm::RollbackKind::Modified,
                            before_content: Some("before".into()),
                            expected_after_hash: Some(hash64("after")),
                        }],
                        display_changes: Vec::new(),
                        transcript: None,
                    }],
                    remember_allow_tools: HashSet::new(),
                },
            )])),
            runs: tokio::sync::Mutex::new(HashMap::new()),
            completed_run_ids: tokio::sync::Mutex::new(VecDeque::new()),
            session_locks: tokio::sync::Mutex::new(HashMap::new()),
            session_info_cache: Arc::default(),
        });

        let result = apply_rollback_last(&state, "sess").await;

        assert!(!result.success);
        let sessions = state.runtime_states.lock().await;
        assert_eq!(sessions["sess"].rollback_journal.len(), 1);
    }

    #[test]
    fn test_build_permission_map_includes_switch_role_for_role_agent() {
        let toml = r#"
[model]
provider = "test"
model_id = "test-model"

[providers.ollama]
base_url = "http://localhost:11434"
"#;
        let config: config::Config = toml::from_str(toml).unwrap();
        let tool_names = vec!["read".to_string(), "edit".to_string()];

        // Agent with roles
        let agent_with_roles = config::AgentConfig {
            roles: Some(vec![config::RoleConfig {
                name: "orchestrator".into(),
                instructions: Some("Plan".into()),
                model: None,
                temperature: None,
                max_tokens: None,
            }]),
            ..Default::default()
        };
        let perms =
            build_permission_map(&config, Some(&agent_with_roles), &tool_names);
        assert_eq!(perms.get("switch_role"), Some(&config::Permission::Allow));

        // Agent without roles
        let agent_no_roles = config::AgentConfig {
            roles: None,
            ..Default::default()
        };
        let perms2 =
            build_permission_map(&config, Some(&agent_no_roles), &tool_names);
        assert!(
            !perms2.contains_key("switch_role")
                || perms2.get("switch_role")
                    != Some(&config::Permission::Allow)
        );

        // No agent at all
        let perms3 = build_permission_map(&config, None, &tool_names);
        assert!(
            !perms3.contains_key("switch_role")
                || perms3.get("switch_role")
                    != Some(&config::Permission::Allow)
        );
    }
}
