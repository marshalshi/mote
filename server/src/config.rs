use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Permission level for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Allow,
    Ask,
    Deny,
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Permission::Allow => write!(f, "allow"),
            Permission::Ask => write!(f, "ask"),
            Permission::Deny => write!(f, "deny"),
        }
    }
}

/// Top-level configuration for mote.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub model: ModelConfig,

    #[serde(rename = "providers")]
    pub providers: ProvidersConfig,

    #[serde(default = "default_history_dir")]
    pub history: HistoryConfig,

    #[serde(default)]
    pub prompts: PromptConfig,

    #[serde(default)]
    pub ui: UiConfig,

    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,

    #[serde(default)]
    pub permissions: GlobalPermissionConfig,

    /// Server configuration (port, bind address).
    #[serde(default)]
    pub server: ServerConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Audio transcription configuration.
    #[serde(default)]
    pub audio: AudioConfig,

    /// Optional Google Sheets sync configuration (defaults to disabled).
    #[serde(default)]
    pub google_sheets: GoogleSheetsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model_id: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_temperature() -> f32 {
    0.3
}
fn default_max_tokens() -> u32 {
    4096
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProvidersConfig {
    pub deepseek: Option<ProviderDeepSeek>,
    pub glm: Option<ProviderApiKey>,
    pub kimi: Option<ProviderApiKey>,
    pub minimax: Option<ProviderApiKey>,
    pub ollama: Option<ProviderOllama>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderDeepSeek {
    /// API key (deprecated in config.toml — use auth.json instead).
    /// When both are set, the secret is resolved from auth.json preferentially.
    pub api_key: Option<String>,
    #[serde(default = "default_deepseek_base_url")]
    pub base_url: String,
    pub default_model: Option<String>,
    pub default_max_tokens: Option<u32>,
}

fn default_deepseek_base_url() -> String {
    "https://api.deepseek.com/v1".to_string()
}

fn default_glm_base_url() -> &'static str {
    "https://api.z.ai/api"
}

fn default_kimi_base_url() -> &'static str {
    "https://api.moonshot.ai"
}

fn default_minimax_base_url() -> &'static str {
    "https://api.minimax.io"
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderApiKey {
    /// API key (deprecated in config.toml — use auth.json instead).
    /// When both are set, the secret is resolved from auth.json preferentially.
    pub api_key: Option<String>,
    pub base_url: String,
    pub default_model: Option<String>,
    pub default_max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderOllama {
    #[serde(default = "default_ollama_base_url")]
    pub base_url: String,
    pub default_model: Option<String>,
    pub default_max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AudioConfig {
    #[serde(default = "default_audio_provider")]
    pub provider: String,
    #[serde(default = "default_audio_model")]
    pub model: String,
    #[serde(default = "default_audio_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_audio_channels")]
    pub channels: u16,
    #[serde(default = "default_audio_realtime_url")]
    pub realtime_url: String,
}

fn default_audio_provider() -> String {
    "openai".into()
}

fn default_audio_model() -> String {
    "gpt-4o-mini-transcribe".into()
}

fn default_audio_sample_rate() -> u32 {
    24_000
}

fn default_audio_channels() -> u16 {
    1
}

fn default_audio_realtime_url() -> String {
    "https://api.openai.com/v1/audio/transcriptions".into()
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            provider: default_audio_provider(),
            model: default_audio_model(),
            sample_rate: default_audio_sample_rate(),
            channels: default_audio_channels(),
            realtime_url: default_audio_realtime_url(),
        }
    }
}

/// Optional Google Sheets sync configuration.
///
/// Slice 1: read-only service-account access to a spreadsheet tab plus the
/// eligibility/filter rules applied during normalization. When `enabled` is
/// false (the default), no sync runs and no credentials are loaded.
#[derive(Debug, Clone, Deserialize)]
pub struct GoogleSheetsConfig {
    /// Master switch. Default: false (sync disabled).
    #[serde(default)]
    pub enabled: bool,
    /// Spreadsheet ID from the Google Sheets URL. Required when enabled.
    #[serde(default)]
    pub spreadsheet_id: Option<String>,
    /// Tab name to read. Default: "US".
    #[serde(default = "default_google_sheets_sheet_name")]
    pub sheet_name: String,
    /// Path to the service-account JSON credential file. Required when enabled.
    #[serde(default)]
    pub credentials_path: Option<PathBuf>,
    /// Exact activity types eligible for import. Default: ["Boris Job", "Demo"].
    #[serde(default = "default_eligible_activity_types")]
    pub eligible_activity_types: Vec<String>,
}

fn default_google_sheets_sheet_name() -> String {
    "US".into()
}

fn default_eligible_activity_types() -> Vec<String> {
    vec!["Boris Job".into(), "Demo".into()]
}

impl Default for GoogleSheetsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            spreadsheet_id: None,
            sheet_name: default_google_sheets_sheet_name(),
            credentials_path: None,
            eligible_activity_types: default_eligible_activity_types(),
        }
    }
}

fn default_ollama_base_url() -> String {
    "http://localhost:11434".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct HistoryConfig {
    pub dir: PathBuf,
}
fn default_history_dir() -> HistoryConfig {
    let dir = dirs::home_dir()
        .map(|h| h.join(".config").join("mote").join("history"))
        .unwrap_or_else(|| PathBuf::from("history"));
    HistoryConfig { dir }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptConfig {
    #[serde(default = "default_prompt_file")]
    pub default: PathBuf,
}

fn built_in_assets_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn default_prompt_file() -> PathBuf {
    built_in_assets_root()
        .join("prompts")
        .join("system")
        .join("mote.md")
}
impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            default: default_prompt_file(),
        }
    }
}

/// UI accent color and display settings.
#[derive(Debug, Clone, Deserialize)]
pub struct UiConfig {
    /// Accent bar color for the input area.
    #[serde(default = "default_accent")]
    pub input_accent: String,
    /// Accent bar color for user messages.
    #[serde(default = "default_accent")]
    pub user_accent: String,
}
fn default_accent() -> String {
    "cyan".into()
}
impl Default for UiConfig {
    fn default() -> Self {
        Self {
            input_accent: default_accent(),
            user_accent: default_accent(),
        }
    }
}

