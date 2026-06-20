use crate::llm::Role;
use marshaling_protocol::{ModelInfo, ToolCallDisplay, ToolStatus};
use ratatui::style::Color;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// Server connection health.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerHealth {
    Unknown,
    Connected,
    Disconnected(String),
}

// ── Display message ───────────────────────────────────────

/// What generated this display message — used to filter history sent to the LLM.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageSource {
    /// Real user message or assistant response from the agent.
    Conversation,
    /// Slash command output (/help, /agent, /tokens, etc.) — do not send to LLM.
    Command,
    /// Error message — do not send to LLM.
    Error,
}

#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: Role,
    pub content: String,
    /// Reasoning/thinking text for this message (rendered grey above the content).
    pub thinking: Option<String>,
    /// Source of this message — determines whether it's included in LLM history.
    pub source: MessageSource,
}

impl DisplayMessage {
    pub fn command(role: Role, content: String) -> Self {
        Self {
            role,
            content,
            thinking: None,
            source: MessageSource::Command,
        }
    }
}

// ── App state ─────────────────────────────────────────────

pub struct App {
    pub state: AppState,
    pub messages: Vec<DisplayMessage>,
    pub input: String,
    pub input_cursor: usize,
    /// Scroll offset from the bottom in lines. 0 = at bottom (showing newest content).
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub model_info: String,
    pub default_model_info: String,
    pub agent_model_info: HashMap<String, String>,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub stream_buffer: String,
    /// Streaming reasoning/thinking text (displayed with grey styling).
    pub reasoning_buffer: String,
    pub input_history: Vec<String>,
    input_history_idx: Option<usize>,

    pub current_agent: String,
    pub agent_names: Vec<String>,
    /// Subagent names (agents available as subagent targets).
    pub subagent_names: Vec<String>,
    pub handled_slash_command: bool,
    pub suggestions: Vec<String>,
    pub suggestion_index: usize,

    /// Tool calls currently being executed (during agent loop).
    pub tool_calls: Vec<ToolCallDisplay>,

    /// Per-agent model overrides selected by the user.
    pub agent_model_overrides: HashMap<String, AgentModelOverride>,

    /// Cached (provider, model_id) pairs from the last /model fetch.
    pub models_cache: Vec<(String, String)>,

    /// Accent bar color for input area.
    pub input_accent: Color,
    /// Accent bar color for user messages.
    pub user_accent: Color,

    /// Pending async slash action (e.g., fetching models).
    pub pending_slash: Option<SlashAction>,

    /// Queued user messages (entered while agent was running).
    pub input_queue: VecDeque<String>,

    /// Loading bar progress: Some(0.0..1.0) = active, None = idle.
    pub loading_progress: Option<f32>,

    /// Server connection health.
    pub server_health: ServerHealth,

    /// Pending permission request waiting for user response.
    pub pending_permission: Option<PendingPermission>,

    /// Queued permission response to send to the server (processed in event loop).
    /// Tuple: (permission_id, allowed, remember_for_session_tool)
    pub pending_permission_response: Option<(String, bool, bool)>,

    /// Tool names that have been "Allow Always"ed — auto-allowed for this session.
    pub auto_allowed_tools: HashSet<String>,

    /// Currently selected skill name (set by use_skill tool).
    pub current_skill: Option<String>,

    /// All subagent views (running or finished). Each subagent gets its own screen.
    pub subagent_views: Vec<SubagentView>,

    /// Which subagent is currently being viewed. None = primary view.
    pub current_subagent_index: Option<usize>,

    /// Whether the user has requested cancellation of the running agent.
    pub pending_cancel: bool,

    /// Double-press Esc to cancel while agent is running.
    esc_cancel_deadline: Option<Instant>,

    /// Workspace root for this client session (client launch dir).
    pub workspace_root: String,

    /// Repo-local AGENTS.md content loaded by the client (if present).
    pub repo_agents_md: Option<String>,

    /// Session key used by server to scope runtime mutable state.
    pub runtime_session_key: String,

    /// Session picker popup state.
    pub session_picker_open: bool,
    pub session_picker_items: Vec<marshaling_protocol::SessionInfo>,
    pub session_picker_index: usize,

    /// Model picker popup state.
    pub model_picker_open: bool,
    pub model_picker_items: Vec<ModelChoice>,
    pub model_picker_index: usize,

    /// Active session id to continue on next turns.
    pub active_session_id: Option<String>,
}

/// Tracks a running subagent's output for the multi-agent TUI.
#[derive(Debug, Clone)]
pub struct SubagentView {
    pub id: String,
    pub name: String,
    pub stream_buffer: String,
    pub reasoning_buffer: String,
    pub tool_calls: Vec<ToolCallDisplay>,
    pub done: bool,
    pub content: String,
}

