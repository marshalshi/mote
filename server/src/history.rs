use crate::llm::Role;
use crate::session::{Message, SessionMeta};
use anyhow::{Context, Result};
use std::path::Path;

/// Parse a session file (markdown + YAML frontmatter) into its metadata and messages.
pub fn parse_file(path: &Path) -> Result<(SessionMeta, Vec<Message>)> {
    let content = std::fs::read_to_string(path).with_context(|| {
        format!("Failed to read session file: {}", path.display())
    })?;
    parse(&content)
}

/// Parse a session file's content string.
pub fn parse(content: &str) -> Result<(SessionMeta, Vec<Message>)> {
    // Split on the first `---` to isolate YAML frontmatter.
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .context("Session file must start with `---`")?;

    let (yaml_text, body) = rest
        .split_once("\n---")
        .or_else(|| rest.split_once("\r\n---"))
        .context("Session file missing closing `---` after frontmatter")?;

    let meta: SessionMeta = serde_yaml::from_str(yaml_text)
        .context("Failed to parse YAML frontmatter")?;

    let messages = parse_body(body);

    Ok((meta, messages))
}

/// Parse the markdown body into messages.
///
/// Expected format:
///   ## User — HH:MM:SS
///   <content>
///
///   ## Assistant — HH:MM:SS
///   <content>
fn parse_body(body: &str) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut current_role: Option<Role> = None;
    let mut current_content = String::new();

    for line in body.lines() {
        if let Some(rest) = line
            .strip_prefix("## ")
            .and_then(|r| r.split_once(" — "))
            .or_else(|| {
                line.strip_prefix("## ").and_then(|r| r.split_once(" — "))
            })
        {
            let role_str = rest.0.trim();
            // Flush previous message
            if let Some(role) = current_role.take() {
                let content =
                    std::mem::take(&mut current_content).trim().to_string();
                if !content.is_empty() {
                    messages.push(Message::new(role, content));
                }
            }
            current_role = match role_str {
                "User" => Some(Role::User),
                "Assistant" => Some(Role::Assistant),
                _ => None,
            };
        } else if current_role.is_some() {
            if !current_content.is_empty() || !line.trim().is_empty() {
                if !current_content.is_empty() {
                    current_content.push('\n');
                }
                current_content.push_str(line);
            }
        }
    }

    // Flush last message
    if let Some(role) = current_role {
        let content = current_content.trim().to_string();
        if !content.is_empty() {
            messages.push(Message::new(role, content));
        }
    }

    messages
}

/// Serialize a session to a markdown string with YAML frontmatter.
pub fn serialize(meta: &SessionMeta, messages: &[Message]) -> Result<String> {
    let yaml = serde_yaml::to_string(meta)
        .context("Failed to serialize session metadata")?;

    let mut body = String::new();
    for msg in messages {
        let time = msg.timestamp.format("%H:%M:%S");
        let role_heading = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            _ => continue,
        };
        body.push_str(&format!(
            "## {} — {}\n{}\n\n",
            role_heading, time, msg.content
        ));
    }

    Ok(format!("---\n{}---\n\n{}", yaml, body.trim()))
}

/// Save a session to disk. Creates `{hist_dir}/{id}.md`.
pub fn save_session(
    hist_dir: &Path,
    session: &crate::session::Session,
) -> Result<()> {
    let meta = session.meta();
    let content = serialize(&meta, &session.messages)?;
    std::fs::create_dir_all(hist_dir)?;
    let path = hist_dir.join(format!("{}.md", session.id));
    std::fs::write(&path, content)?;
    tracing::info!("Session saved: {}", path.display());
    Ok(())
}