/// Server bind configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Port to listen on (default: 9847).
    #[serde(default = "default_server_port")]
    pub port: u16,
    /// Normal tool-capable turn budget before a text-only finalization step
    /// (default: 30).
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    /// Agent name used when no agent is specified (default: "build").
    #[serde(default = "default_agent_name")]
    pub default_agent: String,
}

fn default_server_port() -> u16 {
    9847
}
fn default_max_steps() -> usize {
    30
}
fn default_agent_name() -> String {
    marshaling_protocol::DEFAULT_AGENT_NAME.into()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_server_port(),
            max_steps: default_max_steps(),
            default_agent: default_agent_name(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    /// Directory for log files.
    #[serde(default = "default_log_dir")]
    pub dir: PathBuf,
}

fn default_log_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".config").join("mote").join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"))
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: default_log_dir(),
        }
    }
}

/// Per-role configuration within an agent's `roles` array.
#[derive(Debug, Clone, Deserialize)]
pub struct RoleConfig {
    /// Role identifier (e.g., "orchestrator", "coder", "reviewer").
    pub name: String,
    /// Agent-specific system instructions for this role. Falls back to
    /// the agent-level `instructions` when not set.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Optional model override: "provider/model_id" or just "model_id".
    /// Falls back to agent.model -> config defaults.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional temperature override for this role.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Optional max_tokens override for this role.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Per-agent override within the `[agents]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    #[allow(dead_code)]
    pub model: Option<String>,
    #[allow(dead_code)]
    pub temperature: Option<f32>,
    #[allow(dead_code)]
    pub max_tokens: Option<u32>,
    /// Per-tool permission overrides for this agent: tool_name → Permission
    #[serde(default)]
    pub permissions: HashMap<String, Permission>,
    /// Agent-specific system instructions (markdown text), injected as a prompt layer.
    /// If set, these instructions appear in the system prompt for this agent only.
    #[serde(default)]
    pub instructions: Option<String>,
    /// If true, omit the global ~/.config/mote/AGENTS.md layer for this agent.
    #[serde(default)]
    pub disable_user_agents_md: bool,
    /// If true, omit the shared system prompt layer for this agent.
    ///
    /// The field name intentionally matches the current agent-definition
    /// contract spelling. `disable_system_prompt` is accepted as an alias.
    #[serde(default, alias = "disable_system_prompt")]
    pub disble_system_prompt: bool,
    /// If true, omit the workspace/repo AGENTS.md layer for this agent.
    /// Also accepts `disable_repo_agents_md` as a serde alias for YAML compat.
    #[serde(default, alias = "disable_repo_agents_md")]
    pub disable_workspace_agents_md: bool,
    /// Optional reminder profile that changes the dynamic system reminder wording.
    /// When `Some("pm")`, the reminder emphasizes DB as truth source, validate
    /// before writes, ask one clarifying question when ambiguous, and surface
    /// alerts proactively. When None or any other value, the default coding-oriented
    /// reminder is used.
    #[serde(default)]
    pub reminder_profile: Option<String>,
    /// Agent mode: "primary" (user-selectable, default), "subagent" (tool-only), "all" (both).
    #[serde(default = "default_agent_mode")]
    pub mode: String,
    /// Optional list of roles for native loop orchestration.
    /// When present, the first role is the orchestrator and the loop
    /// can switch between roles via the `switch_role` tool.
    /// When None (legacy), the agent behaves as a single-role agent.
    #[serde(default)]
    pub roles: Option<Vec<RoleConfig>>,
}

fn default_agent_mode() -> String {
    "primary".into()
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: None,
            temperature: None,
            max_tokens: None,
            permissions: HashMap::new(),
            instructions: None,
            disable_user_agents_md: false,
            disble_system_prompt: false,
            disable_workspace_agents_md: false,
            reminder_profile: None,
            mode: default_agent_mode(),
            roles: None,
        }
    }
}

impl AgentConfig {
    /// Whether this agent should appear in the user-facing /agent list.
    pub fn is_user_selectable(&self) -> bool {
        self.mode == "primary" || self.mode == "all"
    }

    /// Whether this agent can be invoked as a subagent tool.
    pub fn is_subagent_callable(&self) -> bool {
        self.mode == "subagent" || self.mode == "all"
    }

    /// Validate the role list: must not be empty, names must be non-empty and unique.
    pub fn validate_roles(&self) -> Result<(), String> {
        if let Some(ref roles) = self.roles {
            if roles.is_empty() {
                return Err("roles list is empty".into());
            }
            let mut seen = std::collections::HashSet::new();
            for role in roles {
                let name = role.name.trim();
                if name.is_empty() {
                    return Err("role name must not be empty".into());
                }
                if !seen.insert(name.to_string()) {
                    return Err(format!("duplicate role name: {}", name));
                }
            }
        }
        Ok(())
    }

    /// Resolve the effective instructions for a role.
    /// Falls back: role.instructions -> agent.instructions -> None.
    pub fn effective_role_instructions(
        &self,
        role: &RoleConfig,
    ) -> Option<String> {
        role.instructions
            .clone()
            .or_else(|| self.instructions.clone())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// Global permission defaults (applied to all agents unless overridden).
#[derive(Debug, Clone, Deserialize)]
pub struct GlobalPermissionConfig {
    /// Default permission for all tools: Allow, Ask (default), or Deny.
    #[serde(default = "default_global_perm")]
    pub default: Permission,
    /// Per-tool defaults: tool_name → Permission
    #[serde(flatten)]
    pub tools: HashMap<String, Permission>,
}

fn default_global_perm() -> Permission {
    Permission::Ask
}

impl Default for GlobalPermissionConfig {
    fn default() -> Self {
        Self {
            default: Permission::Ask,
            tools: HashMap::new(),
        }
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!("Failed to read config: {}", path.display())
        })?;
        Ok(toml::from_str(&raw)
            .context("Failed to parse config.toml — check the format")?)
    }

    pub fn effective_provider(&self, agent_override: Option<&str>) -> String {
        if let Some(model_str) = agent_override {
            if let Some((provider, _)) = model_str.split_once('/') {
                return provider.to_string();
            }
        }
        self.model.provider.clone()
    }

