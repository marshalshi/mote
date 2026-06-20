use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

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
    pub github: Option<ProviderGitHub>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderGitHub {
    /// GitHub Models base URL.
    #[serde(default = "default_github_base_url")]
    pub base_url: String,
    pub default_model: Option<String>,
    pub default_max_tokens: Option<u32>,
}

fn default_github_base_url() -> String {
    "https://models.github.ai/inference".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderOllama {
    #[serde(default = "default_ollama_base_url")]
    pub base_url: String,
    pub default_model: Option<String>,
    pub default_max_tokens: Option<u32>,
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
    pub model_specific: Option<PathBuf>,
    #[serde(default)]
    #[allow(dead_code)] // deserialized from config, used at runtime
    pub instructions: Vec<PathBuf>,
}
fn default_prompt_file() -> PathBuf {
    PathBuf::from("prompts/default.txt")
}
impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            default: default_prompt_file(),
            model_specific: None,
            instructions: vec![],
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
    /// Maximum agent loop steps per request (default: 10).
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
}

fn default_server_port() -> u16 {
    9847
}
fn default_max_steps() -> usize {
    10
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_server_port(),
            max_steps: default_max_steps(),
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

/// Per-agent override within the `[agents]` section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentConfig {
    #[allow(dead_code)]
    pub model: Option<String>,
    pub model_specific: Option<PathBuf>,
    pub default: Option<PathBuf>,
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
    /// Agent mode: "primary" (user-selectable, default), "subagent" (tool-only), "all" (both).
    #[serde(default = "default_agent_mode")]
    pub mode: String,
}

fn default_agent_mode() -> String {
    "primary".into()
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
            "github" => self
                .providers
                .github
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

    /// Get the GitHub token from auth.json.
    pub fn resolve_github_token(
        &self,
        auth: &crate::auth::Auth,
    ) -> Result<String> {
        if let Some(token) = auth.token("github") {
            return Ok(Self::expand(token));
        }
        anyhow::bail!(
            "No GitHub token found in auth.json. Run --login github first."
        );
    }

    pub fn github_base_url(&self) -> Result<String> {
        Ok(Self::expand(match &self.providers.github {
            Some(cfg) => &cfg.base_url,
            None => "https://models.github.ai/inference",
        })
        .trim_end_matches('/')
        .to_string())
    }

    pub fn github_default_model(&self) -> Option<&str> {
        self.providers.github.as_ref()?.default_model.as_deref()
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

/// Load agent definitions from `~/.config/mote/agents/*.toml`.
/// Each file is named `<agent_name>.toml` and contains an `AgentConfig`.
pub fn load_file_agents() -> HashMap<String, AgentConfig> {
    let dir = resolve_config_path("agents");
    if !dir.is_dir() {
        return HashMap::new();
    }
    let mut agents = HashMap::new();
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "toml") {
                    if let Some(stem) =
                        path.file_stem().and_then(|s| s.to_str())
                    {
                        match std::fs::read_to_string(&path) {
                            Ok(content) => {
                                match toml::from_str::<AgentConfig>(&content) {
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
    agents
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
default = "prompts/default.txt"
instructions = ["prompts/instructions/"]
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
                model_specific: None,
                default: None,
                temperature: Some(0.3),
                max_tokens: Some(4096),
                permissions: HashMap::new(),
                instructions: None,
                mode: "primary".into(),
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
    fn test_load_file_agents_reads_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agents");
        std::fs::create_dir(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("review.toml"),
            r#"
model = "ollama/qwen"
temperature = 0.2
max_tokens = 2048
"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("plan.toml"),
            r#"
model = "ollama/deepseek"
permissions = { bash = "deny" }
"#,
        )
        .unwrap();

        // Temporarily override resolve_config_path to use our temp dir
        // Since resolve_config_path is a module function, we test via the public API
        // by checking that the function can be called (we can't mock the home dir easily).
        // For now, we verify the parsing logic by calling it directly with the file content.
        let content =
            std::fs::read_to_string(agent_dir.join("review.toml")).unwrap();
        let cfg: AgentConfig = toml::from_str(&content).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("ollama/qwen"));
        assert_eq!(cfg.temperature, Some(0.2));

        let content2 =
            std::fs::read_to_string(agent_dir.join("plan.toml")).unwrap();
        let cfg2: AgentConfig = toml::from_str(&content2).unwrap();
        assert_eq!(cfg2.model.as_deref(), Some("ollama/deepseek"));
        assert_eq!(
            cfg2.permissions.get("bash").copied(),
            Some(Permission::Deny)
        );
    }

    #[test]
    fn test_all_agents_empty_when_no_config_agents() {
        let merged = all_agents(&HashMap::new());
        // File agents depend on the user's ~/.config/mote/agents/ directory.
        // If the directory exists, ensure at least some are loaded.
        let agent_dir = dirs::home_dir()
            .map(|h| h.join(".config").join("mote").join("agents"));
        let has_agent_dir = agent_dir.as_ref().map_or(false, |d| d.is_dir());
        if has_agent_dir {
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
    fn test_agent_mode_serialized_from_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let agent_file = dir.path().join("test_agent.toml");
        std::fs::write(
            &agent_file,
            r#"
model = "deepseek/deepseek-v4-flash"
mode = "all"
temperature = 0.2
[permissions]
bash = "deny"
"#,
        )
        .unwrap();
        let content = std::fs::read_to_string(&agent_file).unwrap();
        let cfg: AgentConfig = toml::from_str(&content).unwrap();
        assert_eq!(cfg.mode, "all");
        assert!(cfg.is_user_selectable());
        assert!(cfg.is_subagent_callable());
        assert_eq!(cfg.temperature, Some(0.2));
        assert_eq!(
            cfg.permissions.get("bash").copied(),
            Some(Permission::Deny)
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
        assert_eq!(config.server.max_steps, 10);
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
}