/// A permission request awaiting user response.
#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub id: String,
    pub tool_name: String,
    pub args: String,
    /// True when the user pressed 'A' and is confirming "allow always".
    pub confirming_always: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelChoice {
    Default,
    Model { provider: String, model_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentModelOverride {
    pub provider: Option<String>,
    pub model_id: String,
}

/// An async action triggered by a slash command that the TUI event loop
/// should process asynchronously.
#[derive(Debug, Clone)]
pub enum SlashAction {
    FetchModels,
    OpenSessions,
    LoadSession(String),
    SaveCredential(String, String, String), // provider, key, value
    RollbackLast,
    RunShell(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppState {
    Idle,
    WaitingResponse,
    AgentRunning,
    Quitting,
}

// ── Built-in commands ─────────────────────────────────────

pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show help"),
    ("/login", "Login to a provider (github)"),
    ("/agent", "List or switch agents"),
    ("/tokens", "Show token usage"),
    ("/new", "Start a new session"),
    ("/sessions", "Open session picker"),
    ("/model", "Show / switch model"),
    ("/subagents", "List active subagents"),
    ("/rollback", "Rollback last changes"),
];

impl App {
    #[allow(dead_code)]
    pub fn new(
        ui_config: &marshaling_protocol::UiConfig,
        model_info: String,
    ) -> Self {
        let workspace_root = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".into());
        let runtime_session_key = format!(
            "legacy:{}:{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        Self::new_with_workspace(
            ui_config,
            model_info,
            workspace_root,
            None,
            runtime_session_key,
        )
    }

    pub fn new_with_workspace(
        ui_config: &marshaling_protocol::UiConfig,
        model_info: String,
        workspace_root: String,
        repo_agents_md: Option<String>,
        runtime_session_key: String,
    ) -> Self {
        let mut agent_names: Vec<String> = ui_config.agent_names.clone();
        agent_names.sort();
        let mut subagent_names: Vec<String> = ui_config.subagent_names.clone();
        subagent_names.sort();
        let mut agent_model_info = ui_config.agent_model_info.clone();
        agent_model_info
            .entry("default".into())
            .or_insert_with(|| model_info.clone());
        Self {
            state: AppState::Idle,
            messages: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            scroll_offset: 0,
            auto_scroll: true,
            model_info: agent_model_info
                .get("default")
                .cloned()
                .unwrap_or_else(|| model_info.clone()),
            default_model_info: model_info,
            agent_model_info,
            tokens_input: 0,
            tokens_output: 0,
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            input_history: Vec::new(),
            input_history_idx: None,
            current_agent: "default".into(),
            agent_names,
            subagent_names,
            handled_slash_command: false,
            suggestions: Vec::new(),
            suggestion_index: 0,
            tool_calls: Vec::new(),
            agent_model_overrides: HashMap::new(),
            models_cache: Vec::new(),
            input_accent: agent_accent_color("default"),
            user_accent: parse_ui_color(&ui_config.user_accent),
            pending_slash: None,
            input_queue: VecDeque::new(),
            loading_progress: None,
            server_health: ServerHealth::Unknown,
            pending_permission: None,
            pending_permission_response: None,
            auto_allowed_tools: HashSet::new(),
            current_skill: None,
            subagent_views: Vec::new(),
            current_subagent_index: None,
            pending_cancel: false,
            esc_cancel_deadline: None,
            workspace_root,
            repo_agents_md,
            runtime_session_key,
            session_picker_open: false,
            session_picker_items: Vec::new(),
            session_picker_index: 0,
            model_picker_open: false,
            model_picker_items: Vec::new(),
            model_picker_index: 0,
            active_session_id: None,
        }
    }

    pub fn esc_cancel_step(&mut self) -> bool {
        let now = Instant::now();
        if let Some(deadline) = self.esc_cancel_deadline
            && now <= deadline
        {
            self.esc_cancel_deadline = None;
            return true;
        }
        self.esc_cancel_deadline = Some(now + Duration::from_secs(2));
        false
    }

    pub fn clear_esc_cancel_arm(&mut self) {
        self.esc_cancel_deadline = None;
    }

    pub fn current_model_override(&self) -> Option<&AgentModelOverride> {
        self.agent_model_overrides.get(&self.current_agent)
    }

    pub fn current_model_override_parts(&self) -> (Option<String>, Option<String>) {
        let Some(ov) = self.current_model_override() else {
            return (None, None);
        };
        (Some(ov.model_id.clone()), ov.provider.clone())
    }

    fn default_model_info_for_agent(&self, agent: &str) -> String {
        self.agent_model_info
            .get(agent)
            .cloned()
            .unwrap_or_else(|| self.default_model_info.clone())
    }

    fn default_provider_for_agent(&self, agent: &str) -> Option<String> {
        self.default_model_info_for_agent(agent)
            .split_once('/')
            .map(|(provider, _)| provider.to_string())
    }

    fn effective_model_info_for_agent(&self, agent: &str) -> String {
        if let Some(ov) = self.agent_model_overrides.get(agent) {
            let provider = ov
                .provider
                .clone()
                .or_else(|| self.default_provider_for_agent(agent))
                .unwrap_or_else(|| "?".into());
            format!("{provider}/{}", ov.model_id)
        } else {
            self.default_model_info_for_agent(agent)
        }
    }

    fn sync_current_agent_model_info(&mut self) {
        self.model_info = self.effective_model_info_for_agent(&self.current_agent);
    }

    // ── Input submission ──────────────────────────────────

    pub fn submit_input(&mut self) -> String {
        let text = std::mem::take(&mut self.input);
        self.input_cursor = 0;
        self.auto_scroll = true;

        // Save to history first (including slash commands), dedup against last entry
        if !text.is_empty()
            && self.input_history.last().map_or(true, |last| last != &text)
        {
            self.input_history.push(text.clone());
        }
        self.input_history_idx = None;

        if let Some(command) = shell_command_from_input(&text) {
            self.suggestions.clear();
            self.suggestion_index = 0;
            self.handled_slash_command = true;
            if command.is_empty() {
                self.messages.push(DisplayMessage::command(
                    Role::Assistant,
                    "Usage: ! <shell command>".into(),
                ));
            } else {
                self.messages.push(DisplayMessage::command(
                    Role::User,
                    format!("$ {command}"),
                ));
                self.pending_slash = Some(SlashAction::RunShell(command));
            }
            return String::new();
        }

        if text.starts_with('/') {
            self.suggestions.clear();
            self.suggestion_index = 0;
            self.handled_slash_command = true;
            self.handle_slash_command(&text);
            return String::new();
        }
        self.suggestions.clear();
        self.suggestion_index = 0;
        self.handled_slash_command = false;

        if !text.is_empty() {
            // Transform @mentions into explicit subagent call instructions
            let transformed = self.transform_mentions(&text);
            self.messages.push(DisplayMessage {
                role: Role::User,
                content: transformed.clone(),
                thinking: None,
                source: MessageSource::Conversation,
            });
            // Clear stream/resoning buffers for the new turn
            self.stream_buffer.clear();
            self.reasoning_buffer.clear();
            return transformed;
        }
        text
    }

    /// Transform `@name` mentions in the message into text the LLM will understand as subagent calls.
    /// E.g., `@review check this` → `[use subagent "review"] check this`
    fn transform_mentions(&self, text: &str) -> String {
        let mut result = text.to_string();
        for name in &self.subagent_names {
            let mention = format!("@{}", name);
            let replacement = format!("[use subagent \"{}\"]", name);
            let mut output = String::new();
            let mut remaining = result.as_str();
            while let Some(pos) = remaining.find(&mention) {
                let before = &remaining[..pos];
                let after = &remaining[pos + mention.len()..];
                // Only replace if preceded by space/start and followed by space/punctuation/end
                let start_ok = pos == 0 || before.ends_with(' ');
                let end_ok = after.is_empty()
                    || after.starts_with(' ')
                    || after.starts_with(',')
                    || after.starts_with('.');
                if start_ok && end_ok {
                    output.push_str(before);
                    output.push_str(&replacement);
                    remaining = after;
                } else {
                    output.push_str(&remaining[..=pos + mention.len() - 1]);
                    remaining = after;
                }
            }
            output.push_str(remaining);
            result = output;
        }
        result
    }

    fn handle_slash_command(&mut self, cmd: &str) {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        match parts[0] {
            "/agent" => {
                if parts.len() < 2 {
                    let all_agents = all_agent_names(&self.agent_names);
                    self.messages.push(DisplayMessage {
                        role: Role::Assistant,
                        content: format!("Available agents:\n  {}\nType /agent <name> to switch.", all_agents.join("\n  ")),
                        thinking: None,
                        source: MessageSource::Command,
                    });
                } else {
                    let name = parts[1];
                    if name == "default"
                        || self.agent_names.contains(&name.to_string())
                    {
                        self.current_agent = name.to_string();
                        self.input_accent =
                            agent_accent_color(&self.current_agent);
                        self.sync_current_agent_model_info();
                        self.messages.push(DisplayMessage::command(
                            Role::Assistant,
                            format!(
                                "Switched to agent: {} ({})",
                                name, self.model_info
                            ),
                        ));
                    } else {
                        self.messages.push(DisplayMessage::command(
                            Role::Assistant,
                            format!(
                                "Unknown agent: {}. Available: {}",
                                name,
                                all_agent_names(&self.agent_names).join(", ")
                            ),
                        ));
                    }
                }
            }
            "/help" => {
                self.messages.push(DisplayMessage {
                    role: Role::Assistant,
                    content: [
                        "Commands:",
                        "  /agent <name>     — Switch agent",
                        "  /help             — Show this help",
                        "  /tokens           — Show token usage",
                        "  /new              — Start a new session",
                        "  /sessions         — Open session picker",
                        "  /model            — Show / switch model",
                        "  /subagents        — List active subagents",
                        "  /rollback last    — Rollback latest file changes",
                        "  /login github <token>  — Save GitHub token",
                        "  /login deepseek <key>  — Save DeepSeek API key",
                        "  ! <command>       — Run local shell command",
                        "",
                        "Keybindings (configurable in keybindings.toml):",
                        "  Enter             — Send message",
                        "  Alt+Enter         — Newline",
                        "  Ctrl+A / Ctrl+E   — Line start / end",
                        "  Ctrl+D            — Delete current char",
                        "  Ctrl+K            — Clear current line",
                        "  Esc                — Press twice within 2s to stop running agent",
                        "  Ctrl+C             — Quit / cancel immediately",
                        "  Tab                — Cycle agent",
                        "  Up/Down           — Input history",
                        "  PgUp/PgDn, Ctrl+↑/↓ — Scroll",
                        "  Ctrl+P            — Agent command",
                        "  F5                — Cycle subagent views",
                    ]
                    .join("\n"),
                    thinking: None,
                    source: MessageSource::Command,
                });
            }
            "/tokens" => {
                self.messages.push(DisplayMessage::command(
                    Role::Assistant,
                    format!(
                        "Tokens: {} in / {} out",
                        self.tokens_input, self.tokens_output
                    ),
                ));
            }
            "/new" => {
                if self.state == AppState::Idle {
                    self.start_new_session();
                } else {
                    self.messages.push(DisplayMessage::command(
                        Role::Assistant,
                        "Cannot start a new session while agent is running."
                            .into(),
                    ));
                }
            }
            "/login" => {
                if parts.len() < 2 {
                    self.messages.push(DisplayMessage::command(Role::Assistant, "Usage: /login github <token>  or  /login deepseek <api_key>".into()));
                } else if parts[1] == "github" {
                    if parts.len() >= 3 {
                        let token = parts[2].to_string();
                        self.messages.push(DisplayMessage::command(
                            Role::Assistant,
                            "Saving GitHub token...".into(),
                        ));
                        self.pending_slash = Some(SlashAction::SaveCredential(
                            "github".into(),
                            "token".into(),
                            token,
                        ));
                    } else {
                        self.messages.push(DisplayMessage::command(Role::Assistant,
                            "Usage: /login github <token>\nCreate a PAT at: https://github.com/settings/tokens?type=beta\nScope: models:read".into()));
                    }
                } else if parts[1] == "deepseek" {
                    if parts.len() >= 3 {
                        let key = parts[2].to_string();
                        self.messages.push(DisplayMessage::command(
                            Role::Assistant,
                            "Saving DeepSeek API key...".into(),
                        ));
                        self.pending_slash = Some(SlashAction::SaveCredential(
                            "deepseek".into(),
                            "api_key".into(),
                            key,
                        ));
                    } else {
                        self.messages.push(DisplayMessage::command(Role::Assistant,
                            "Usage: /login deepseek <api_key>\nGet your key at: https://platform.deepseek.com/api_keys".into()));
                    }
                } else {
                    self.messages.push(DisplayMessage::command(
                        Role::Assistant,
                        format!(
                            "Unknown provider: {}. Supported: github, deepseek",
                            parts[1]
                        ),
                    ));
                }
            }
            "/sessions" => {
                if self.state == AppState::Idle {
                    self.pending_slash = Some(SlashAction::OpenSessions);
                } else {
                    self.messages.push(DisplayMessage::command(
                        Role::Assistant,
                        "Cannot open sessions while agent is running.".into(),
                    ));
                }
            }
            "/models" => {
                self.messages.push(DisplayMessage::command(
                    Role::Assistant,
                    format!("Messages: {}", self.messages.len()),
                ));
            }
            "/model" => {
                if parts.len() < 2 {
                    self.messages.push(DisplayMessage {
                        role: Role::Assistant,
                        content: "Fetching available models...".into(),
                        thinking: None,
                        source: MessageSource::Command,
                    });
                    self.pending_slash = Some(SlashAction::FetchModels);
                } else {
                    let name = parts[1];
                    if name == "default" {
                        self.agent_model_overrides.remove(&self.current_agent);
                        self.sync_current_agent_model_info();
                        self.messages.push(DisplayMessage::command(Role::Assistant, format!("Reset agent '{}' to default model: {}", self.current_agent, self.model_info)));
                    } else {
                        // Split provider/model format: "deepseek/deepseek-v4-pro" → provider "deepseek", model "deepseek-v4-pro"
                        let (model_name, provider) =
                            if let Some((p, m)) = name.split_once('/') {
                                (m.to_string(), Some(p.to_string()))
                            } else {
                                // Look up the provider from cache (model names are stored without provider prefix)
                                let prov = self
                                    .models_cache
                                    .iter()
                                    .find(|(_, m)| m == name)
                                    .map(|(p, _)| p.clone());
                                (name.to_string(), prov)
                            };
                        self.agent_model_overrides.insert(
                            self.current_agent.clone(),
                            AgentModelOverride {
                                provider: provider.clone(),
                                model_id: model_name.clone(),
                            },
                        );
                        self.sync_current_agent_model_info();
                        let extra = provider
                            .map(|p| format!(" (provider: {})", p))
                            .unwrap_or_default();
                        self.messages.push(DisplayMessage::command(
                            Role::Assistant,
                            format!(
                                "Switched agent '{}' to model: {}{}",
                                self.current_agent, model_name, extra
                            ),
                        ));
                    }
                }
            }
            "/subagents" => {
                if self.subagent_views.is_empty() {
                    self.messages.push(DisplayMessage::command(
                        Role::Assistant,
                        "No subagents running.".into(),
                    ));
                } else {
                    let mut lines = vec!["Active subagents:".to_string()];
                    for (i, sv) in self.subagent_views.iter().enumerate() {
                        let status = if sv.done { "done" } else { "running" };
                        let viewing = self.current_subagent_index == Some(i);
                        let marker = if viewing { " ←" } else { "" };
                        lines.push(format!(
                            "  {}. {} ({}){}",
                            i + 1,
                            sv.name,
                            status,
                            marker
                        ));
                    }
                    lines.push(String::new());
                    lines.push("Press F5 to cycle between views.".into());
                    self.messages.push(DisplayMessage::command(
                        Role::Assistant,
                        lines.join("\n"),
                    ));
                }
            }
            "/rollback" => {
                let sub = parts.get(1).copied().unwrap_or("last");
                if sub != "last" {
                    self.messages.push(DisplayMessage::command(
                        Role::Assistant,
                        "Usage: /rollback last".into(),
                    ));
                } else {
                    self.messages.push(DisplayMessage::command(
                        Role::Assistant,
                        "Requesting rollback of latest tracked changes..."
                            .into(),
                    ));
                    self.pending_slash = Some(SlashAction::RollbackLast);
                }
            }
            _ => {
                self.messages.push(DisplayMessage::command(
                    Role::Assistant,
                    format!("Unknown command: {}. Type /help.", cmd),
                ));
            }
        }
    }

    // ── Agent loop integration ────────────────────────────

    pub fn start_agent(&mut self) {
        self.state = AppState::AgentRunning;
        self.stream_buffer.clear();
        self.reasoning_buffer.clear();
        self.tool_calls.clear();
        self.loading_progress = Some(0.0);
        self.current_skill = None;
    }

    pub fn agent_text_delta(&mut self, chunk: &str) {
        self.stream_buffer.push_str(chunk);
    }

    pub fn agent_reasoning_delta(&mut self, chunk: &str) {
        self.reasoning_buffer.push_str(chunk);
    }

    pub fn agent_tool_started(&mut self, id: &str, name: &str) {
        self.tool_calls.push(ToolCallDisplay {
            id: id.to_string(),
            name: name.to_string(),
            status: ToolStatus::Running,
            changes: Vec::new(),
        });
    }

    pub fn agent_tool_completed(
        &mut self,
        id: &str,
        _result: &str,
        changes: &[marshaling_protocol::FileChange],
    ) {
        if let Some(tc) = self.tool_calls.iter_mut().find(|t| t.id == id) {
            tc.status = ToolStatus::Success;
            tc.changes = changes.to_vec();
        }
    }

    pub fn agent_tool_failed(&mut self, id: &str, error: &str) {
        if let Some(tc) = self.tool_calls.iter_mut().find(|t| t.id == id) {
            tc.status = ToolStatus::Failed(error.to_string());
        }
    }

    /// Called at the end of each agent turn. Saves the intermediate text
    /// and tool calls to the conversation history.
    pub fn agent_turn_done(
        &mut self,
        text: &str,
        tool_calls: &[ToolCallDisplay],
    ) {
        let change_summary = render_change_summary(tool_calls);
        let final_text = if change_summary.is_empty() {
            text.to_string()
        } else if text.is_empty() {
            change_summary
        } else {
            format!("{}\n\n{}", text, change_summary)
        };
        let thinking = if self.reasoning_buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.reasoning_buffer))
        };
        if !final_text.is_empty() {
            self.messages.push(DisplayMessage {
                role: Role::Assistant,
                content: final_text,
                thinking,
                source: MessageSource::Conversation,
            });
        } else if thinking.is_some() {
            // Even if the text is empty, save thinking (e.g., tool-only turns)
            self.messages.push(DisplayMessage {
                role: Role::Assistant,
                content: String::new(),
                thinking,
                source: MessageSource::Conversation,
            });
        }
        self.stream_buffer.clear();
        self.tool_calls.clear();
    }

    /// Called when the agent loop finishes entirely.
    pub fn agent_done(&mut self, content: &str) {
        let thinking = if self.reasoning_buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.reasoning_buffer))
        };
        // Sentinel values from the agent loop — don't display as assistant messages
        if matches!(
            content,
            "(cancelled)" | "(max steps reached)" | "(interrupted)"
        ) {
            if thinking.is_some() {
                self.messages.push(DisplayMessage {
                    role: Role::Assistant,
                    content: String::new(),
                    thinking,
                    source: MessageSource::Conversation,
                });
            }
            self.stream_buffer.clear();
            self.tool_calls.clear();
            self.state = AppState::Idle;
            return;
        }
        if !content.is_empty() {
            self.messages.push(DisplayMessage {
                role: Role::Assistant,
                content: content.to_string(),
                thinking,
                source: MessageSource::Conversation,
            });
        } else if thinking.is_some() {
            self.messages.push(DisplayMessage {
                role: Role::Assistant,
                content: String::new(),
                thinking,
                source: MessageSource::Conversation,
            });
        }
        self.stream_buffer.clear();
        self.tool_calls.clear();
        self.state = AppState::Idle;
    }

    /// Show an error message (used by start_agent on setup failure).
    pub fn set_error(&mut self, error: &str) {
        if !self.stream_buffer.is_empty() {
            self.messages.push(DisplayMessage {
                role: Role::Assistant,
                content: self.stream_buffer.clone(),
                thinking: None,
                source: MessageSource::Error,
            });
        }
        self.messages.push(DisplayMessage {
            role: Role::Assistant,
            content: format!("Error: {}", error),
            thinking: None,
            source: MessageSource::Error,
        });
        self.stream_buffer.clear();
        self.reasoning_buffer.clear();
        self.tool_calls.clear();
        self.state = AppState::Idle;
    }

    /// Queue a message for later processing (when agent is running).
    pub fn queue_input(&mut self, text: &str) {
        self.input_queue.push_back(text.to_string());
        self.input.clear();
        self.input_cursor = 0;
    }

    // ── Text editing helpers ─────────────────────────────

    pub fn insert_newline(&mut self) {
        self.input.insert(self.input_cursor, '\n');
        self.input_cursor += 1;
    }

    pub fn insert_char(&mut self, c: char) {
        if self.input.len() == 1 && self.input.starts_with('/') && c == '/' {
            return;
        }
        self.input.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
        self.update_suggestions();
    }

    pub fn delete_before(&mut self) {
        if self.input_cursor > 0 {
            let prev = self.input[..self.input_cursor]
                .chars()
                .next_back()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.input_cursor -= prev;
            self.input.remove(self.input_cursor);
            self.update_suggestions();
        }
    }

    pub fn delete_after(&mut self) {
        if self.input_cursor < self.input.len() {
            self.input.remove(self.input_cursor);
            self.update_suggestions();
        }
    }

    pub fn kill_line(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.update_suggestions();
    }

    pub fn cursor_left(&mut self) {
        if self.input_cursor > 0 {
            let prev = self.input[..self.input_cursor]
                .chars()
                .next_back()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.input_cursor -= prev;
        }
    }

    pub fn cursor_right(&mut self) {
        if self.input_cursor < self.input.len() {
            let next = self.input[self.input_cursor..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.input_cursor += next;
        }
    }

    pub fn cursor_home(&mut self) {
        self.input_cursor = 0;
    }
    pub fn cursor_end(&mut self) {
        self.input_cursor = self.input.len();
    }

    pub fn history_up(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let idx = match self.input_history_idx {
            None => self.input_history.len() - 1,
            Some(i) if i > 0 => i - 1,
            Some(_) => return,
        };
        self.input_history_idx = Some(idx);
        self.input = self.input_history[idx].clone();
        self.input_cursor = self.input.len();
    }

    pub fn history_down(&mut self) {
        match self.input_history_idx {
            None => {}
            Some(i) if i + 1 < self.input_history.len() => {
                self.input_history_idx = Some(i + 1);
                self.input = self.input_history[i + 1].clone();
                self.input_cursor = self.input.len();
            }
            Some(_) => {
                self.input_history_idx = None;
                self.input.clear();
                self.input_cursor = 0;
            }
        }
    }

    pub fn cycle_agent(&mut self) {
        let names = all_agent_names(&self.agent_names);
        if names.is_empty() {
            return;
        }
        let idx = names
            .iter()
            .position(|n| n == &self.current_agent)
            .unwrap_or(0);
        let next = names[(idx + 1) % names.len()].clone();
        self.current_agent = next.clone();
        self.input_accent = agent_accent_color(&self.current_agent);
        self.sync_current_agent_model_info();
    }

    pub fn open_session_picker(
        &mut self,
        mut items: Vec<marshaling_protocol::SessionInfo>,
    ) {
        // Newest sessions first in the popup (top = latest).
        items.sort_by(|a, b| {
            b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id))
        });
        self.session_picker_open = true;
        self.session_picker_items = items;
        self.session_picker_index = 0;
    }

    pub fn close_session_picker(&mut self) {
        self.session_picker_open = false;
    }

    pub fn open_model_picker(&mut self, mut models: Vec<ModelInfo>) {
        models.sort_by(|a, b| {
            a.provider
                .cmp(&b.provider)
                .then_with(|| a.model_id.cmp(&b.model_id))
        });
        self.models_cache = models
            .iter()
            .map(|m| (m.provider.clone(), m.model_id.clone()))
            .collect();
        let mut items = Vec::with_capacity(models.len() + 1);
        items.push(ModelChoice::Default);
        items.extend(models.into_iter().map(|m| ModelChoice::Model {
            provider: m.provider,
            model_id: m.model_id,
        }));
        self.model_picker_open = true;
        self.model_picker_items = items;
        self.model_picker_index = 0;
    }

    pub fn close_model_picker(&mut self) {
        self.model_picker_open = false;
    }

    pub fn model_picker_up(&mut self) {
        if self.model_picker_items.is_empty() {
            return;
        }
        if self.model_picker_index == 0 {
            self.model_picker_index = self.model_picker_items.len() - 1;
        } else {
            self.model_picker_index -= 1;
        }
    }

    pub fn model_picker_down(&mut self) {
        if self.model_picker_items.is_empty() {
            return;
        }
        self.model_picker_index =
            (self.model_picker_index + 1) % self.model_picker_items.len();
    }

    pub fn selected_model_choice(&self) -> Option<ModelChoice> {
        self.model_picker_items
            .get(self.model_picker_index)
            .cloned()
    }

    pub fn apply_model_choice(&mut self, choice: ModelChoice) {
        match choice {
            ModelChoice::Default => {
                self.agent_model_overrides.remove(&self.current_agent);
                self.sync_current_agent_model_info();
                self.messages.push(DisplayMessage::command(
                    Role::Assistant,
                    format!(
                        "Reset agent '{}' to default model: {}",
                        self.current_agent, self.model_info
                    ),
                ));
            }
            ModelChoice::Model { provider, model_id } => {
                self.agent_model_overrides.insert(
                    self.current_agent.clone(),
                    AgentModelOverride {
                        provider: Some(provider.clone()),
                        model_id: model_id.clone(),
                    },
                );
                self.sync_current_agent_model_info();
                self.messages.push(DisplayMessage::command(
                    Role::Assistant,
                    format!(
                        "Switched agent '{}' to model: {provider}/{model_id}",
                        self.current_agent
                    ),
                ));
            }
        }
    }

    pub fn session_picker_up(&mut self) {
        if self.session_picker_items.is_empty() {
            return;
        }
        if self.session_picker_index == 0 {
            self.session_picker_index = self.session_picker_items.len() - 1;
        } else {
            self.session_picker_index -= 1;
        }
    }

    pub fn session_picker_down(&mut self) {
        if self.session_picker_items.is_empty() {
            return;
        }
        self.session_picker_index =
            (self.session_picker_index + 1) % self.session_picker_items.len();
    }

    pub fn reset_for_loaded_session(&mut self) {
        self.state = AppState::Idle;
        self.messages.clear();
        self.stream_buffer.clear();
        self.reasoning_buffer.clear();
        self.tool_calls.clear();
        self.loading_progress = None;
        self.pending_permission = None;
        self.pending_permission_response = None;
        self.pending_cancel = false;
        self.clear_esc_cancel_arm();
        self.current_skill = None;
        self.subagent_views.clear();
        self.current_subagent_index = None;
        self.input_queue.clear();
        self.input.clear();
        self.input_cursor = 0;
        self.suggestions.clear();
        self.suggestion_index = 0;
        self.handled_slash_command = false;
        self.close_session_picker();
        self.close_model_picker();
        self.scroll_to_bottom();
        self.tokens_input = 0;
        self.tokens_output = 0;
    }

    pub fn start_new_session(&mut self) {
        self.state = AppState::Idle;
        self.messages.clear();
        self.stream_buffer.clear();
        self.reasoning_buffer.clear();
        self.tool_calls.clear();
        self.loading_progress = None;
        self.pending_permission = None;
        self.pending_permission_response = None;
        self.pending_cancel = false;
        self.clear_esc_cancel_arm();
        self.current_skill = None;
        self.subagent_views.clear();
        self.current_subagent_index = None;
        self.input_queue.clear();
        self.input.clear();
        self.input_cursor = 0;
        self.suggestions.clear();
        self.suggestion_index = 0;
        self.handled_slash_command = false;
        self.close_session_picker();
        self.close_model_picker();
        self.active_session_id = None;
        self.tokens_input = 0;
        self.tokens_output = 0;
        self.scroll_to_bottom();
        self.messages.push(DisplayMessage::command(
            Role::Assistant,
            "Started a new session.".into(),
        ));
    }

    /// Scroll up: increase offset from bottom to see OLDER content above.
    pub fn scroll_up(&mut self, amount: usize) {
        if self.auto_scroll {
            self.auto_scroll = false;
        }
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
    }

    /// Scroll down: decrease offset from bottom to see NEWER content below.
    pub fn scroll_down(&mut self, amount: usize) {
        if self.auto_scroll {
            return;
        }
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
        self.scroll_offset = 0;
    }

    // ── Suggestions ───────────────────────────────────────

    pub fn update_suggestions(&mut self) {
        self.suggestions.clear();
        self.suggestion_index = 0;
        let input = self.input.trim_start();
        if input.starts_with('@') {
            let after = input[1..].trim_start();
            for name in &self.subagent_names {
                if name.starts_with(after) {
                    self.suggestions.push(format!("@{}", name));
                }
            }
            return;
        }
        if !input.starts_with('/') {
            return;
        }

        if let Some(cmd) = input.split_whitespace().next() {
            let after = input[cmd.len()..].trim_start();
            if !after.is_empty() && (cmd == "/agent" || cmd == "/a") {
                for name in all_agent_names(&self.agent_names) {
                    if name.starts_with(after) {
                        self.suggestions.push(format!("/agent {}", name));
                    }
                }
                return;
            }
            if !after.is_empty() {
                return;
            }
        }

        let lower = input.to_lowercase();
        for &(cmd, desc) in SLASH_COMMANDS {
            if cmd.starts_with(&lower) {
                self.suggestions.push(format!("{}  — {}", cmd, desc));
            }
        }
    }

    pub fn suggestion_next(&mut self) {
        if !self.suggestions.is_empty() {
            self.suggestion_index =
                (self.suggestion_index + 1).min(self.suggestions.len());
        }
    }

    pub fn suggestion_prev(&mut self) {
        if self.suggestion_index > 0 {
            self.suggestion_index -= 1;
        }
    }

    pub fn selected_suggestion(&self) -> Option<&str> {
        if self.suggestion_index > 0
            && self.suggestion_index <= self.suggestions.len()
        {
            let s = &self.suggestions[self.suggestion_index - 1];
            Some(s.split("  —").next().unwrap_or(s))
        } else {
            None
        }
    }

    pub fn accept_suggestion(&mut self) {
        if let Some(cmd) = self.selected_suggestion() {
            self.input = cmd.to_string();
            self.input_cursor = self.input.len();
            self.suggestions.clear();
            self.suggestion_index = 0;
        }
    }
}