    pub fn effective_model_id(&self, agent_override: Option<&str>) -> String {
        if let Some(model_str) = agent_override {
            if let Some((_, model_id)) = model_str.split_once('/') {
                return model_id.to_string();
            }
            return model_str.to_string();
        }
        let def = match self.model.provider.as_str() {
            "deepseek" => self
                .providers
                .deepseek
                .as_ref()
                .and_then(|p| p.default_model.clone()),
            "glm" => self
                .providers
                .glm
                .as_ref()
                .and_then(|p| p.default_model.clone()),
            "kimi" => self
                .providers
                .kimi
                .as_ref()
                .and_then(|p| p.default_model.clone()),
            "minimax" => self
                .providers
                .minimax
                .as_ref()
                .and_then(|p| p.default_model.clone()),
            "ollama" => self
                .providers
                .ollama
                .as_ref()
                .and_then(|p| p.default_model.clone()),
            _ => None,
        };
        def.unwrap_or_else(|| self.model.model_id.clone())
    }

    pub fn effective_model_info(&self, agent_override: Option<&str>) -> String {
        let provider = self.effective_provider(agent_override);
        let model_id = self.effective_model_id(agent_override);
        format!("{provider}/{model_id}")
    }

    /// Resolve effective provider and model_id for a role, with the standard
    /// fallback chain: role.model -> agent.model -> config defaults.
    ///
    /// Provider cascade:
    ///   1. role_model explicit prefix (e.g., "ollama/qwen" → "ollama")
    ///   2. agent_model explicit prefix (e.g., "ollama/llama3" → "ollama")
    ///   3. config default provider
    ///
    /// Model_id cascade:
    ///   1. role_model id (strip prefix if present)
    ///   2. agent_model id (strip prefix if present)
    ///   3. config default model_id
    pub fn effective_role_model(
        &self,
        role_model: Option<&str>,
        agent_model: Option<&str>,
    ) -> (String, String) {
        // Resolve provider
        let provider = if let Some(rm) = role_model {
            if let Some((p, _)) = rm.split_once('/') {
                p.to_string()
            } else if let Some(am) = agent_model {
                if let Some((p, _)) = am.split_once('/') {
                    p.to_string()
                } else {
                    self.model.provider.clone()
                }
            } else {
                self.model.provider.clone()
            }
        } else if let Some(am) = agent_model {
            if let Some((p, _)) = am.split_once('/') {
                p.to_string()
            } else {
                self.model.provider.clone()
            }
        } else {
            self.model.provider.clone()
        };

        // Resolve model_id
        let model_id = if let Some(rm) = role_model {
            if let Some((_, m)) = rm.split_once('/') {
                m.to_string()
            } else {
                rm.to_string()
            }
        } else if let Some(am) = agent_model {
            if let Some((_, m)) = am.split_once('/') {
                m.to_string()
            } else {
                am.to_string()
            }
        } else {
            self.effective_model_id(None)
        };

        (provider, model_id)
    }

    pub fn effective_temperature(&self, agent_override: Option<f32>) -> f32 {
        agent_override.unwrap_or(self.model.temperature)
    }

    /// Resolve effective max_tokens: agent → provider default → global.
    pub fn effective_max_tokens(
        &self,
        agent_override: Option<u32>,
        provider_name: &str,
    ) -> u32 {
        if let Some(t) = agent_override {
            return t;
        }
        let def = match provider_name {
            "deepseek" => self
                .providers
                .deepseek
                .as_ref()
                .and_then(|p| p.default_max_tokens),
            "glm" => self
                .providers
                .glm
                .as_ref()
                .and_then(|p| p.default_max_tokens),
            "kimi" => self
                .providers
                .kimi
                .as_ref()
                .and_then(|p| p.default_max_tokens),
            "minimax" => self
                .providers
                .minimax
                .as_ref()
                .and_then(|p| p.default_max_tokens),
            "ollama" => self
                .providers
                .ollama
                .as_ref()
                .and_then(|p| p.default_max_tokens),
            _ => None,
        };
        def.unwrap_or(self.model.max_tokens)
    }

    /// Return the raw accent color strings (served to the client via HTTP).
    pub fn input_accent(&self) -> &str {
        &self.ui.input_accent
    }
    pub fn user_accent(&self) -> &str {
        &self.ui.user_accent
    }

