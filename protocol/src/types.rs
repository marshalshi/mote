use serde::{Deserialize, Serialize};

/// Protocol version. Bump when breaking changes are made to ServerEvent/ChatRequest.
pub const PROTOCOL_VERSION: &str = "1";

// ── Client → Server ─────────────────────────────────────

/// A single message from the conversation history (sent by the client).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub role: String, // "user" or "assistant"
    pub content: String,
}

/// Initial message the client sends over WebSocket to start a chat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub message: String,
    pub agent: String,
    pub model_override: Option<String>,
    /// Provider override (when model_override is just the model name without provider/ prefix).
    #[serde(default)]
    pub provider_override: Option<String>,
    /// Previous conversation turns so the LLM has context.
    #[serde(default)]
    pub history: Vec<HistoryMessage>,
    /// Resume an existing session (loaded from server).
    #[serde(default)]
    pub session_id: Option<String>,
}

// ── Client ↔ Server (bi-directional events during a session) ──

/// Events sent from client to server during an active chat session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientEvent {
    #[serde(rename = "permission_response")]
    PermissionResponse { id: String, allowed: bool },
    /// Cancel the currently running agent loop.
    #[serde(rename = "cancel")]
    Cancel,
    /// Request rollback of the most recent tracked change-set.
    #[serde(rename = "rollback_last")]
    RollbackLast,
}

// ── Server → Client (streaming events) ──────────────────

/// Events sent from server to client during a streaming chat.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerEvent {
    #[serde(rename = "text_delta")]
    TextDelta { data: String },
    #[serde(rename = "reasoning_delta")]
    ReasoningDelta { data: String },
    #[serde(rename = "tool_started")]
    ToolStarted { id: String, name: String },
    #[serde(rename = "tool_completed")]
    ToolCompleted {
        id: String,
        result: String,
        #[serde(default)]
        changes: Vec<FileChange>,
    },
    #[serde(rename = "tool_failed")]
    ToolFailed { id: String, error: String },
    #[serde(rename = "turn_done")]
    TurnDone {
        text: String,
        tool_calls: Vec<ToolCallDisplay>,
    },
    #[serde(rename = "permission_request")]
    PermissionRequest {
        id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    #[serde(rename = "skills_loaded")]
    SkillsLoaded { names: Vec<String> },
    #[serde(rename = "skill_selected")]
    SkillSelected { name: String },
    #[serde(rename = "sub_started")]
    SubagentStarted { id: String, name: String },
    #[serde(rename = "sub_text_delta")]
    SubagentTextDelta { id: String, data: String },
    #[serde(rename = "sub_reasoning_delta")]
    SubagentReasoningDelta { id: String, data: String },
    #[serde(rename = "sub_tool_started")]
    SubagentToolStarted {
        id: String,
        sub_id: String,
        tool_name: String,
    },
    #[serde(rename = "sub_tool_completed")]
    SubagentToolCompleted {
        id: String,
        sub_id: String,
        result: String,
        #[serde(default)]
        changes: Vec<FileChange>,
    },
    #[serde(rename = "sub_tool_failed")]
    SubagentToolFailed {
        id: String,
        sub_id: String,
        error: String,
    },
    #[serde(rename = "sub_done")]
    SubagentDone { id: String, content: String },
    #[serde(rename = "done")]
    Done {
        content: String,
        tokens_input: u64,
        tokens_output: u64,
    },
    #[serde(rename = "error")]
    Error { message: String },
    /// Result of a user-triggered rollback operation.
    #[serde(rename = "rollback_result")]
    RollbackResult {
        success: bool,
        message: String,
        #[serde(default)]
        changes: Vec<FileChange>,
    },
    /// Catch-all for unknown event types (backwards compatibility).
    #[serde(other)]
    Unknown,
}

// ── Shared display types ─────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallDisplay {
    pub id: String,
    pub name: String,
    pub status: ToolStatus,
    /// Structured file changes for this tool call (if any).
    #[serde(default)]
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolStatus {
    Running,
    Success,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileChangeKind {
    Modified,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLineKind {
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    /// Unified-diff-like lines for modified files.
    #[serde(default)]
    pub diff_lines: Vec<DiffLine>,
    /// True if server truncated diff output for readability.
    #[serde(default)]
    pub truncated: bool,
}

// ── Server → Client (REST responses) ────────────────────

/// UI configuration returned by GET /config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub input_accent: String,
    pub user_accent: String,
    pub agent_names: Vec<String>,
    /// Agents available as subagent tool targets (includes "all" mode agents).
    pub subagent_names: Vec<String>,
    pub model_info: String,
}

/// Session listing entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub created: String,
    pub model: String,
    pub message_count: usize,
    /// Short summary derived from the first user message.
    #[serde(default)]
    pub summary: Option<String>,
}

/// Model listing entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackResultPayload {
    pub success: bool,
    pub message: String,
    #[serde(default)]
    pub changes: Vec<FileChange>,
}

/// Full session data returned by GET /sessions/:id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub id: String,
    pub created: String,
    pub model: String,
    pub messages: Vec<HistoryMessage>,
}

// ── Health check ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    /// Protocol version for compatibility checks.
    #[serde(default)]
    pub protocol_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version_exists() {
        assert!(!PROTOCOL_VERSION.is_empty());
    }

    #[test]
    fn test_history_message_partial_eq() {
        let a = HistoryMessage {
            role: "user".into(),
            content: "hi".into(),
        };
        let b = HistoryMessage {
            role: "user".into(),
            content: "hi".into(),
        };
        let c = HistoryMessage {
            role: "assistant".into(),
            content: "hi".into(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_tool_status_partial_eq() {
        assert_eq!(ToolStatus::Running, ToolStatus::Running);
        assert_eq!(ToolStatus::Success, ToolStatus::Success);
        assert_ne!(ToolStatus::Running, ToolStatus::Success);
        assert_eq!(
            ToolStatus::Failed("err".into()),
            ToolStatus::Failed("err".into())
        );
    }

    #[test]
    fn test_health_response_default_version() {
        let json = r#"{"status":"ok"}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, "ok");
        assert_eq!(resp.protocol_version, ""); // default when missing
    }

    #[test]
    fn test_health_response_with_version() {
        let json = r#"{"status":"ok","protocol_version":"1"}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.protocol_version, "1");
    }

    #[test]
    fn test_server_event_unknown_variant() {
        let json = r#"{"type":"future_event","some_field":42}"#;
        let evt: ServerEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(evt, ServerEvent::Unknown));
    }
}
