pub use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::llm::{ChatMessage, Role, Usage};

/// A single entry in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

impl Message {
    pub fn new(role: Role, content: String) -> Self {
        Self {
            role,
            content,
            timestamp: Utc::now(),
        }
    }
}

/// Frontmatter metadata written at the top of each session file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub model_provider: String,
    pub model_id: String,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub version: String,
    /// Short summary of the conversation (first user message or auto-generated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// In-memory conversation session.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub created: DateTime<Utc>,
    pub messages: Vec<Message>,
    pub model_provider: String,
    pub model_id: String,
    pub tokens_input: u64,
    pub tokens_output: u64,
    /// Short summary of the conversation.
    pub summary: Option<String>,
}

impl Session {
    /// Create a new session with a timestamp-based ID.
    #[allow(dead_code)] // public API, used in tests and handle_socket
    pub fn new(model_provider: String, model_id: String) -> Self {
        let now = Utc::now();
        let id = format!("chat-{}", now.format("%Y%m%d-%H%M%S%6f"));
        Self {
            id,
            created: now,
            messages: vec![],
            model_provider,
            model_id,
            tokens_input: 0,
            tokens_output: 0,
            summary: None,
        }
    }

    /// Rebuild a session from existing metadata and messages (used for --resume).
    #[allow(dead_code)] // public API, used in tests and handle_socket
    pub fn from_meta(meta: SessionMeta, messages: Vec<Message>) -> Self {
        Self {
            tokens_input: meta.tokens_input,
            tokens_output: meta.tokens_output,
            id: meta.id,
            created: meta.created,
            model_provider: meta.model_provider,
            model_id: meta.model_id,
            messages,
            summary: meta.summary,
        }
    }

    /// Add a user message.
    #[allow(dead_code)] // public API, used in tests and handle_socket
    pub fn add_user(&mut self, content: String) {
        self.messages.push(Message::new(Role::User, content));
    }

    /// Add an assistant message and track token usage.
    #[allow(dead_code)]
    pub fn add_assistant(&mut self, content: String, usage: &Usage) {
        self.messages.push(Message::new(Role::Assistant, content));
        self.tokens_input += usage.prompt_tokens;
        self.tokens_output += usage.completion_tokens;
    }

    /// Build the list of LLM API messages (no system — those are handled externally).
    #[allow(dead_code)] // public API, used in tests and handle_socket
    pub fn chat_messages(&self) -> Vec<ChatMessage> {
        self.messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: Some(m.content.clone()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            })
            .collect()
    }

    /// Create a session from the agent's internal conversation history.
    /// Converts `Vec<ChatMessage>` (LLM format) into `Vec<Message>` (session format).
    pub fn from_chat_history(
        model_id: &str,
        model_provider: &str,
        _agent_name: &str,
        tokens_input: u64,
        tokens_output: u64,
        chat_history: &[ChatMessage],
    ) -> Self {
        use crate::llm::Role as ChatRole;
        let now = Utc::now();
        let id = format!("chat-{}", now.format("%Y%m%d-%H%M%S%6f"));
        let mut messages = Vec::new();
        for msg in chat_history {
            let role = match msg.role {
                ChatRole::User => {
                    // Tool results come as User messages with tool_call_id
                    if msg.tool_call_id.is_some() {
                        // Include tool results as user messages (OpenAI convention)
                        Some(Role::User)
                    } else {
                        Some(Role::User)
                    }
                }
                ChatRole::Assistant => {
                    if msg.content.is_some() {
                        Some(Role::Assistant)
                    } else {
                        None
                    }
                }
                _ => None, // skip System/tool messages and internal tool-call placeholders
            };
            if let Some(role) = role {
                let content = msg.content.clone().unwrap_or_default();
                if !content.is_empty() {
                    messages.push(Message::new(role, content));
                }
            }
        }
        // Generate summary from the first user message (5-10 words style)
        let summary = chat_history.iter().find_map(|msg| {
            if matches!(msg.role, crate::llm::Role::User) {
                msg.content.as_ref().map(|c| {
                    let trimmed: String = c.trim().replace('\n', " ");
                    let words: Vec<&str> = trimmed
                        .split_whitespace()
                        .filter(|w| !w.is_empty())
                        .collect();
                    let take_n = 8usize;
                    if words.len() > take_n {
                        format!("{}...", words[..take_n].join(" "))
                    } else {
                        words.join(" ")
                    }
                })
            } else {
                None
            }
        });

        Self {
            id,
            created: now,
            messages,
            model_provider: model_provider.to_string(),
            model_id: model_id.to_string(),
            tokens_input,
            tokens_output,
            summary,
        }
    }

    /// Build session metadata for the history file.
    pub fn meta(&self) -> SessionMeta {
        SessionMeta {
            id: self.id.clone(),
            created: self.created,
            updated: Utc::now(),
            model_provider: self.model_provider.clone(),
            model_id: self.model_id.clone(),
            tokens_input: self.tokens_input,
            tokens_output: self.tokens_output,
            version: env!("CARGO_PKG_VERSION").to_string(),
            summary: self.summary.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_session() {
        let s = Session::new("ollama".into(), "deepseek-r1:8b".into());
        assert!(s.id.starts_with("chat-"));
        assert_eq!(s.model_provider, "ollama");
        assert!(s.messages.is_empty());
    }

    #[test]
    fn test_add_messages() {
        let mut s = Session::new("deepseek".into(), "deepseek-chat".into());
        s.add_user("Hello".into());
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].content, "Hello");

        s.add_assistant(
            "Hi!".into(),
            &Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
        );
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.tokens_input, 10);
        assert_eq!(s.tokens_output, 5);
    }

    #[test]
    fn test_chat_messages() {
        let mut s = Session::new("test".into(), "test-model".into());
        s.add_user("Hello".into());
        let chat = s.chat_messages();
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, Role::User);
        assert_eq!(chat[0].content, Some("Hello".to_string()));
    }

    #[test]
    fn test_meta_roundtrip() {
        let s = Session::new("ollama".into(), "r1".into());
        let meta = s.meta();
        assert_eq!(meta.model_provider, "ollama");
        assert_eq!(meta.model_id, "r1");
        assert_eq!(meta.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn test_from_meta_restores_state() {
        let meta = SessionMeta {
            id: "chat-test".into(),
            created: Utc::now(),
            updated: Utc::now(),
            model_provider: "ollama".into(),
            model_id: "r1".into(),
            tokens_input: 100,
            tokens_output: 50,
            version: "0.1.0".into(),
            summary: None,
        };
        let msgs = vec![Message::new(Role::User, "Hello".into())];
        let s = Session::from_meta(meta, msgs);
        assert_eq!(s.id, "chat-test");
        assert_eq!(s.tokens_input, 100);
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn test_from_chat_history_skips_internal_tool_calls_and_results() {
        let tool_call = crate::llm::ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: crate::llm::ToolFunction {
                name: "read".into(),
                arguments: "{}".into(),
            },
        };
        let history = vec![
            ChatMessage::user("please read"),
            ChatMessage::assistant_tool_calls(vec![tool_call]),
            ChatMessage::tool_result("call_1", "file contents"),
            ChatMessage::assistant_text("done"),
        ];

        let session = Session::from_chat_history(
            "model", "provider", "default", 1, 2, &history,
        );

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "please read");
        assert_eq!(session.messages[1].content, "done");
    }
}