    /// Return agent names for client UI.
    #[allow(dead_code)] // public API, used in tests
    pub fn agent_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.agents.keys().cloned().collect();
        names.sort();
        names
    }

    fn expand(val: &str) -> String {
        use std::sync::LazyLock;
        static ENV_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"\$\{([^}]+)\}|\$([A-Za-z_][A-Za-z0-9_]*)")
                .unwrap()
        });
        ENV_RE
            .replace_all(val, |caps: &regex::Captures| {
                let key = caps
                    .get(1)
                    .or_else(|| caps.get(2))
                    .map(|m| m.as_str())
                    .unwrap_or("");
                std::env::var(key).unwrap_or_else(|_| {
                    caps.get(0)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default()
                })
            })
            .to_string()
    }

    /// Get the DeepSeek API key from auth.json first, falling back to config.toml
    /// with a deprecation warning.
    pub fn resolve_deepseek_api_key(
        &self,
        auth: &crate::auth::Auth,
    ) -> Result<String> {
        // 1. Check auth.json first
        if let Some(key) = auth.api_key("deepseek") {
            return Ok(Self::expand(key));
        }
        // 2. Fallback to config.toml (deprecated)
        if let Some(key) = self
            .providers
            .deepseek
            .as_ref()
            .and_then(|p| p.api_key.as_deref())
        {
            tracing::warn!(
                "Deprecation: deepseek.api_key in config.toml is deprecated. Move it to auth.json (~/.config/mote/auth.json)"
            );
            return Ok(Self::expand(key));
        }
        anyhow::bail!(
            "No DeepSeek API key found. Add it to ~/.config/mote/auth.json: {{\"deepseek\":{{\"api_key\":\"sk-...\"}}}}"
        );
    }

    pub fn deepseek_base_url(&self) -> Result<String> {
        Ok(Self::expand(
            &self
                .providers
                .deepseek
                .as_ref()
                .context("DeepSeek not configured")?
                .base_url,
        )
        .trim_end_matches('/')
        .to_string())
    }
    pub fn ollama_base_url(&self) -> Result<String> {
        Ok(Self::expand(
            &self
                .providers
                .ollama
                .as_ref()
                .context("Ollama not configured")?
                .base_url,
        )
        .trim_end_matches('/')
        .to_string())
    }

    pub fn resolve_provider_api_key(
        &self,
        auth: &crate::auth::Auth,
        provider: &str,
    ) -> Result<String> {
        if let Some(key) = auth.api_key(provider) {
            return Ok(Self::expand(key));
        }
        if let Some(key) =
            self.provider_api_key_config(provider)?.api_key.as_deref()
        {
            tracing::warn!(
                "Deprecation: {provider}.api_key in config.toml is deprecated. Move it to auth.json (~/.config/mote/auth.json)"
            );
            return Ok(Self::expand(key));
        }
        anyhow::bail!(
            "No {provider} API key found. Run --login {provider} or add it to ~/.config/mote/auth.json."
        );
    }

    pub fn provider_base_url(&self, provider: &str) -> Result<String> {
        let base_url = match provider {
            "glm" => self
                .providers
                .glm
                .as_ref()
                .map(|cfg| cfg.base_url.as_str())
                .unwrap_or(default_glm_base_url()),
            "kimi" => self
                .providers
                .kimi
                .as_ref()
                .map(|cfg| cfg.base_url.as_str())
                .unwrap_or(default_kimi_base_url()),
            "minimax" => self
                .providers
                .minimax
                .as_ref()
                .map(|cfg| cfg.base_url.as_str())
                .unwrap_or(default_minimax_base_url()),
            _ => {
                return Ok(Self::expand(
                    &self.provider_api_key_config(provider)?.base_url,
                )
                .trim_end_matches('/')
                .to_string());
            }
        };
        Ok(Self::expand(base_url).trim_end_matches('/').to_string())
    }

    pub fn has_provider_api_key_source(
        &self,
        auth: &crate::auth::Auth,
        provider: &str,
    ) -> bool {
        if auth.api_key(provider).is_some() {
            return true;
        }
        match provider {
            "deepseek" => self
                .providers
                .deepseek
                .as_ref()
                .and_then(|p| p.api_key.as_deref())
                .is_some(),
            "glm" => self
                .providers
                .glm
                .as_ref()
                .and_then(|p| p.api_key.as_deref())
                .is_some(),
            "kimi" => self
                .providers
                .kimi
                .as_ref()
                .and_then(|p| p.api_key.as_deref())
                .is_some(),
            "minimax" => self
                .providers
                .minimax
                .as_ref()
                .and_then(|p| p.api_key.as_deref())
                .is_some(),
            _ => false,
        }
    }

    pub fn resolve_audio_api_key(
        &self,
        auth: &crate::auth::Auth,
    ) -> Result<String> {
        if self.audio.provider != "openai" {
            anyhow::bail!(
                "Unsupported audio provider '{}'. Supported: openai",
                self.audio.provider
            );
        }
        auth.api_key("openai")
            .map(Self::expand)
            .context("No OpenAI API key found. Run --login openai or add {\"openai\":{\"api_key\":\"sk-...\"}} to ~/.config/mote/auth.json.")
    }

    fn provider_api_key_config(
        &self,
        provider: &str,
    ) -> Result<&ProviderApiKey> {
        match provider {
            "glm" => self.providers.glm.as_ref().context("GLM not configured"),
            "kimi" => {
                self.providers.kimi.as_ref().context("Kimi not configured")
            }
            "minimax" => self
                .providers
                .minimax
                .as_ref()
                .context("MiniMax not configured"),
            _ => anyhow::bail!("Unsupported API-key provider: {provider}"),
        }
    }

    /// Resolve the effective permission for a tool, given the current agent name.
    /// Resolution order: agent-specific → global tool → global default.
    #[allow(dead_code)] // public API, used in tests
    pub fn resolve_permission(
        &self,
        agent_name: &str,
        tool_name: &str,
    ) -> Permission {
        // 1. Agent-specific permission
        if let Some(agent) = self.agents.get(agent_name) {
            if let Some(perm) = agent.permissions.get(tool_name) {
                return *perm;
            }
        }
        // 2. Global tool permission
        if let Some(perm) = self.permissions.tools.get(tool_name) {
            return *perm;
        }
        // 3. Global default
        self.permissions.default
    }
}

/// Load agent definitions from Markdown files.
///
/// Reads from two locations, with later sources overriding earlier ones:
/// 1. Built-in agents shipped in the repo: `prompts/agents/*.md`
/// 2. User agents: `~/.config/mote/agents/*.md` (falls back to `./agents`)
pub fn load_file_agents() -> HashMap<String, AgentConfig> {
    let mut agents = HashMap::new();

    // 1. Built-in agents shipped in the repo.
    load_agents_from_dir(
        &built_in_assets_root().join("prompts").join("agents"),
        &mut agents,
    );

    // 2. User agents (override built-in on name collision).
    let user_dir = resolve_config_path("agents");
    load_agents_from_dir(&user_dir, &mut agents);

    agents
}

/// Read `*.md` agent files from `dir` into `agents`, overwriting on collision.
fn load_agents_from_dir(dir: &Path, agents: &mut HashMap<String, AgentConfig>) {
    if !dir.is_dir() {
        return;
    }
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "md") {
                    if let Some(stem) =
                        path.file_stem().and_then(|s| s.to_str())
                    {
                        match std::fs::read_to_string(&path) {
                            Ok(content) => {
                                match parse_agent_markdown(&content) {
                                    Ok(mut cfg) => {
                                        // Validate mode
                                        let mode = cfg.mode.clone();
                                        if !["primary", "subagent", "all"]
                                            .contains(&mode.as_str())
                                        {
                                            tracing::warn!(
                                                "Agent '{}' has unknown mode '{}', defaulting to 'primary'",
                                                stem,
                                                mode
                                            );
                                            cfg.mode = "primary".into();
                                        }
                                        agents.insert(stem.to_string(), cfg);
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Failed to parse agent file '{}': {e}",
                                            path.display()
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to read agent file '{}': {e}",
                                    path.display()
                                );
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "Failed to read agents directory '{}': {e}",
                dir.display()
            );
        }
    }
}

fn parse_agent_markdown(content: &str) -> Result<AgentConfig> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalized.trim();

    let (mut cfg, body) = match split_markdown_frontmatter(trimmed) {
        Some((frontmatter, body)) => (
            serde_yaml::from_str::<AgentConfig>(frontmatter)
                .context("Failed to parse YAML frontmatter")?,
            body,
        ),
        None => (AgentConfig::default(), trimmed),
    };

    let instructions = body.trim();
    if !instructions.is_empty() {
        cfg.instructions = Some(instructions.to_string());
    }

    Ok(cfg)
}

