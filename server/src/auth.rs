use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Auth credentials loaded from ~/.config/mote/auth.json
///
/// Format:
/// ```json5
/// {
///   "deepseek": { "api_key": "sk-..." },
///   "glm":      { "api_key": "..." },
///   // Providers without auth can be omitted or set to {}
///   "ollama": {}
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Auth {
    /// Per-provider auth credentials.
    /// Keys are provider names (lowercase, e.g. "deepseek", "glm", "ollama").
    #[serde(flatten)]
    pub providers: HashMap<String, ProviderAuth>,
}

/// Auth credentials for a single provider.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderAuth {
    /// API key (used by DeepSeek, OpenAI, etc.)
    pub api_key: Option<String>,
    /// Bearer token (kept for custom providers or future integrations)
    pub token: Option<String>,
    /// Generic extra fields for provider-specific auth needs.
    #[serde(flatten)]
    pub extra: HashMap<String, String>,
}

impl ProviderAuth {
    /// Return the api_key if present, or None.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// Return the token if present, or None.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

impl Auth {
    /// Load auth from `~/.config/mote/auth.json`.
    /// Returns an empty Auth (no error) if the file doesn't exist or is invalid.
    pub fn load() -> Self {
        let path = auth_path();
        if !path.exists() {
            tracing::debug!("No auth.json found at {}", path.display());
            return Self::default();
        }
        match Self::load_from(&path) {
            Ok(auth) => auth,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse auth.json at {}: {:#}",
                    path.display(),
                    e
                );
                Self::default()
            }
        }
    }

    fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let auth: Auth = match json5::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(primary_err) => {
                let repaired = repair_missing_commas_between_fields(&raw);
                json5::from_str(&repaired).with_context(|| {
                    format!(
                        "Failed to parse JSON5 in {}: {primary_err}",
                        path.display()
                    )
                })?
            }
        };
        Ok(auth)
    }

    /// Get the ProviderAuth for a given provider name.
    pub fn for_provider(&self, provider: &str) -> Option<&ProviderAuth> {
        self.providers.get(provider)
    }

    /// Get the API key for a provider, or None.
    pub fn api_key(&self, provider: &str) -> Option<&str> {
        self.for_provider(provider)?.api_key()
    }

    /// Get the token for a provider, or None.
    pub fn token(&self, provider: &str) -> Option<&str> {
        self.for_provider(provider)?.token()
    }
}

/// Best-effort repair for a common editing mistake in auth.json:
/// missing trailing commas between object fields.
///
/// Example repaired:
///   "api_key": "..."
///   "token": null
/// becomes:
///   "api_key": "...",
///   "token": null
fn repair_missing_commas_between_fields(input: &str) -> String {
    let lines: Vec<&str> = input.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let mut updated = (*line).to_string();

        // Only consider likely "key: value" lines that are missing a comma.
        let is_candidate = trimmed.contains(':')
            && !trimmed.ends_with(',')
            && !trimmed.ends_with('{')
            && !trimmed.ends_with('[')
            && !trimmed.ends_with('}')
            && !trimmed.ends_with(']');

        if is_candidate {
            let next_significant = lines
                .iter()
                .skip(idx + 1)
                .map(|l| l.trim())
                .find(|t| !t.is_empty() && !t.starts_with("//"));

            // If the next significant line starts another field,
            // this line should end with a comma.
            if next_significant.is_some_and(|next| next.starts_with('"')) {
                updated.push(',');
            }
        }

        out.push(updated);
    }

    out.join("\n")
}

/// Get the path to the auth file.
pub fn auth_path() -> std::path::PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".config").join("mote").join("auth.json")
    } else {
        std::path::PathBuf::from("auth.json")
    }
}

