use anyhow::Result;
use async_trait::async_trait;
use marshaling_protocol::FileChange;
use serde::{Deserialize, Serialize};
use std::fmt;

// ── Roles ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::System => write!(f, "system"),
            Role::User => write!(f, "user"),
            Role::Assistant => write!(f, "assistant"),
            Role::Tool => write!(f, "tool"),
        }
    }
}

// ── Tool types ────────────────────────────────────────────

/// A tool call emitted by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

/// A tool definition sent to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub def_type: String,
    pub function: ToolFunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// ── Chat messages ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Reasoning/thinking content (DeepSeek reasoning models, OpenAI o1/o3).
    /// Must be passed back in subsequent requests when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Loop-only metadata. This must never be sent to a provider or persisted.
    #[serde(skip)]
    pub internal_role_task: bool,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            internal_role_task: false,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            internal_role_task: false,
        }
    }

    #[allow(dead_code)]
    pub fn assistant_text(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            internal_role_task: false,
        }
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self::assistant_tool_calls_with_content(calls, None)
    }

    pub fn assistant_tool_calls_with_content(
        calls: Vec<ToolCall>,
        content: Option<String>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: Some(calls),
            tool_call_id: None,
            reasoning_content: None,
            internal_role_task: false,
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            internal_role_task: false,
        }
    }

    /// Create a user message used internally to hand work to a role.
    pub fn role_task(content: impl Into<String>) -> Self {
        let mut message = Self::user(content);
        message.internal_role_task = true;
        message
    }
}

// ── Usage ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

// ── Chat options ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChatOptions {
    pub model_id: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub tools: Vec<ToolDef>,
}

impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            model_id: "unknown".into(),
            temperature: 0.3,
            max_tokens: 4096,
            tools: Vec::new(),
        }
    }
}

// ── Chat result ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ChatResult {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// Provider-reported reason this assistant turn ended.
    /// Examples: "stop", "tool_calls", "length", "content_filter".
    pub finish_reason: Option<String>,
    /// Reasoning/thinking content (DeepSeek r1, etc.)
    pub reasoning_content: Option<String>,
}

// ── Stream events ─────────────────────────────────────────

/// An event produced during streaming.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of response text.
    Chunk(String),
    /// A chunk of reasoning/thinking text (DeepSeek reasoning models).
    ReasoningChunk(String),
    /// The stream finished.
    Done(ChatResult),
}

// ── Tool trait ────────────────────────────────────────────

/// A single executable tool (read, write, bash, etc.).
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's definition. Must not change over the tool's lifetime:
    /// [`ToolRegistry`] computes it once and caches it.
    fn def(&self) -> ToolDef;
    async fn execute(
        &self,
        args: serde_json::Value,
    ) -> Result<ToolExecutionResult>;
}

#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub output: String,
    pub changes: Vec<FileChange>,
    /// Internal rollback metadata (server-only, not sent over protocol directly).
    pub rollback_entries: Vec<RollbackEntry>,
}

#[derive(Debug, Clone)]
pub enum RollbackKind {
    /// File existed before and was modified.
    Modified,
    /// File was created by tool execution.
    Added,
    /// File was deleted by tool execution.
    Removed,
}

#[derive(Debug, Clone)]
pub struct RollbackEntry {
    pub path: std::path::PathBuf,
    pub kind: RollbackKind,
    pub before_content: Option<String>,
    pub expected_after_hash: Option<u64>,
}

// ── Tool registry ─────────────────────────────────────────

