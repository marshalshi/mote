use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WorkspaceContext {
    pub root: PathBuf,
    pub repo_agents_md: Option<String>,
    pub runtime_session_key: String,
}

pub fn resolve_workspace_context(
    session_key_override: Option<&str>,
) -> Result<WorkspaceContext> {
    let root = std::env::current_dir().context("Failed to resolve CWD")?;
    let repo_agents_md = read_repo_agents_md(&root)?;
    let runtime_session_key = if let Some(k) = session_key_override {
        validate_session_key(k)?;
        k.trim().to_string()
    } else {
        load_or_create_persistent_client_key()?
    };
    Ok(WorkspaceContext {
        root,
        repo_agents_md,
        runtime_session_key,
    })
}

fn read_repo_agents_md(root: &std::path::Path) -> Result<Option<String>> {
    let path = root.join("AGENTS.md");
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

fn validate_session_key(key: &str) -> Result<()> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Session key cannot be empty");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ':')
    {
        anyhow::bail!(
            "Invalid session key: only letters, numbers, '-', '_' and ':' are allowed"
        );
    }
    Ok(())
}

fn load_or_create_persistent_client_key() -> Result<String> {
    let base = dirs::home_dir()
        .map(|h| h.join(".config").join("mote"))
        .context("Cannot resolve home directory for persistent session key")?;
    std::fs::create_dir_all(&base)
        .with_context(|| format!("Failed to create {}", base.display()))?;
    let path = base.join("client_id");
    if path.exists() {
        let key = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        validate_session_key(&key)?;
        return Ok(key.trim().to_string());
    }
    let key = format!(
        "client-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    std::fs::write(&path, &key)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_repo_agents_md_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let got = read_repo_agents_md(dir.path()).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn test_read_repo_agents_md_reads_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("AGENTS.md");
        std::fs::write(&p, "  hello\n").unwrap();
        let got = read_repo_agents_md(dir.path()).unwrap();
        assert_eq!(got.as_deref(), Some("hello"));
    }

    #[test]
    fn test_validate_session_key() {
        assert!(validate_session_key("client-1:abc").is_ok());
        assert!(validate_session_key("bad key").is_err());
        assert!(validate_session_key("../bad").is_err());
    }
}