fn shell_command_from_input(text: &str) -> Option<String> {
    text.strip_prefix('!')
        .map(|cmd| cmd.trim_start().to_string())
}

/// Returns all agent names including the built-in "default".
fn all_agent_names(configured: &[String]) -> Vec<String> {
    let mut names = vec!["default".to_string()];
    names.extend(configured.iter().cloned());
    names
}

fn agent_accent_color(name: &str) -> Color {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    let hue = (hasher.finish() % 360) as f32;
    let (r, g, b) = hsl_to_rgb(hue, 0.65, 0.55);
    Color::Rgb(r, g, b)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as u8 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to_u8 = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    (to_u8(r1), to_u8(g1), to_u8(b1))
}

/// Parse a color name string into a ratatui Color.
fn parse_ui_color(name: &str) -> Color {
    let s = name.trim();
    // Support hex colors: #RRGGBB
    if s.starts_with('#') && s.len() == 7 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&s[1..3], 16),
            u8::from_str_radix(&s[3..5], 16),
            u8::from_str_radix(&s[5..7], 16),
        ) {
            return Color::Rgb(r, g, b);
        }
    }
    match s.to_lowercase().as_str() {
        "cyan" => Color::Cyan,
        "green" => Color::Green,
        "blue" => Color::Blue,
        "yellow" => Color::Yellow,
        "red" => Color::Red,
        "magenta" => Color::Magenta,
        "white" => Color::White,
        _ => Color::Cyan,
    }
}