fn split_markdown_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---\n")?;
    let (frontmatter, body) = rest.split_once("\n---")?;
    Some((frontmatter, body.trim_start_matches('\n')))
}

/// Get all agents: merged from config.toml `[agents]` and file-based agents.
/// Config.toml agents take precedence on name collision.
pub fn all_agents(
    config_agents: &HashMap<String, AgentConfig>,
) -> HashMap<String, AgentConfig> {
    let mut agents = load_file_agents();
    for (name, cfg) in config_agents {
        agents.insert(name.clone(), cfg.clone());
    }
    agents
}

/// Resolve a config file path: check `~/.config/mote/` first, fall back to CWD.
fn resolve_config_path(filename: &str) -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        let p = home.join(".config").join("mote").join(filename);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_var_expansion() {
        unsafe { std::env::set_var("TEST_API_KEY", "sk-test123") };
        assert_eq!(Config::expand("${TEST_API_KEY}"), "sk-test123");
    }
    #[test]
    fn test_env_var_dollar_brace() {
        unsafe { std::env::set_var("MY_VAR", "value") };
        assert_eq!(Config::expand("${MY_VAR}"), "value");
    }
    #[test]
    fn test_env_var_unset_keeps_literal() {
        unsafe { std::env::remove_var("UNSET_VAR_XYZ") };
        assert_eq!(
            Config::expand("prefix_${UNSET_VAR_XYZ}_suffix"),
            "prefix_${UNSET_VAR_XYZ}_suffix"
        );
    }
    #[test]
    fn test_env_var_no_false_positive() {
        assert_eq!(Config::expand("costs $5.00"), "costs $5.00");
    }

    #[test]
    fn test_config_parse() {
        let toml = r#"
[model]
provider = "deepseek"
model_id = "deepseek-chat"
temperature = 0.5

[providers.deepseek]
api_key = "sk-test"
base_url = "https://api.deepseek.com/v1"
default_max_tokens = 8192

[history]
dir = "history"

[prompts]
default = "prompts/system/mote.md"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.model.provider, "deepseek");
        assert_eq!(config.model.max_tokens, 4096);
        assert_eq!(
            config
                .providers
                .deepseek
                .as_ref()
                .unwrap()
                .default_max_tokens,
            Some(8192)
        );
    }

    #[test]
    fn test_effective_max_tokens_agent() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "ollama"
model_id = "test"
[providers.ollama]
base_url = "http://localhost:11434"
[agents.myagent]
max_tokens = 2048
"#,
        )
        .unwrap();
        assert_eq!(config.effective_max_tokens(Some(2048), "ollama"), 2048);
        assert_eq!(config.effective_max_tokens(None, "ollama"), 4096);
    }

    #[test]
    fn test_effective_max_tokens_provider_default() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "deepseek"
model_id = "test"
[providers.deepseek]
api_key = "x"
default_max_tokens = 16384
"#,
        )
        .unwrap();
        assert_eq!(config.effective_max_tokens(None, "deepseek"), 16384);
    }

    #[test]
    fn test_new_api_key_provider_defaults() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "glm"
model_id = "fallback"
max_tokens = 4096

[providers.glm]
base_url = "https://api.z.ai/api"
default_model = "glm-5.2"
default_max_tokens = 65536

[providers.kimi]
base_url = "https://api.moonshot.ai"
default_model = "kimi-k2.6"
default_max_tokens = 32768

[providers.minimax]
base_url = "https://api.minimax.io"
default_model = "MiniMax-M3"
default_max_tokens = 131072
"#,
        )
        .unwrap();

        assert_eq!(config.effective_model_id(None), "glm-5.2");
        assert_eq!(config.effective_max_tokens(None, "glm"), 65536);
        assert_eq!(config.effective_max_tokens(None, "kimi"), 32768);
        assert_eq!(config.effective_max_tokens(None, "minimax"), 131072);
        assert_eq!(
            config.provider_base_url("glm").unwrap(),
            "https://api.z.ai/api"
        );
        assert_eq!(
            config.provider_base_url("kimi").unwrap(),
            "https://api.moonshot.ai"
        );
        assert_eq!(
            config.provider_base_url("minimax").unwrap(),
            "https://api.minimax.io"
        );
    }

    #[test]
    fn test_provider_api_key_prefers_auth_json() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "kimi"
model_id = "kimi-k2.6"

[providers.kimi]
api_key = "config-key"
base_url = "https://api.moonshot.ai"
"#,
        )
        .unwrap();
        let mut providers = HashMap::new();
        providers.insert(
            "kimi".into(),
            crate::auth::ProviderAuth {
                api_key: Some("auth-key".into()),
                token: None,
                extra: HashMap::new(),
            },
        );
        let auth = crate::auth::Auth { providers };

        assert_eq!(
            config.resolve_provider_api_key(&auth, "kimi").unwrap(),
            "auth-key"
        );
    }

    #[test]
    fn test_accent_color_default() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "ollama"
model_id = "test"
[providers.ollama]
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();
        assert_eq!(config.input_accent(), "cyan");
        assert_eq!(config.user_accent(), "cyan");
    }

    #[test]
    fn test_accent_color_custom() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "ollama"