/// List sessions from a history directory, returning metadata for each.
/// Entries are sorted by modification time, newest first.
pub fn list_sessions(
    hist_dir: &Path,
) -> Result<Vec<(SessionMeta, std::path::PathBuf)>> {
    if !hist_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<_> = std::fs::read_dir(hist_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "md"))
        .collect();
    entries.sort_by_key(|e| {
        e.path().metadata().ok().and_then(|m| m.modified().ok())
    });

    let mut sessions = Vec::new();
    for entry in entries {
        let path = entry.path();
        if let Ok((meta, _msgs)) = parse_file(&path) {
            sessions.push((meta, path));
        }
    }
    sessions.reverse(); // newest first
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Role;

    fn sample_content() -> &'static str {
        "---\n\
         id: chat-20260525-233658\n\
         created: 2026-05-25T23:36:58.084098Z\n\
         updated: 2026-05-25T23:36:58.084098Z\n\
         model_provider: ollama\n\
         model_id: deepseek-r1:8b\n\
         tokens_input: 10\n\
         tokens_output: 5\n\
         version: 0.1.0\n\
         ---\n\
         \n\
         ## User — 23:36:58\n\
         What is 99-1?\n\
         \n\
         ## Assistant — 23:36:59\n\
         98\n"
    }

    #[test]
    fn test_parse_roundtrip() {
        let content = sample_content();
        let (meta, messages) = parse(content).unwrap();
        assert_eq!(meta.id, "chat-20260525-233658");
        assert_eq!(meta.model_provider, "ollama");
        assert_eq!(meta.tokens_input, 10);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].content, "What is 99-1?");
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].content, "98");

        let serialized = serialize(&meta, &messages).unwrap();
        let (meta2, messages2) = parse(&serialized).unwrap();
        assert_eq!(meta2.id, meta.id);
        assert_eq!(meta2.tokens_input, meta.tokens_input);
        assert_eq!(messages2.len(), messages.len());
        assert_eq!(messages2[0].content, messages[0].content);
    }

    #[test]
    fn test_parse_missing_frontmatter() {
        assert!(parse("no frontmatter here").is_err());
    }

    #[test]
    fn test_parse_empty_body() {
        let content = "---\n\
                       id: test\n\
                       created: 2026-05-25T00:00:00Z\n\
                       updated: 2026-05-25T00:00:00Z\n\
                       model_provider: test\n\
                       model_id: test\n\
                       tokens_input: 0\n\
                       tokens_output: 0\n\
                       version: 0.1.0\n\
                       ---\n";
        let (meta, messages) = parse(content).unwrap();
        assert_eq!(meta.id, "test");
        assert!(messages.is_empty());
    }

    #[test]
    fn test_parse_multi_line_content() {
        let content = "---\n\
                       id: m\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n\
                       model_provider: t\nmodel_id: t\n\
                       tokens_input: 0\ntokens_output: 0\nversion: 0.1.0\n\
                       ---\n\
                       \n\
                       ## User — 00:00:00\n\
                       Line 1\n\
                       Line 2\n\
                       \n\
                       ## Assistant — 00:00:01\n\
                       Response\n";
        let (_, msgs) = parse(content).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "Line 1\nLine 2");
        assert_eq!(msgs[1].content, "Response");
    }

    #[test]
    fn test_parse_windows_line_endings() {
        let content = "---\r\n\
                       id: w\r\ncreated: 2026-01-01T00:00:00Z\r\nupdated: 2026-01-01T00:00:00Z\r\n\
                       model_provider: t\r\nmodel_id: t\r\n\
                       tokens_input: 0\r\ntokens_output: 0\r\nversion: 0.1.0\r\n\
                       ---\r\n\
                       \r\n\
                       ## User — 00:00:00\r\n\
                       Hello\r\n";
        let (_, msgs) = parse(content).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Hello");
    }

    #[test]
    fn test_parse_leading_trailing_whitespace() {
        let content = "---\n\
                       id: w\ncreated: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n\
                       model_provider: t\nmodel_id: t\n\
                       tokens_input: 0\ntokens_output: 0\nversion: 0.1.0\n\
                       ---\n\
                       \n  \n\
                       ## User — 00:00:00\n\
                       \n  \n  Hello  \n  \n\
                       \n  \n\
                       ## Assistant — 00:00:01\n\
                       World\n";
        let (_, msgs) = parse(content).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content.trim(), "Hello");
        assert_eq!(msgs[1].content, "World");
    }

    #[test]
    fn test_serialize_preserves_content() {
        let msgs = vec![
            Message::new(Role::User, "Multi\nline\ninput".into()),
            Message::new(
                Role::Assistant,
                "Code:\n```rust\nfn main() {}\n```".into(),
            ),
        ];
        let meta = SessionMeta {
            id: "test".into(),
            created: chrono::Utc::now(),
            updated: chrono::Utc::now(),
            model_provider: "ollama".into(),
            model_id: "r1".into(),
            tokens_input: 50,
            tokens_output: 30,
            version: "0.1.0".into(),
            summary: None,
        };
        let out = serialize(&meta, &msgs).unwrap();
        let (meta2, msgs2) = parse(&out).unwrap();
        assert_eq!(meta2.id, "test");
        assert_eq!(msgs2.len(), 2);
        assert_eq!(msgs2[0].content, "Multi\nline\ninput");
        assert!(msgs2[1].content.contains("fn main()"));
    }
}