fn render_change_summary(tool_calls: &[ToolCallDisplay]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for tc in tool_calls {
        if tc.changes.is_empty() {
            continue;
        }
        lines.push(format!("Changes from tool `{}`:", tc.name));
        for ch in &tc.changes {
            match ch.kind {
                marshaling_protocol::FileChangeKind::Modified => {
                    lines.push("```diff".into());
                    lines.push(format!("diff -- {}", ch.path));
                    for dl in &ch.diff_lines {
                        let prefix = match dl.kind {
                            marshaling_protocol::DiffLineKind::Added => "+",
                            marshaling_protocol::DiffLineKind::Removed => "-",
                            marshaling_protocol::DiffLineKind::Context => " ",
                        };
                        lines.push(format!("{}{}", prefix, dl.content));
                    }
                    if ch.truncated {
                        lines.push("[diff truncated]".into());
                    }
                    lines.push("```".into());
                }
                marshaling_protocol::FileChangeKind::Added => {
                    lines.push(format!("! new file added: {}", ch.path));
                }
                marshaling_protocol::FileChangeKind::Removed => {
                    lines.push(format!("! file removed: {}", ch.path));
                }
            }
        }
        lines.push(String::new());
    }
    while lines.last().is_some_and(|s| s.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ui_config() -> marshaling_protocol::UiConfig {
        let mut agent_model_info = HashMap::new();
        agent_model_info.insert("default".into(), "test/test-model".into());
        marshaling_protocol::UiConfig {
            input_accent: "cyan".into(),
            user_accent: "cyan".into(),
            agent_names: vec!["default".into()],
            subagent_names: vec!["review".into()],
            model_info: "test/test-model".into(),
            agent_model_info,
        }
    }

    #[test]
    fn test_new_app() {
        let cfg = test_ui_config();
        let app = App::new(&cfg, cfg.model_info.clone());
        assert_eq!(app.state, AppState::Idle);
        assert!(app.messages.is_empty());
        assert_eq!(app.current_agent, "default");
        assert!(app.current_model_override().is_none());
    }

    #[test]
    fn test_submit_normal_message() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "hello".into();
        app.input_cursor = 5;

        let text = app.submit_input();
        assert_eq!(text, "hello");
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, Role::User);
        assert_eq!(app.messages[0].content, "hello");
        assert!(app.input.is_empty());
    }

    #[test]
    fn test_submit_shell_command_creates_pending_action() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "! ls -la".into();
        app.input_cursor = app.input.len();

        let text = app.submit_input();

        assert!(text.is_empty());
        assert!(app.handled_slash_command);
        assert!(matches!(
            app.pending_slash,
            Some(SlashAction::RunShell(ref cmd)) if cmd == "ls -la"
        ));
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].source, MessageSource::Command);
        assert_eq!(app.messages[0].content, "$ ls -la");
    }

    #[test]
    fn test_submit_bare_shell_prefix_shows_usage() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "!".into();

        app.submit_input();

        assert!(app.pending_slash.is_none());
        assert!(app.messages[0].content.contains("Usage: !"));
    }

    #[test]
    fn test_submit_empty_quits() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        let text = app.submit_input();
        assert!(text.is_empty());
        assert!(!app.handled_slash_command);
    }

    #[test]
    fn test_slash_help() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "/help".into();
        app.input_cursor = 5;

        let text = app.submit_input();
        assert!(text.is_empty());
        assert!(app.handled_slash_command);
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].content.contains("Commands"));
    }

    #[test]
    fn test_slash_tokens() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.tokens_input = 100;
        app.tokens_output = 50;
        app.input = "/tokens".into();

        app.submit_input();
        assert!(app.messages[0].content.contains("100"));
        assert!(app.messages[0].content.contains("50"));
    }

    #[test]
    fn test_slash_sessions_opens_action() {
        let cfg = test_ui_config();
        let mut app = App::new_with_workspace(
            &cfg,
            cfg.model_info.clone(),
            "/tmp/ws".into(),
            None,
            "sess-123".into(),
        );
        app.input = "/sessions".into();
        app.submit_input();
        assert!(matches!(app.pending_slash, Some(SlashAction::OpenSessions)));
    }

    #[test]
    fn test_slash_model_fetches_picker_action() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "/model".into();

        app.submit_input();

        assert!(matches!(app.pending_slash, Some(SlashAction::FetchModels)));
    }

    #[test]
    fn test_model_picker_apply_model_and_default() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.open_model_picker(vec![
            marshaling_protocol::ModelInfo {
                provider: "github".into(),
                model_id: "gpt-4o".into(),
            },
            marshaling_protocol::ModelInfo {
                provider: "deepseek".into(),
                model_id: "deepseek-chat".into(),
            },
        ]);
        assert!(app.model_picker_open);
        assert_eq!(app.model_picker_items.len(), 3);

        app.model_picker_down();
        let choice = app.selected_model_choice().unwrap();
        app.apply_model_choice(choice);
        let ov = app.current_model_override().unwrap();
        assert_eq!(ov.provider.as_deref(), Some("deepseek"));
        assert_eq!(ov.model_id, "deepseek-chat");
        assert_eq!(app.model_info, "deepseek/deepseek-chat");

        app.apply_model_choice(ModelChoice::Default);
        assert!(app.current_model_override().is_none());
        assert_eq!(app.model_info, "test/test-model");
    }

    #[test]
    fn test_start_new_session_clears_active_session_and_runtime_state() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.messages.push(DisplayMessage {
            role: Role::User,
            content: "old".into(),
            thinking: None,
            source: MessageSource::Conversation,
        });
        app.active_session_id = Some("chat-old".into());
        app.tokens_input = 10;
        app.tokens_output = 20;
        app.stream_buffer = "stream".into();
        app.reasoning_buffer = "thinking".into();
        app.input = "draft".into();

        app.start_new_session();

        assert!(app.active_session_id.is_none());
        assert_eq!(app.tokens_input, 0);
        assert_eq!(app.tokens_output, 0);
        assert!(app.stream_buffer.is_empty());
        assert!(app.reasoning_buffer.is_empty());
        assert!(app.input.is_empty());
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].content.contains("new session"));
    }

    #[test]
    fn test_slash_sessions_blocked_while_running() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.state = AppState::AgentRunning;
        app.input = "/sessions".into();
        app.submit_input();
        assert!(app.pending_slash.is_none());
        assert!(
            app.messages
                .last()
                .is_some_and(|m| m.content.contains("Cannot open sessions"))
        );
    }

    #[test]
    fn test_reset_for_loaded_session_clears_transient_state() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.state = AppState::AgentRunning;
        app.messages
            .push(DisplayMessage::command(Role::Assistant, "temp".into()));
        app.stream_buffer = "stream".into();
        app.reasoning_buffer = "thinking".into();
        app.tool_calls.push(ToolCallDisplay {
            id: "1".into(),
            name: "read".into(),
            status: ToolStatus::Running,
            changes: Vec::new(),
        });
        app.loading_progress = Some(0.5);
        app.pending_permission = Some(PendingPermission {
            id: "p1".into(),
            tool_name: "read".into(),
            args: "{}".into(),
            confirming_always: false,
        });
        app.pending_permission_response = Some(("p1".into(), true, false));
        app.pending_cancel = true;
        app.subagent_views.push(SubagentView {
            id: "sub1".into(),
            name: "review".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });
        app.current_subagent_index = Some(0);
        app.input_queue.push_back("queued".into());
        app.input = "abc".into();
        app.input_cursor = 3;
        app.suggestions = vec!["/sessions".into()];
        app.suggestion_index = 1;
        app.session_picker_open = true;
        app.tokens_input = 12;
        app.tokens_output = 34;

        app.reset_for_loaded_session();

        assert_eq!(app.state, AppState::Idle);
        assert!(app.messages.is_empty());
        assert!(app.stream_buffer.is_empty());
        assert!(app.reasoning_buffer.is_empty());
        assert!(app.tool_calls.is_empty());
        assert!(app.loading_progress.is_none());
        assert!(app.pending_permission.is_none());
        assert!(app.pending_permission_response.is_none());
        assert!(!app.pending_cancel);
        assert!(app.subagent_views.is_empty());
        assert!(app.current_subagent_index.is_none());
        assert!(app.input_queue.is_empty());
        assert!(app.input.is_empty());
        assert_eq!(app.input_cursor, 0);
        assert!(app.suggestions.is_empty());
        assert_eq!(app.suggestion_index, 0);
        assert!(!app.session_picker_open);
        assert_eq!(app.tokens_input, 0);
        assert_eq!(app.tokens_output, 0);
    }

    #[test]
    fn test_open_session_picker_sorts_newest_first() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.open_session_picker(vec![
            marshaling_protocol::SessionInfo {
                id: "old".into(),
                created: "2026-06-10T10:00:00Z".into(),
                model: "p/m".into(),
                message_count: 1,
                summary: None,
            },
            marshaling_protocol::SessionInfo {
                id: "new".into(),
                created: "2026-06-11T10:00:00Z".into(),
                model: "p/m".into(),
                message_count: 2,
                summary: None,
            },
        ]);

        assert_eq!(app.session_picker_items[0].id, "new");
        assert_eq!(app.session_picker_items[1].id, "old");
    }

    #[test]
    fn test_slash_agent_list() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "/agent".into();
        app.submit_input();
        assert!(app.messages[0].content.contains("default"));
    }

    #[test]
    fn test_slash_agent_switch() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "/agent default".into();
        app.submit_input();
        assert_eq!(app.current_agent, "default");
        assert_eq!(app.input_accent, agent_accent_color("default"));
    }

    #[test]
    fn test_cycle_agent_keeps_stable_colors() {
        let cfg = marshaling_protocol::UiConfig {
            input_accent: "cyan".into(),
            user_accent: "cyan".into(),
            agent_names: vec!["plan".into(), "review".into()],
            subagent_names: vec![],
            model_info: "test/test-model".into(),
            agent_model_info: HashMap::from([
                ("default".into(), "test/test-model".into()),
                ("plan".into(), "deepseek/plan-model".into()),
                ("review".into(), "github/review-model".into()),
            ]),
        };
        let mut app = App::new(&cfg, cfg.model_info.clone());

        assert_eq!(app.input_accent, agent_accent_color("default"));

        app.cycle_agent();
        assert_eq!(app.current_agent, "plan");
        assert_eq!(app.input_accent, agent_accent_color("plan"));

        app.cycle_agent();
        assert_eq!(app.current_agent, "review");
        assert_eq!(app.input_accent, agent_accent_color("review"));

        app.input = "/agent plan".into();
        app.submit_input();
        assert_eq!(app.current_agent, "plan");
        assert_eq!(app.input_accent, agent_accent_color("plan"));
    }

    #[test]
    fn test_slash_unknown() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "/xyz".into();
        app.submit_input();
        assert!(app.messages[0].content.contains("Unknown"));
    }

    #[test]
    fn test_update_suggestions_starts_with_slash() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "/".into();
        app.update_suggestions();
        assert!(!app.suggestions.is_empty());
        assert!(app.suggestions[0].contains("/help"));
    }

    #[test]
    fn test_update_suggestions_filtering() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "/he".into();
        app.update_suggestions();
        assert!(app.suggestions.iter().any(|s| s.contains("/help")));
        assert!(!app.suggestions.iter().any(|s| s.contains("/agent")));
    }

    #[test]
    fn test_update_suggestions_no_slash() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "hello".into();
        app.update_suggestions();
        assert!(app.suggestions.is_empty());
    }

    #[test]
    fn test_accept_suggestion() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "/".into();
        app.update_suggestions();
        assert!(!app.suggestions.is_empty());
        app.suggestion_index = 1;
        let first_cmd = app.selected_suggestion().unwrap().to_string();
        app.accept_suggestion();
        assert_eq!(app.input, first_cmd);
        assert!(app.suggestions.is_empty());
    }

    #[test]
    fn test_input_history() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "first".into();
        app.submit_input();
        app.input = "second".into();
        app.submit_input();
        assert_eq!(app.input_history.len(), 2);

        app.history_up();
        assert_eq!(app.input, "second");
        app.history_up();
        assert_eq!(app.input, "first");
        app.history_down();
        assert_eq!(app.input, "second");
    }

    #[test]
    fn test_scroll_helpers() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        assert!(app.auto_scroll);
        app.scroll_up(5);
        assert!(!app.auto_scroll);
        app.scroll_to_bottom();
        assert!(app.auto_scroll);
    }

    #[test]
    fn test_insert_newline() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "hello".into();
        app.input_cursor = 5;
        app.insert_newline();
        assert_eq!(app.input, "hello\n");
        assert_eq!(app.input_cursor, 6);
    }

    #[test]
    fn test_insert_char_and_delete() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.insert_char('a');
        app.insert_char('b');
        app.insert_char('c');
        assert_eq!(app.input, "abc");
        app.cursor_left();
        app.delete_before();
        assert_eq!(app.input, "ac");
    }

    #[test]
    fn test_kill_line() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.input = "hello world".into();
        app.input_cursor = 5;
        app.kill_line();
        assert!(app.input.is_empty());
        assert_eq!(app.input_cursor, 0);
    }

    #[test]
    fn test_cycle_agent_updates_agent_without_echo_message() {
        let cfg = marshaling_protocol::UiConfig {
            input_accent: "cyan".into(),
            user_accent: "cyan".into(),
            agent_names: vec!["plan".into(), "review".into()],
            subagent_names: vec![],
            model_info: "test/test-model".into(),
            agent_model_info: HashMap::from([
                ("default".into(), "test/test-model".into()),
                ("plan".into(), "deepseek/plan-model".into()),
                ("review".into(), "github/review-model".into()),
            ]),
        };
        let mut app = App::new(&cfg, cfg.model_info.clone());
        assert_eq!(app.current_agent, "default");
        app.cycle_agent();
        assert_eq!(app.current_agent, "plan");
        assert!(app.messages.is_empty());
    }

    #[test]
    fn test_esc_double_press_cancel_arm() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        assert!(!app.esc_cancel_step());
        assert!(app.esc_cancel_step());
    }

    #[test]
    fn test_model_override() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        assert!(app.current_model_override().is_none());
        app.agent_model_overrides.insert(
            app.current_agent.clone(),
            AgentModelOverride {
                provider: Some("deepseek".into()),
                model_id: "deepseek-chat".into(),
            },
        );
        let ov = app.current_model_override().unwrap();
        assert_eq!(ov.provider.as_deref(), Some("deepseek"));
        assert_eq!(ov.model_id, "deepseek-chat");
    }

    #[test]
    fn test_switching_agent_updates_model_info() {
        let cfg = marshaling_protocol::UiConfig {
            input_accent: "cyan".into(),
            user_accent: "cyan".into(),
            agent_names: vec!["plan".into(), "review".into()],
            subagent_names: vec![],
            model_info: "test/test-model".into(),
            agent_model_info: HashMap::from([
                ("default".into(), "test/test-model".into()),
                ("plan".into(), "deepseek/plan-model".into()),
                ("review".into(), "github/review-model".into()),
            ]),
        };
        let mut app = App::new(&cfg, cfg.model_info.clone());
        assert_eq!(app.model_info, "test/test-model");
        app.cycle_agent();
        assert_eq!(app.current_agent, "plan");
        assert_eq!(app.model_info, "deepseek/plan-model");
        app.cycle_agent();
        assert_eq!(app.current_agent, "review");
        assert_eq!(app.model_info, "github/review-model");
    }

    #[test]
    fn test_model_override_is_scoped_per_agent() {
        let cfg = marshaling_protocol::UiConfig {
            input_accent: "cyan".into(),
            user_accent: "cyan".into(),
            agent_names: vec!["plan".into(), "review".into()],
            subagent_names: vec![],
            model_info: "test/test-model".into(),
            agent_model_info: HashMap::from([
                ("default".into(), "test/test-model".into()),
                ("plan".into(), "deepseek/plan-model".into()),
                ("review".into(), "github/review-model".into()),
            ]),
        };
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.cycle_agent();
        app.apply_model_choice(ModelChoice::Model {
            provider: "deepseek".into(),
            model_id: "plan-override".into(),
        });
        assert_eq!(app.model_info, "deepseek/plan-override");

        app.cycle_agent();
        assert_eq!(app.current_agent, "review");
        assert_eq!(app.model_info, "github/review-model");

        app.cycle_agent();
        assert_eq!(app.current_agent, "default");
        assert_eq!(app.model_info, "test/test-model");

        app.cycle_agent();
        assert_eq!(app.current_agent, "plan");
        assert_eq!(app.model_info, "deepseek/plan-override");
    }

    #[test]
    fn test_agent_done_with_content() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.agent_done("Task complete");
        assert_eq!(app.state, AppState::Idle);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "Task complete");
    }

    #[test]
    fn test_agent_done_cancelled() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.agent_done("(cancelled)");
        assert_eq!(app.messages.len(), 0);
    }

    #[test]
    fn test_set_error() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.set_error("Something went wrong");
        assert_eq!(app.state, AppState::Idle);
        assert!(app.messages[0].content.contains("Error"));
    }

    #[test]
    fn test_all_agent_names() {
        let configured = vec!["plan".into(), "review".into()];
        let names = all_agent_names(&configured);
        assert_eq!(names, vec!["default", "plan", "review"]);
    }

    #[test]
    fn test_tool_call_tracking() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.agent_tool_started("c1", "read");
        assert_eq!(app.tool_calls.len(), 1);
        assert!(matches!(app.tool_calls[0].status, ToolStatus::Running));

        app.agent_tool_completed("c1", "result", &[]);
        assert!(matches!(app.tool_calls[0].status, ToolStatus::Success));

        app.agent_tool_started("c2", "bash");
        app.agent_tool_failed("c2", "error msg");
        assert_eq!(app.tool_calls.len(), 2);
        assert!(
            matches!(&app.tool_calls[1].status, ToolStatus::Failed(e) if e == "error msg")
        );
    }

    #[test]
    fn test_subagent_views_created_on_started() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        assert!(app.subagent_views.is_empty());
        assert!(app.current_subagent_index.is_none());

        // Simulate SubagentStarted
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        assert_eq!(app.subagent_views.len(), 1);
        assert_eq!(app.subagent_views[0].name, "review");
    }

    #[test]
    fn test_subagent_text_delta_appends_to_buffer() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        // Append text delta
        if let Some(sv) =
            app.subagent_views.iter_mut().find(|s| s.id == "sub_001")
        {
            sv.stream_buffer.push_str("Hello ");
            sv.stream_buffer.push_str("World");
        }
        assert_eq!(app.subagent_views[0].stream_buffer, "Hello World");
    }

    #[test]
    fn test_subagent_done_flags_complete() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: "checking...".into(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        // Mark done
        if let Some(sv) =
            app.subagent_views.iter_mut().find(|s| s.id == "sub_001")
        {
            sv.done = true;
            sv.content = "No bugs found.".into();
            sv.stream_buffer.clear();
        }

        let sv = &app.subagent_views[0];
        assert!(sv.done);
        assert_eq!(sv.content, "No bugs found.");
        assert!(sv.stream_buffer.is_empty());
    }

    #[test]
    fn test_subagent_view_cycling() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });
        app.subagent_views.push(SubagentView {
            id: "sub_002".into(),
            name: "wiki".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        // Starts showing primary
        assert!(app.current_subagent_index.is_none());

        // Cycle to first subagent
        app.current_subagent_index = Some(0);
        assert_eq!(app.current_subagent_index, Some(0));

        // Cycle to second subagent
        app.current_subagent_index = Some(1);
        assert_eq!(app.current_subagent_index, Some(1));

        // Cycle back to primary
        app.current_subagent_index = None;
        assert!(app.current_subagent_index.is_none());
    }

    #[test]
    fn test_subagent_view_cycling_empty() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        assert!(app.subagent_views.is_empty());

        // Cycling when no subagents should be a no-op
        app.current_subagent_index = None;
        assert!(app.current_subagent_index.is_none());
    }

    #[test]
    fn test_subagent_tool_tracking() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        if let Some(sv) =
            app.subagent_views.iter_mut().find(|s| s.id == "sub_001")
        {
            sv.tool_calls.push(marshaling_protocol::ToolCallDisplay {
                id: "tc_1".into(),
                name: "read".into(),
                status: marshaling_protocol::ToolStatus::Running,
                changes: Vec::new(),
            });
            assert_eq!(sv.tool_calls.len(), 1);
            assert!(matches!(
                sv.tool_calls[0].status,
                marshaling_protocol::ToolStatus::Running
            ));
        }

        assert_eq!(app.subagent_views[0].tool_calls[0].name, "read");
    }

    #[test]
    fn test_subagent_reasoning_delta() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        if let Some(sv) =
            app.subagent_views.iter_mut().find(|s| s.id == "sub_001")
        {
            sv.reasoning_buffer.push_str("thinking...");
        }
        assert!(app.subagent_views[0].reasoning_buffer.contains("thinking"));
    }

    #[test]
    fn test_multiple_subagents_tracked() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());

        // Add two subagents
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: "reviewing...".into(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });
        app.subagent_views.push(SubagentView {
            id: "sub_002".into(),
            name: "wiki".into(),
            stream_buffer: "researching...".into(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        assert_eq!(app.subagent_views.len(), 2);
        assert_eq!(app.subagent_views[0].name, "review");
        assert_eq!(app.subagent_views[1].name, "wiki");

        // Update specific subagent by ID
        if let Some(sv) =
            app.subagent_views.iter_mut().find(|s| s.id == "sub_002")
        {
            sv.stream_buffer.push_str(" done");
        }
        assert_eq!(app.subagent_views[1].stream_buffer, "researching... done");
        assert_eq!(app.subagent_views[0].stream_buffer, "reviewing...");
    }

    #[test]
    fn test_subagent_done_inserts_conversation_message() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        // Simulate SubagentDone
        if let Some(sv) =
            app.subagent_views.iter_mut().find(|s| s.id == "sub_001")
        {
            sv.done = true;
            sv.content = "No bugs found.".into();
        }
        // Insert message (same logic as mod.rs)
        let name = app.subagent_views[0].name.clone();
        let result = app.subagent_views[0].content.clone();
        app.messages.push(DisplayMessage {
            role: crate::llm::Role::Assistant,
            content: format!("--- Subagent: {} ---\n{}", name, result),
            thinking: None,
            source: MessageSource::Conversation,
        });

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, crate::llm::Role::Assistant);
        assert!(app.messages[0].content.contains("Subagent: review"));
        assert!(app.messages[0].content.contains("No bugs found."));
        assert!(matches!(
            app.messages[0].source,
            MessageSource::Conversation
        ));
    }

    #[test]
    fn test_switch_view_cycling_logic() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });
        app.subagent_views.push(SubagentView {
            id: "sub_002".into(),
            name: "wiki".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        // Simulate the SwitchView cycling logic from mod.rs
        // Primary → subagent 0
        app.current_subagent_index = Some(0);
        assert_eq!(app.current_subagent_index, Some(0));

        // subagent 0 → subagent 1
        match app.current_subagent_index {
            None => app.current_subagent_index = Some(0),
            Some(idx) if idx + 1 < app.subagent_views.len() => {
                app.current_subagent_index = Some(idx + 1)
            }
            Some(_) => app.current_subagent_index = None,
        }
        assert_eq!(app.current_subagent_index, Some(1));

        // subagent 1 → primary
        match app.current_subagent_index {
            None => app.current_subagent_index = Some(0),
            Some(idx) if idx + 1 < app.subagent_views.len() => {
                app.current_subagent_index = Some(idx + 1)
            }
            Some(_) => app.current_subagent_index = None,
        }
        assert!(app.current_subagent_index.is_none());

        // primary → subagent 0 (wraps around)
        match app.current_subagent_index {
            None => app.current_subagent_index = Some(0),
            Some(idx) if idx + 1 < app.subagent_views.len() => {
                app.current_subagent_index = Some(idx + 1)
            }
            Some(_) => app.current_subagent_index = None,
        }
        assert_eq!(app.current_subagent_index, Some(0));
    }

    #[test]
    fn test_out_of_bounds_index_renders_primary() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        app.subagent_views.push(SubagentView {
            id: "sub_001".into(),
            name: "review".into(),
            stream_buffer: String::new(),
            reasoning_buffer: String::new(),
            tool_calls: Vec::new(),
            done: false,
            content: String::new(),
        });

        // Set an out-of-bounds index
        app.current_subagent_index = Some(5);
        // The render logic should fall back to primary when index is invalid
        let sv = app
            .current_subagent_index
            .and_then(|idx| app.subagent_views.get(idx));
        assert!(sv.is_none());
    }

    #[test]
    fn test_skill_selected_updates_current_skill() {
        let cfg = test_ui_config();
        let mut app = App::new(&cfg, cfg.model_info.clone());
        assert!(app.current_skill.is_none());

        app.current_skill = Some("python-conventions".into());
        assert_eq!(app.current_skill.as_deref(), Some("python-conventions"));

        // Switching to a new skill replaces
        app.current_skill = Some("rust-rules".into());
        assert_eq!(app.current_skill.as_deref(), Some("rust-rules"));
    }

    #[test]
    fn test_parse_ui_color_named() {
        assert_eq!(parse_ui_color("cyan"), Color::Cyan);
        assert_eq!(parse_ui_color("Green"), Color::Green);
        assert_eq!(parse_ui_color("RED"), Color::Red);
        assert_eq!(parse_ui_color("unknown"), Color::Cyan); // default
    }

    #[test]
    fn test_parse_ui_color_hex() {
        assert_eq!(parse_ui_color("#89b4fa"), Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(parse_ui_color("#000000"), Color::Rgb(0, 0, 0));
        assert_eq!(parse_ui_color("#ffffff"), Color::Rgb(255, 255, 255));
    }

    #[test]
    fn test_parse_ui_color_hex_invalid() {
        assert_eq!(parse_ui_color("#xyz"), Color::Cyan); // too short, falls through
        assert_eq!(parse_ui_color("#gggggg"), Color::Cyan); // invalid hex digits
    }

    #[test]
    fn test_render_change_summary_wraps_modified_diff_in_fenced_block() {
        let summary = render_change_summary(&[ToolCallDisplay {
            id: "1".into(),
            name: "edit".into(),
            status: ToolStatus::Success,
            changes: vec![marshaling_protocol::FileChange {
                path: "src/main.rs".into(),
                kind: marshaling_protocol::FileChangeKind::Modified,
                diff_lines: vec![
                    marshaling_protocol::DiffLine {
                        kind: marshaling_protocol::DiffLineKind::Added,
                        content: "let x = 1;".into(),
                    },
                    marshaling_protocol::DiffLine {
                        kind: marshaling_protocol::DiffLineKind::Removed,
                        content: "let x = 0;".into(),
                    },
                ],
                truncated: false,
            }],
        }]);

        assert!(summary.contains("```diff"));
        assert!(summary.contains("diff -- src/main.rs"));
        assert!(summary.contains("+let x = 1;"));
        assert!(summary.contains("-let x = 0;"));
        assert!(summary.contains("```"));
    }
}