model_id = "t"
[providers.ollama]
base_url = "http://localhost:11434"
[ui]
input_accent = "green"
user_accent = "blue"
"#,
        )
        .unwrap();
        assert_eq!(config.input_accent(), "green");
        assert_eq!(config.user_accent(), "blue");
    }

    #[test]
    fn test_deepseek_api_key_with_expansion() {
        unsafe { std::env::set_var("DS_KEY", "sk-real-key") };
        let config: Config = toml::from_str(
            r#"
[model]
provider = "deepseek"
model_id = "x"
[providers.deepseek]
api_key = "${DS_KEY}"
base_url = "https://api.deepseek.com/v1"
"#,
        )
        .unwrap();
        // With an empty auth, should fall back to config.toml
        let auth = crate::auth::Auth::default();
        assert_eq!(
            config.resolve_deepseek_api_key(&auth).unwrap(),
            "sk-real-key"
        );
    }

    #[test]
    fn test_all_agents_merges_and_overrides() {
        let mut config_agents = HashMap::new();
        config_agents.insert(
            "code".into(),
            AgentConfig {
                model: Some("ollama/qwen".into()),
                temperature: Some(0.3),
                max_tokens: Some(4096),
                permissions: HashMap::new(),
                instructions: None,
                disable_user_agents_md: false,
                disble_system_prompt: false,
                disable_workspace_agents_md: false,
                reminder_profile: None,
                mode: "primary".into(),
                roles: None,
            },
        );
        // all_agents should include file agents (if any) AND config agents, with config winning
        let merged = all_agents(&config_agents);
        assert!(
            merged.contains_key("code"),
            "config agent should be present"
        );
        assert_eq!(merged["code"].model.as_deref(), Some("ollama/qwen"));
    }

    #[test]
    fn test_resolve_permission_agent_override() {
        // Agent-specific permission should win over global
        let config: Config = toml::from_str(
            r#"
[model]
provider = "deepseek"
model_id = "x"
[providers.deepseek]
api_key = "placeholder"
base_url = "https://api.deepseek.com/v1"
[permissions]
default = "deny"
bash = "ask"
[agents.code]
permissions = { bash = "allow" }
"#,
        )
        .unwrap();
        // Agent "code" overrides bash to "allow"
        assert_eq!(
            config.resolve_permission("code", "bash"),
            Permission::Allow
        );
        // No agent → falls back to global tool → global default
        assert_eq!(
            config.resolve_permission("nonexistent", "bash"),
            Permission::Ask
        );
        assert_eq!(
            config.resolve_permission("nonexistent", "write"),
            Permission::Deny
        );
    }

    #[test]
    fn test_permissions_default_to_ask_when_omitted() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "ollama"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();

        assert_eq!(config.permissions.default, Permission::Ask);
        assert_eq!(
            config.resolve_permission("missing-agent", "bash"),
            Permission::Ask
        );
    }

    #[test]
    fn test_parse_agent_markdown_with_frontmatter_and_body() {
        let cfg = parse_agent_markdown(
            r#"---
model: ollama/qwen
temperature: 0.2
max_tokens: 2048
mode: all
permissions:
  bash: deny
---

# Review

You are a review agent.
"#,
        )
        .unwrap();

        assert_eq!(cfg.model.as_deref(), Some("ollama/qwen"));
        assert_eq!(cfg.temperature, Some(0.2));
        assert_eq!(cfg.max_tokens, Some(2048));
        assert_eq!(cfg.mode, "all");
        assert_eq!(
            cfg.permissions.get("bash").copied(),
            Some(Permission::Deny)
        );
        assert_eq!(
            cfg.instructions.as_deref(),
            Some("# Review\n\nYou are a review agent.")
        );
    }

    #[test]
    fn test_parse_agent_markdown_without_frontmatter_uses_defaults() {
        let cfg = parse_agent_markdown("# Build\n\nUse defaults.").unwrap();

        assert_eq!(cfg.mode, "primary");
        assert_eq!(
            cfg.instructions.as_deref(),
            Some("# Build\n\nUse defaults.")
        );
    }

    #[test]
    fn test_parse_agent_markdown_supports_crlf_frontmatter() {
        let cfg = parse_agent_markdown(
            "---\r\nmodel: ollama/qwen\r\nmode: subagent\r\n---\r\n\r\nRun checks.\r\n",
        )
        .unwrap();

        assert_eq!(cfg.model.as_deref(), Some("ollama/qwen"));
        assert_eq!(cfg.mode, "subagent");
        assert_eq!(cfg.instructions.as_deref(), Some("Run checks."));
    }

    #[test]
    fn test_load_file_agents_reads_markdown_files() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agents");
        std::fs::create_dir(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("review.md"),
            r#"---
model: ollama/qwen
temperature: 0.2
max_tokens: 2048
---

Review instructions.
"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("plan.md"),
            r#"---
model: ollama/deepseek
permissions:
  bash: deny
---

Plan instructions.
"#,
        )
        .unwrap();

        let content =
            std::fs::read_to_string(agent_dir.join("review.md")).unwrap();
        let cfg = parse_agent_markdown(&content).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("ollama/qwen"));
        assert_eq!(cfg.temperature, Some(0.2));
        assert_eq!(cfg.instructions.as_deref(), Some("Review instructions."));

        let content2 =
            std::fs::read_to_string(agent_dir.join("plan.md")).unwrap();
        let cfg2 = parse_agent_markdown(&content2).unwrap();
        assert_eq!(cfg2.model.as_deref(), Some("ollama/deepseek"));
        assert_eq!(
            cfg2.permissions.get("bash").copied(),
            Some(Permission::Deny)
        );
        assert_eq!(cfg2.instructions.as_deref(), Some("Plan instructions."));
    }

    #[test]
    fn test_default_prompt_file_points_to_built_in_asset() {
        let path = default_prompt_file();
        assert!(path.ends_with("prompts/system/mote.md"));
        assert!(path.is_absolute());
    }

    #[test]
    fn test_all_agents_empty_when_no_config_agents() {
        let merged = all_agents(&HashMap::new());
        // File agents depend on the user's ~/.config/mote/agents/ directory.
        // If markdown agent files exist, ensure at least some are loaded.
        let agent_dir = dirs::home_dir()
            .map(|h| h.join(".config").join("mote").join("agents"));
        let has_markdown_agents = agent_dir.as_ref().map_or(false, |d| {
            d.is_dir()
                && std::fs::read_dir(d)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|entry| {
                        entry
                            .path()
                            .extension()
                            .map_or(false, |ext| ext == "md")
                    })
        });
        if has_markdown_agents {
            assert!(!merged.is_empty(), "expected file agents to be loaded");
        }
        // The function should never crash regardless
    }

    #[test]
    fn test_agent_mode_default_is_primary() {
        let cfg: AgentConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.mode, "primary");
        assert!(cfg.is_user_selectable());
        assert!(!cfg.is_subagent_callable());
    }

    #[test]
    fn test_agent_mode_primary() {
        let cfg: AgentConfig = toml::from_str(r#"mode = "primary""#).unwrap();
        assert!(cfg.is_user_selectable());
        assert!(!cfg.is_subagent_callable());
    }

    #[test]
    fn test_agent_mode_subagent() {
        let cfg: AgentConfig = toml::from_str(r#"mode = "subagent""#).unwrap();
        assert!(!cfg.is_user_selectable());
        assert!(cfg.is_subagent_callable());
    }

    #[test]
    fn test_agent_mode_all() {
        let cfg: AgentConfig = toml::from_str(r#"mode = "all""#).unwrap();
        assert!(cfg.is_user_selectable());
        assert!(cfg.is_subagent_callable());
    }

    #[test]
    fn test_agent_mode_with_permissions() {
        let cfg: AgentConfig = toml::from_str(
            r#"
mode = "all"
[permissions]
read = "ask"
bash = "allow"
subagent = "deny"
"#,
        )
        .unwrap();
        assert_eq!(cfg.mode, "all");
        assert_eq!(cfg.permissions.get("read").copied(), Some(Permission::Ask));
        assert_eq!(
            cfg.permissions.get("bash").copied(),
            Some(Permission::Allow)
        );
        assert_eq!(
            cfg.permissions.get("subagent").copied(),
            Some(Permission::Deny)
        );
        // Unknown keys are ignored
        assert!(cfg.permissions.get("nonexistent").is_none());
    }

    #[test]
    fn test_agent_mode_serialized_from_markdown_file() {
        let dir = tempfile::tempdir().unwrap();
        let agent_file = dir.path().join("test_agent.md");
        std::fs::write(
            &agent_file,
            r#"---
model: deepseek/deepseek-v4-flash
mode: all
temperature: 0.2
permissions:
  bash: deny
---

Use markdown instructions.
"#,
        )
        .unwrap();
        let content = std::fs::read_to_string(&agent_file).unwrap();
        let cfg = parse_agent_markdown(&content).unwrap();
        assert_eq!(cfg.mode, "all");
        assert!(cfg.is_user_selectable());
        assert!(cfg.is_subagent_callable());
        assert_eq!(cfg.temperature, Some(0.2));
        assert_eq!(
            cfg.permissions.get("bash").copied(),
            Some(Permission::Deny)
        );
        assert_eq!(
            cfg.instructions.as_deref(),
            Some("Use markdown instructions.")
        );
    }

    #[test]
    fn test_server_config_default_port() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();
        assert_eq!(config.server.port, 9847, "default port should be 9847");
    }

    #[test]
    fn test_server_config_custom_port() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
[server]
port = 9848
"#,
        )
        .unwrap();
        assert_eq!(config.server.port, 9848);
    }

    #[test]
    fn test_server_config_empty_section_defaults() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