/// Save or update a credential (api_key, token, etc.) in auth.json.
/// Creates the file and parent directory if they don't exist.
pub fn save_credential(provider: &str, field: &str, value: &str) -> Result<()> {
    let path = auth_path();
    let mut auth = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        // Parse as JSON5 (supports comments), fall back to plain JSON
        json5::from_str(&raw)
            .unwrap_or_else(|_| serde_json::from_str(&raw).unwrap_or_default())
    } else {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create ~/.config/mote")?;
        }
        Auth::default()
    };

    let entry = auth
        .providers
        .entry(provider.to_string())
        .or_insert_with(ProviderAuth::default);

    match field {
        "api_key" => entry.api_key = Some(value.to_string()),
        "token" => entry.token = Some(value.to_string()),
        _ => {
            anyhow::bail!("Unknown credential field '{}'", field);
        }
    }

    let json = serde_json::to_string_pretty(&auth)
        .context("Failed to serialize auth.json")?;
    std::fs::write(&path, json)
        .with_context(|| format!("Failed to write {}", path.display()))?;

    tracing::info!(
        "Saved credential '{field}' for provider '{provider}' to {}",
        path.display()
    );

    // Restrict permissions to owner-only on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(&path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_default_is_empty() {
        let auth = Auth::default();
        assert!(auth.providers.is_empty());
        assert!(auth.api_key("deepseek").is_none());
        assert!(auth.token("custom").is_none());
    }

    #[test]
    fn test_parse_json5_with_comments() {
        let json5_str = r#"
        {
            // DeepSeek API key
            "deepseek": { "api_key": "sk-test-key" },
            "custom": { "token": "test_token" },
            "ollama": {}
        }
        "#;
        let auth: Auth = json5::from_str(json5_str).unwrap();
        assert_eq!(auth.api_key("deepseek").unwrap(), "sk-test-key");
        assert_eq!(auth.token("custom").unwrap(), "test_token");
        assert!(auth.for_provider("ollama").is_some());
        // Non-existent provider
        assert!(auth.for_provider("nonexistent").is_none());
    }

    #[test]
    fn test_parse_json5_recovers_missing_field_comma() {
        let broken = r#"
        {
          "glm": {
            "api_key": "abc123"
            "token": null
          }
        }
        "#;

        let repaired = repair_missing_commas_between_fields(broken);
        let auth: Auth = json5::from_str(&repaired).unwrap();
        assert_eq!(auth.api_key("glm"), Some("abc123"));
    }

    #[test]
    fn test_provider_auth_api_key_and_token() {
        let pa = ProviderAuth {
            api_key: Some("sk-key".into()),
            token: Some("ghp-token".into()),
            extra: HashMap::new(),
        };
        assert_eq!(pa.api_key(), Some("sk-key"));
        assert_eq!(pa.token(), Some("ghp-token"));
    }

    #[test]
    fn test_provider_auth_empty() {
        let pa = ProviderAuth::default();
        assert!(pa.api_key().is_none());
        assert!(pa.token().is_none());
    }

    #[test]
    fn test_auth_api_key_via_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "deepseek".into(),
            ProviderAuth {
                api_key: Some("sk-123".into()),
                token: None,
                extra: HashMap::new(),
            },
        );
        let auth = Auth { providers };
        assert_eq!(auth.api_key("deepseek"), Some("sk-123"));
        assert!(auth.api_key("ollama").is_none());
    }

    #[test]
    fn test_load_from_nonexistent_file_returns_empty() {
        // load() won't find our test file since it looks in ~/.config/mote/
        // But load_from should fail with an error
        let result =
            Auth::load_from(Path::new("/tmp/__nonexistent_auth_file_xyz__"));
        assert!(result.is_err());
    }

    #[test]
    fn test_json5_with_extra_field() {
        let json5_str =
            r#"{"custom_provider": { "api_key": "k", "extra_field": "v" }}"#;
        let auth: Auth = json5::from_str(json5_str).unwrap();
        let pa = auth.for_provider("custom_provider").unwrap();
        assert_eq!(pa.api_key(), Some("k"));
        assert_eq!(pa.extra.get("extra_field").unwrap(), "v");
    }

    #[test]
    fn test_save_token_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_auth.json");

        // Write with save_token_to_auth at a custom path... we can't easily
        // since save_token_to_auth always writes to ~/.config/mote/auth.json.
        // Instead test the underlying serialization directly.
        let mut auth = Auth::default();
        auth.providers
            .entry("custom".into())
            .or_insert_with(ProviderAuth::default)
            .token = Some("ghp_test_token".into());

        let json = serde_json::to_string_pretty(&auth).unwrap();
        std::fs::write(&path, &json).unwrap();

        let read: Auth =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
                .unwrap();
        assert_eq!(read.token("custom").unwrap(), "ghp_test_token");
        assert!(read.api_key("custom").is_none());
    }

    #[test]
    fn test_save_token_adds_to_empty_auth() {
        let _dir = tempfile::tempdir().unwrap();
        let mut auth = Auth::default();
        assert!(auth.token("custom").is_none());
        auth.providers
            .entry("custom".into())
            .or_insert_with(ProviderAuth::default)
            .token = Some("ghp_new".into());
        assert_eq!(auth.token("custom").unwrap(), "ghp_new");
    }

    #[test]
    fn test_save_credential_roundtrip_via_tempfile() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test_auth.json");

        // Simulate save_credential for api_key (like DeepSeek)
        let mut auth = Auth::default();
        auth.providers
            .entry("deepseek".into())
            .or_insert_with(ProviderAuth::default)
            .api_key = Some("sk-test".into());
        let json = serde_json::to_string_pretty(&auth).unwrap();
        std::fs::write(&path, &json).unwrap();

        // Read back and verify
        let read: Auth =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
                .unwrap();
        assert_eq!(read.api_key("deepseek").unwrap(), "sk-test");
        assert!(read.token("deepseek").is_none());
    }

    #[test]
    fn test_save_credential_roundtrip_token_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test_auth_token.json");

        // Simulate save_credential for token fields.
        let mut auth = Auth::default();
        auth.providers
            .entry("custom".into())
            .or_insert_with(ProviderAuth::default)
            .token = Some("ghp-test".into());
        let json = serde_json::to_string_pretty(&auth).unwrap();
        std::fs::write(&path, &json).unwrap();

        // Read back and verify
        let read: Auth =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
                .unwrap();
        assert_eq!(read.token("custom").unwrap(), "ghp-test");
        assert!(read.api_key("custom").is_none());
    }

    #[test]
    fn test_credential_fields_are_independent() {
        // api_key and token should not interfere with each other
        let json = r#"{"deepseek": { "api_key": "sk-a", "token": "tok-b" }}"#;
        let auth: Auth = serde_json::from_str(json).unwrap();
        let ds = auth.for_provider("deepseek").unwrap();
        assert_eq!(ds.api_key(), Some("sk-a"));
        assert_eq!(ds.token(), Some("tok-b"));
    }
}