/// An agent's tool set with definitions computed once and name lookup.
///
/// Also the single place tools are executed, so cross-cutting behavior
/// (panic isolation today; output limits and timeouts later) applies to every
/// tool uniformly.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    /// `defs[i]` is `tools[i].def()`, in registration order.
    defs: Vec<ToolDef>,
    /// Tool name → index; the first tool registered under a name wins.
    index: std::collections::HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        let defs: Vec<ToolDef> = tools.iter().map(|tool| tool.def()).collect();
        let mut index = std::collections::HashMap::new();
        for (i, def) in defs.iter().enumerate() {
            index.entry(def.function.name.clone()).or_insert(i);
        }
        Self { tools, defs, index }
    }

    /// Definitions of all tools, in registration order.
    pub fn defs(&self) -> &[ToolDef] {
        &self.defs
    }

    pub fn contains(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// Run tool `name`. A panicking tool must not take down the whole run,
    /// so a panic is caught and returned as an error.
    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<ToolExecutionResult> {
        use futures::FutureExt;
        let Some(&i) = self.index.get(name) else {
            anyhow::bail!("Unknown tool: {name}");
        };
        std::panic::AssertUnwindSafe(self.tools[i].execute(args))
            .catch_unwind()
            .await
            .unwrap_or_else(|panic| {
                Err(anyhow::anyhow!(
                    "tool '{name}' panicked: {}",
                    crate::agent::panic_message(panic.as_ref())
                ))
            })
    }
}

/// Create the default built-in tool set.
pub fn builtin_tools(workspace_root: std::path::PathBuf) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(super::tools::ReadTool::new(workspace_root.clone())),
        Box::new(super::tools::GlobTool::new(workspace_root.clone())),
        Box::new(super::tools::GrepTool::new(workspace_root.clone())),
        Box::new(super::tools::WriteTool::new(workspace_root.clone())),
        Box::new(super::tools::EditTool::new(workspace_root.clone())),
        Box::new(super::tools::DeleteTool::new(workspace_root.clone())),
        Box::new(super::tools::BashTool::new(workspace_root)),
    ]
}

// ── Provider trait ────────────────────────────────────────

#[async_trait]
#[allow(dead_code)] // trait methods used via dynamic dispatch
pub trait LlmProvider: Send + Sync {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResult>;

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
        sender: tokio::sync::mpsc::UnboundedSender<Result<StreamEvent>>,
    );

    /// List available model IDs from this provider.
    async fn list_models(&self) -> Result<Vec<String>>;
}

pub mod deepseek;
pub mod ollama;