[server]
"#,
        )
        .unwrap();
        assert_eq!(
            config.server.port, 9847,
            "empty [server] section should default to 9847"
        );
    }

    #[test]
    fn test_server_config_max_steps_default() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();
        assert_eq!(config.server.max_steps, 30);
    }

    #[test]
    fn test_server_config_max_steps_custom() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
[server]
max_steps = 25
"#,
        )
        .unwrap();
        assert_eq!(config.server.max_steps, 25);
    }

    #[test]
    fn test_logging_dir_default() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();
        assert!(config.logging.dir.to_string_lossy().contains("mote/logs"));
    }

    #[test]
    fn test_logging_dir_custom() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
[logging]
dir = "/tmp/mote-logs"
"#,
        )
        .unwrap();
        assert_eq!(config.logging.dir, PathBuf::from("/tmp/mote-logs"));
    }

    #[test]
    fn test_google_sheets_config_defaults() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();
        let gs = &config.google_sheets;
        assert!(!gs.enabled, "google_sheets should be disabled by default");
        assert!(gs.spreadsheet_id.is_none());
        assert_eq!(gs.sheet_name, "US");
        assert!(gs.credentials_path.is_none());
        assert_eq!(
            gs.eligible_activity_types,
            vec!["Boris Job".to_string(), "Demo".to_string()]
        );
    }

    #[test]
    fn test_google_sheets_config_parses_explicit() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