/// Build a provider by name (useful when an agent overrides the provider).
pub fn build_provider_for(
    config: &crate::config::Config,
    auth: &crate::auth::Auth,
    provider_name: &str,
) -> Result<Box<dyn LlmProvider>> {
    match provider_name {
        "deepseek" => {
            Ok(Box::new(deepseek::DeepSeekProvider::new(config, auth)?))
        }
        "glm" => {
            Ok(Box::new(deepseek::DeepSeekProvider::new_glm(config, auth)?))
        }
        "kimi" => Ok(Box::new(deepseek::DeepSeekProvider::new_kimi(
            config, auth,
        )?)),
        "minimax" => Ok(Box::new(deepseek::DeepSeekProvider::new_minimax(
            config, auth,
        )?)),
        "ollama" => Ok(Box::new(ollama::OllamaProvider::new(config, auth)?)),
        other => Err(anyhow::anyhow!(
            "Unknown provider '{}'. Supported: deepseek, glm, kimi, minimax, ollama",
            other
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_message() {
        let msg = ChatMessage::system("You are a helpful assistant.");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.content, Some("You are a helpful assistant.".into()));
        assert!(msg.tool_calls.is_none());
        assert!(msg.tool_call_id.is_none());
    }

    #[test]
    fn test_user_message() {
        let msg = ChatMessage::user("Hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, Some("Hello".into()));
    }

    #[test]
    fn test_tool_result_message() {
        let msg = ChatMessage::tool_result("call_123", "some output");
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.content, Some("some output".into()));
        assert_eq!(msg.tool_call_id, Some("call_123".into()));
    }

    #[test]
    fn test_assistant_tool_calls() {
        let calls = vec![ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: ToolFunction {
                name: "read".into(),
                arguments: r#"{"file_path": "test.txt"}"#.into(),
            },
        }];
        let msg = ChatMessage::assistant_tool_calls(calls);
        assert_eq!(msg.role, Role::Assistant);
        assert!(msg.content.is_none());
        assert!(msg.tool_calls.is_some());
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(msg.tool_calls.as_ref().unwrap()[0].function.name, "read");
    }

    #[test]
    fn test_chat_options_default() {
        let opts = ChatOptions::default();
        assert_eq!(opts.temperature, 0.3);
        assert_eq!(opts.max_tokens, 4096);
        assert!(opts.tools.is_empty());
    }

    #[test]
    fn test_tool_def_serialization() {
        let def = ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "test_tool".into(),
                description: "A test tool".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
        };
        let json = serde_json::to_value(&def).unwrap();
        assert_eq!(json["type"], "function");
        assert_eq!(json["function"]["name"], "test_tool");
    }

    #[test]
    fn test_chat_message_serialization_roundtrip() {
        let msg = ChatMessage::user("hello world");
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello world");
        // tool_calls and tool_call_id should be absent (skip_serializing_if)
        assert!(json.get("tool_calls").is_none());

        let deserialized: ChatMessage = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized.content, Some("hello world".into()));
    }

    #[test]
    fn test_role_task_marker_is_not_serialized() {
        let msg = ChatMessage::role_task("inspect it");
        let json = serde_json::to_value(&msg).unwrap();

        assert!(msg.internal_role_task);
        assert!(json.get("internal_role_task").is_none());
        let deserialized: ChatMessage = serde_json::from_value(json).unwrap();
        assert!(!deserialized.internal_role_task);
    }

    #[test]
    fn test_tool_call_message_omits_content() {
        let calls = vec![ToolCall {
            id: "c1".into(),
            call_type: "function".into(),
            function: ToolFunction {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        }];
        let msg = ChatMessage::assistant_tool_calls(calls);
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "assistant");
        // content should be absent (None → skip_serializing_if)
        assert!(json.get("content").is_none());
        assert!(json["tool_calls"].is_array());
    }

    #[test]
    fn test_role_display() {
        assert_eq!(format!("{}", Role::System), "system");
        assert_eq!(format!("{}", Role::User), "user");
        assert_eq!(format!("{}", Role::Assistant), "assistant");
        assert_eq!(format!("{}", Role::Tool), "tool");
    }

    #[test]
    fn test_usage_default() {
        let u = Usage::default();
        assert_eq!(u.prompt_tokens, 0);
        assert_eq!(u.completion_tokens, 0);
    }

    struct EchoTool {
        name: &'static str,
        output: &'static str,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                def_type: "function".into(),
                function: ToolFunctionDef {
                    name: self.name.into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<ToolExecutionResult> {
            if self.output == "panic" {
                panic!("echo exploded");
            }
            Ok(ToolExecutionResult {
                output: self.output.into(),
                changes: Vec::new(),
                rollback_entries: Vec::new(),
            })
        }
    }

    fn echo(name: &'static str, output: &'static str) -> Box<dyn Tool> {
        Box::new(EchoTool { name, output })
    }

    #[tokio::test]
    async fn test_tool_registry_lookup_order_and_duplicates() {
        let registry = ToolRegistry::new(vec![
            echo("read", "first"),
            echo("bash", "b"),
            echo("read", "second"),
        ]);
        let names: Vec<&str> = registry
            .defs()
            .iter()
            .map(|d| d.function.name.as_str())
            .collect();
        assert_eq!(names, ["read", "bash", "read"]);
        assert!(registry.contains("bash"));
        assert!(!registry.contains("write"));
        // The first tool registered under a name wins.
        let out = registry
            .execute("read", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out.output, "first");
    }

    #[tokio::test]
    async fn test_tool_registry_execute_errors_for_unknown_and_panicking_tools()
    {
        let registry = ToolRegistry::new(vec![echo("boom", "panic")]);
        let unknown = registry
            .execute("nope", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(format!("{unknown}").contains("Unknown tool: nope"));
        let panicked = registry
            .execute("boom", serde_json::json!({}))
            .await
            .unwrap_err();
        let message = format!("{panicked}");
        assert!(message.contains("tool 'boom' panicked"), "{message}");
        assert!(message.contains("echo exploded"), "{message}");
    }
}