[google_sheets]
enabled = true
spreadsheet_id = "1lE3Mvs_mXsY-LskbCyE9Z3Y8de4_71pcJHB3YRQXJ4Y"
sheet_name = "Master Jobs"
credentials_path = "/tmp/service-account.json"
eligible_activity_types = ["Boris Job", "Demo", "Installation"]
"#,
        )
        .unwrap();
        let gs = &config.google_sheets;
        assert!(gs.enabled);
        assert_eq!(
            gs.spreadsheet_id.as_deref(),
            Some("1lE3Mvs_mXsY-LskbCyE9Z3Y8de4_71pcJHB3YRQXJ4Y")
        );
        assert_eq!(gs.sheet_name, "Master Jobs");
        assert_eq!(
            gs.credentials_path.as_deref(),
            Some(Path::new("/tmp/service-account.json"))
        );
        assert_eq!(
            gs.eligible_activity_types,
            vec![
                "Boris Job".to_string(),
                "Demo".to_string(),
                "Installation".to_string()
            ]
        );
    }

    #[test]
    fn test_google_sheets_config_empty_section_uses_defaults() {
        let config: Config = toml::from_str(
            r#"
[model]
provider = "test"
model_id = "x"
[providers.ollama]
base_url = "http://localhost:11434"
[google_sheets]
"#,
        )
        .unwrap();
        assert!(!config.google_sheets.enabled);
        assert_eq!(config.google_sheets.sheet_name, "US");
    }

    // ── Role-based loop tests ──────────────────────────────────────────────

    #[test]
    fn test_parse_agent_with_roles() {
        let markdown = r#"---
mode: primary
temperature: 0.1
roles:
  - name: orchestrator
    model: deepseek/v4
    instructions: "Plan and delegate."
  - name: coder
    model: deepseek/v3
---
# Build

Fallback instructions.
"#;
        let cfg = parse_agent_markdown(markdown).unwrap();
        assert_eq!(cfg.mode, "primary");
        assert_eq!(cfg.temperature, Some(0.1));
        assert!(cfg.instructions.is_some());
        assert!(
            cfg.instructions
                .as_ref()
                .unwrap()
                .contains("Fallback instructions")
        );
        assert!(cfg.roles.is_some());
        let roles = cfg.roles.as_ref().unwrap();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].name, "orchestrator");
        assert_eq!(roles[0].model.as_deref(), Some("deepseek/v4"));
        assert_eq!(
            roles[0].instructions.as_deref(),
            Some("Plan and delegate.")
        );
        assert_eq!(roles[1].name, "coder");
        assert_eq!(roles[1].model.as_deref(), Some("deepseek/v3"));
        assert!(roles[1].instructions.is_none());
        assert!(cfg.validate_roles().is_ok());
    }

    #[test]
    fn test_parse_agent_without_roles_is_legacy() {
        let markdown = r#"---
mode: primary
---
# Build

Just instructions.
"#;
        let cfg = parse_agent_markdown(markdown).unwrap();
        assert_eq!(cfg.mode, "primary");
        assert!(cfg.roles.is_none());
        assert!(cfg.validate_roles().is_ok());
    }

    #[test]
    fn test_agent_prompt_disable_flags_default_false() {
        let cfg = parse_agent_markdown("# Build\n\nInstructions.").unwrap();
        assert!(!cfg.disable_user_agents_md);
        assert!(!cfg.disble_system_prompt);
    }

    #[test]
    fn test_parse_agent_prompt_disable_flags() {
        let markdown = r#"---
disable_user_agents_md: true
disble_system_prompt: true
---
# Build
"#;
        let cfg = parse_agent_markdown(markdown).unwrap();
        assert!(cfg.disable_user_agents_md);
        assert!(cfg.disble_system_prompt);
    }

    #[test]
    fn test_disable_workspace_agents_md_default_false() {
        let cfg = parse_agent_markdown("# Build\n\nInstructions.").unwrap();
        assert!(!cfg.disable_workspace_agents_md);
    }

    #[test]
    fn test_disable_workspace_agents_md_explicit_true() {
        let markdown = r#"---
disable_workspace_agents_md: true
---
# Build
"#;
        let cfg = parse_agent_markdown(markdown).unwrap();
        assert!(cfg.disable_workspace_agents_md);
    }

    #[test]
    fn test_disable_workspace_agents_md_alias_repo() {
        let markdown = r#"---
disable_repo_agents_md: true
---
# Build
"#;
        let cfg = parse_agent_markdown(markdown).unwrap();
        assert!(cfg.disable_workspace_agents_md);
    }

    #[test]
    fn test_reminder_profile_default_none() {
        let cfg = parse_agent_markdown("# Build\n\nInstructions.").unwrap();
        assert_eq!(cfg.reminder_profile, None);
    }

    #[test]
    fn test_reminder_profile_explicit_pm() {
        let markdown = r#"---
reminder_profile: "pm"
---
# Build
"#;
        let cfg = parse_agent_markdown(markdown).unwrap();
        assert_eq!(cfg.reminder_profile.as_deref(), Some("pm"));
    }

    #[test]
    fn test_reminder_profile_arbitrary_value() {
        let markdown = r#"---
reminder_profile: "custom"
---
# Build
"#;
        let cfg = parse_agent_markdown(markdown).unwrap();
        assert_eq!(cfg.reminder_profile.as_deref(), Some("custom"));
    }

    #[test]
    fn test_parse_agent_prompt_disable_system_prompt_alias() {
        let markdown = r#"---
disable_system_prompt: true
---
# Build
"#;
        let cfg = parse_agent_markdown(markdown).unwrap();
        assert!(cfg.disble_system_prompt);
    }

    #[test]
    fn test_validate_roles_empty_rejected() {
        let cfg = AgentConfig {
            roles: Some(vec![]),
            ..Default::default()
        };
        assert!(cfg.validate_roles().is_err());
    }

    #[test]
    fn test_validate_roles_duplicate_name_rejected() {
        let cfg = AgentConfig {
            roles: Some(vec![
                RoleConfig {
                    name: "coder".into(),
                    instructions: None,
                    model: None,
                    temperature: None,
                    max_tokens: None,
                },
                RoleConfig {
                    name: "coder".into(),
                    instructions: None,
                    model: None,
                    temperature: None,
                    max_tokens: None,
                },
            ]),
            ..Default::default()
        };
        assert!(cfg.validate_roles().is_err());
    }

    #[test]
    fn test_role_instructions_fallback() {
        let agent = AgentConfig {
            instructions: Some("Agent default instructions".into()),
            roles: Some(vec![
                RoleConfig {
                    name: "orchestrator".into(),
                    instructions: Some("Orchestrator specific".into()),
                    model: None,
                    temperature: None,
                    max_tokens: None,
                },
                RoleConfig {
                    name: "coder".into(),
                    instructions: None,
                    model: None,
                    temperature: None,
                    max_tokens: None,
                },
            ]),
            ..Default::default()
        };
        let roles = agent.roles.as_ref().unwrap();
        // Orchestrator has its own instructions
        assert_eq!(
            agent.effective_role_instructions(&roles[0]),
            Some("Orchestrator specific".into())
        );
        // Coder falls back to agent instructions
        assert_eq!(
            agent.effective_role_instructions(&roles[1]),
            Some("Agent default instructions".into())
        );
    }

    #[test]
    fn test_effective_role_model_fallback() {
        let toml = r#"
[model]
provider = "deepseek"
model_id = "deepseek-chat"

[providers.ollama]
"#;
        let config: Config = toml::from_str(toml).unwrap();

        // Role model wins
        let (prov, model) =
            config.effective_role_model(Some("ollama/qwen"), None);
        assert_eq!(prov, "ollama");
        assert_eq!(model, "qwen");

        // Agent model fallback when role has no model
        let (prov, model) =
            config.effective_role_model(None, Some("glm/glm-4"));
        assert_eq!(prov, "glm");
        assert_eq!(model, "glm-4");

        // Config default when neither has model
        let (prov, model) = config.effective_role_model(None, None);
        assert_eq!(prov, "deepseek");
        assert_eq!(model, "deepseek-chat");

        // Model without provider prefix uses default provider
        let (prov, model) = config.effective_role_model(Some("gpt-4"), None);
        assert_eq!(prov, "deepseek");
        assert_eq!(model, "gpt-4");
    }
}
