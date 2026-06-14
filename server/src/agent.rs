use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::llm::*;
use crate::prompt::ToolResultSummary;

// Re-export protocol types used for tool call display
pub use marshaling_protocol::{FileChange, ToolCallDisplay, ToolStatus};

/// Default max steps if not configured.
pub const DEFAULT_MAX_STEPS: usize = 10;

/// Truncate a string to at most `max_bytes` bytes without panicking on
/// multi-byte character boundaries. Returns the original string if it fits.
pub fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Events emitted by the agent loop to the TUI.
#[derive(Debug)]
pub enum AgentEvent {
    /// Text delta from the current LLM turn.
    TextDelta(String),
    /// Reasoning/thinking text delta (DeepSeek reasoning models).
    ReasoningDelta(String),
    /// A tool call has started.
    ToolStarted { id: String, name: String },
    /// A tool call completed.
    ToolCompleted {
        id: String,
        name: String,
        result: String,
        changes: Vec<FileChange>,
        rollback_entries: Vec<crate::llm::RollbackEntry>,
    },
    /// A tool call failed.
    ToolFailed { id: String, error: String },
    /// One turn of the agent loop completed (text + tool calls are done).
    TurnDone {
        text: String,
        tool_calls: Vec<ToolCallDisplay>,
    },
    /// Permission requested for a tool execution (user must approve/deny).
    PermissionRequest {
        id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// Skills that have been loaded for this session.
    SkillsLoaded { names: Vec<String> },
    /// A skill was selected via use_skill tool.
    SkillSelected { name: String },
    /// A subagent has been started during the agent loop.
    SubagentStarted { id: String, name: String },
    /// Text delta from a running subagent.
    SubagentTextDelta { id: String, data: String },
    /// Reasoning/thinking delta from a running subagent.
    SubagentReasoningDelta { id: String, data: String },
    /// A tool was started inside a subagent.
    SubagentToolStarted {
        id: String,
        sub_id: String,
        tool_name: String,
    },
    /// A tool completed inside a subagent.
    SubagentToolCompleted {
        id: String,
        sub_id: String,
        result: String,
        changes: Vec<FileChange>,
    },
    /// A tool failed inside a subagent.
    SubagentToolFailed {
        id: String,
        sub_id: String,
        error: String,
    },
    /// A subagent has finished.
    SubagentDone { id: String, content: String },
    /// The agent loop has finished.
    Done {
        content: String,
        tokens_input: u64,
        tokens_output: u64,
        /// Full conversation history including system messages, tool results, etc.
        history: Vec<ChatMessage>,
    },
}

/// Run the agent loop.
///
/// Takes the user message, system prompts, tool set, and previous LLM history.
/// Sends `AgentEvent`s back through `events_tx` for the TUI to render.
/// Checks `cancel_rx` periodically to abort.
pub async fn run_loop(
    provider: Arc<dyn LlmProvider>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    system_layers: Vec<String>,
    user_message: String,
    mut history: Vec<ChatMessage>,
    options: ChatOptions,
    events_tx: UnboundedSender<Result<AgentEvent>>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    mut permission_rx: tokio::sync::mpsc::UnboundedReceiver<(String, bool)>,
    // Pre-resolved permission map: tool_name → Permission
    permissions: std::collections::HashMap<String, crate::config::Permission>,
    // Configurable max steps (defaults to DEFAULT_MAX_STEPS if 0)
    max_steps: usize,
    // Workspace context for dynamic reminder text.
    working_directory: String,
) {
    let max_steps = if max_steps == 0 {
        DEFAULT_MAX_STEPS
    } else {
        max_steps
    };
    // Add the user message
    history.push(ChatMessage::user(&user_message));

    // Emit SkillsLoaded event from system layers
    let skill_names: Vec<String> = system_layers
        .iter()
        .filter_map(|layer| {
            if layer.starts_with("Skills available:") {
                // Extract skill names from lines like "  name — desc"
                Some(
                    layer
                        .lines()
                        .skip(1)
                        .filter_map(|line| {
                            let line = line.trim();
                            if line.is_empty() || !line.contains(" — ") {
                                return None;
                            }
                            line.split(" — ").next().map(|s| s.to_string())
                        })
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            }
        })
        .flatten()
        .collect();
    if !skill_names.is_empty() {
        let _ =
            events_tx.send(Ok(AgentEvent::SkillsLoaded { names: skill_names }));
    }

    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;

    for _step in 0..max_steps {
        tracing::debug!(
            "agent turn {}/{}: {} tools, {} history messages",
            _step + 1,
            max_steps,
            tools.len(),
            history.len()
        );

        // Check cancel
        if *cancel_rx.borrow() {
            let _ = events_tx.send(Ok(AgentEvent::Done {
                content: "(cancelled)".into(),
                tokens_input: total_input,
                tokens_output: total_output,
                history,
            }));
            return;
        }

        // ── Phase 1: Build messages and stream from LLM ─────────

        // Build messages: system + reminder + history
        // Use iter().cloned() to avoid allocating an intermediate Vec from history.clone()
        let mut messages: Vec<ChatMessage> =
            Vec::with_capacity(system_layers.len() + 1 + history.len());
        messages.extend(system_layers.iter().map(|l| ChatMessage::system(l)));

        // Build and inject the dynamic system reminder (Layer 7)
        let last_user_msg = extract_last_user_message(&history);
        let last_turn_results = extract_last_turn_results(&history);
        let tool_defs: Vec<ToolDef> = tools.iter().map(|t| t.def()).collect();

        let reminder_ctx = crate::prompt::ReminderContext {
            step: _step + 1,
            max_steps,
            working_directory: working_directory.clone(),
            tool_defs: &tool_defs,
            last_turn_results,
            last_user_message: last_user_msg,
        };
        let reminder = crate::prompt::build_system_reminder(&reminder_ctx);
        messages.push(ChatMessage::system(&reminder));

        messages.extend(history.iter().cloned());

        // Build tool definitions for the API
        let mut opts = options.clone();
        opts.tools = tools.iter().map(|t| t.def()).collect();

        // Create channel for stream events
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::unbounded_channel();

        // Clone the Arc for the spawned task
        let prov = Arc::clone(&provider);
        tokio::spawn(async move {
            prov.chat_stream(&messages, &opts, stream_tx).await;
        });

        // Process stream events
        let mut text_buf = String::new();
        let mut result: Option<ChatResult> = None;

        while let Some(event) = stream_rx.recv().await {
            if *cancel_rx.borrow() {
                let _ = events_tx.send(Ok(AgentEvent::Done {
                    content: "(cancelled)".into(),
                    tokens_input: total_input,
                    tokens_output: total_output,
                    history,
                }));
                return;
            }
            match event {
                Ok(StreamEvent::Chunk(text)) => {
                    text_buf.push_str(&text);
                    let _ = events_tx.send(Ok(AgentEvent::TextDelta(text)));
                }
                Ok(StreamEvent::ReasoningChunk(text)) => {
                    let _ =
                        events_tx.send(Ok(AgentEvent::ReasoningDelta(text)));
                }
                Ok(StreamEvent::Done(r)) => {
                    result = Some(r);
                    break;
                }
                Err(e) => {
                    let _ = events_tx.send(Err(e));
                    return;
                }
            }
        }

        let result = match result {
            Some(r) => r,
            None => {
                let _ = events_tx.send(Ok(AgentEvent::Done {
                    content: text_buf,
                    tokens_input: total_input,
                    tokens_output: total_output,
                    history,
                }));
                return;
            }
        };

        // Accumulate token usage
        total_input += result.usage.prompt_tokens;
        total_output += result.usage.completion_tokens;

        // Check for tool calls
        if result.tool_calls.is_empty() {
            // No tools — agent is done
            let _ = events_tx.send(Ok(AgentEvent::Done {
                content: result.content.unwrap_or_default(),
                tokens_input: total_input,
                tokens_output: total_output,
                history,
            }));
            return;
        }

        // ── Phase 2: Execute tool calls ─────────────────────────

        // There are tool calls — add the assistant message to history (with reasoning content)
        tracing::debug!("→ {} tool call(s) from LLM", result.tool_calls.len());
        for tc in &result.tool_calls {
            tracing::debug!(
                "  tool: {} (args: {})",
                tc.function.name,
                safe_truncate(&tc.function.arguments, 120)
            );
        }
        let mut msg =
            ChatMessage::assistant_tool_calls(result.tool_calls.clone());
        msg.reasoning_content = result.reasoning_content;
        history.push(msg);

        // Execute each tool
        let mut displays = Vec::new();
        for tc in &result.tool_calls {
            if *cancel_rx.borrow() {
                let _ = events_tx.send(Ok(AgentEvent::Done {
                    content: "(cancelled)".into(),
                    tokens_input: total_input,
                    tokens_output: total_output,
                    history,
                }));
                return;
            }

            // Emit skill selected event when use_skill is called
            if tc.function.name == "use_skill" {
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(
                    &tc.function.arguments,
                ) {
                    if let Some(skill) =
                        args.get("skill_name").and_then(|v| v.as_str())
                    {
                        let _ = events_tx.send(Ok(AgentEvent::SkillSelected {
                            name: skill.to_string(),
                        }));
                        // Also send as reasoning so the TUI shows grey thinking text
                        let _ = events_tx.send(Ok(AgentEvent::ReasoningDelta(
                            format!("[Skill selected: {}]", skill),
                        )));
                    }
                }
            }

            let _ = events_tx.send(Ok(AgentEvent::ToolStarted {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
            }));

            // Find the tool
            let tool = match tools
                .iter()
                .find(|t| t.def().function.name == tc.function.name)
            {
                Some(t) => t,
                None => {
                    let err = format!("Unknown tool: {}", tc.function.name);
                    let _ = events_tx.send(Ok(AgentEvent::ToolFailed {
                        id: tc.id.clone(),
                        error: err.clone(),
                    }));
                    history.push(ChatMessage::tool_result(
                        &tc.id,
                        format!("Error: {}", err),
                    ));
                    displays.push(ToolCallDisplay {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        status: ToolStatus::Failed(err),
                        changes: Vec::new(),
                    });
                    continue;
                }
            };

            // Parse arguments
            let args: serde_json::Value =
                match serde_json::from_str(&tc.function.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        let err = format!("Failed to parse arguments: {}", e);
                        let _ = events_tx.send(Ok(AgentEvent::ToolFailed {
                            id: tc.id.clone(),
                            error: err.clone(),
                        }));
                        history.push(ChatMessage::tool_result(
                            &tc.id,
                            format!("Error: {}", err),
                        ));
                        displays.push(ToolCallDisplay {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            status: ToolStatus::Failed(err),
                            changes: Vec::new(),
                        });
                        continue;
                    }
                };

            // Check permission for this tool
            let perm = permissions
                .get(&tc.function.name)
                .copied()
                .unwrap_or(crate::config::Permission::Allow);
            match perm {
                crate::config::Permission::Allow => {} // proceed to execute
                crate::config::Permission::Deny => {
                    let err = format!(
                        "Permission denied: '{}' is not allowed for this agent",
                        tc.function.name
                    );
                    let _ = events_tx.send(Ok(AgentEvent::ToolFailed {
                        id: tc.id.clone(),
                        error: err.clone(),
                    }));
                    history.push(ChatMessage::tool_result(
                        &tc.id,
                        format!("Error: {}", err),
                    ));
                    displays.push(ToolCallDisplay {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        status: ToolStatus::Failed(err),
                        changes: Vec::new(),
                    });
                    continue;
                }
                crate::config::Permission::Ask => {
                    // Request user permission
                    let perm_id = format!("perm_{}", tc.id);
                    let _ = events_tx.send(Ok(AgentEvent::PermissionRequest {
                        id: perm_id.clone(),
                        tool_name: tc.function.name.clone(),
                        args: args.clone(),
                    }));
                    // Wait for permission response
                    let allowed = loop {
                        tokio::select! {
                            resp = permission_rx.recv() => {
                                match resp {
                                    Some((id, allowed)) if id == perm_id => break allowed,
                                    Some(_) => continue,
                                    None => break false,
                                }
                            }
                            _ = cancel_rx.changed() => {
                                if *cancel_rx.borrow() {
                                    let _ = events_tx.send(Ok(AgentEvent::Done { content: "(cancelled)".into(), tokens_input: total_input, tokens_output: total_output, history }));
                                    return;
                                }
                            }
                        }
                    };
                    if !allowed {
                        let err = format!(
                            "Permission denied by user for tool '{}'",
                            tc.function.name
                        );
                        let _ = events_tx.send(Ok(AgentEvent::ToolFailed {
                            id: tc.id.clone(),
                            error: err.clone(),
                        }));
                        history.push(ChatMessage::tool_result(
                            &tc.id,
                            format!("Error: {}", err),
                        ));
                        displays.push(ToolCallDisplay {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            status: ToolStatus::Failed(err),
                            changes: Vec::new(),
                        });
                        continue;
                    }
                }
            }

            // Execute
            match tool.execute(args).await {
                Ok(output) => {
                    let _ = events_tx.send(Ok(AgentEvent::ToolCompleted {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        result: output.output.clone(),
                        changes: output.changes.clone(),
                        rollback_entries: output.rollback_entries.clone(),
                    }));
                    history
                        .push(ChatMessage::tool_result(&tc.id, &output.output));
                    displays.push(ToolCallDisplay {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        status: ToolStatus::Success,
                        changes: output.changes,
                    });
                }
                Err(e) => {
                    let err = format!("{:#}", e);
                    let _ = events_tx.send(Ok(AgentEvent::ToolFailed {
                        id: tc.id.clone(),
                        error: err.clone(),
                    }));
                    history.push(ChatMessage::tool_result(
                        &tc.id,
                        format!("Error: {}", err),
                    ));
                    displays.push(ToolCallDisplay {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        status: ToolStatus::Failed(err),
                        changes: Vec::new(),
                    });
                }
            }
        }

        // ── Phase 3: Signal turn completion ────────────────────

        // Signal turn complete with tool displays
        let _ = events_tx.send(Ok(AgentEvent::TurnDone {
            text: text_buf,
            tool_calls: displays,
        }));

        // History now contains tool results — loop continues
    }

    let _ = events_tx.send(Ok(AgentEvent::Done {
        content: "(max steps reached)".into(),
        tokens_input: total_input,
        tokens_output: total_output,
        history,
    }));
}

/// Extract tool results from the most recent turn in history.
fn extract_last_turn_results(
    history: &[ChatMessage],
) -> Vec<ToolResultSummary> {
    // Find the most recent assistant message with tool calls
    let last_tool_call_map: std::collections::HashMap<&str, &str> = history
        .iter()
        .rev()
        .find_map(|msg| {
            if matches!(msg.role, Role::Assistant) {
                msg.tool_calls.as_ref().map(|calls| {
                    calls
                        .iter()
                        .map(|tc| (tc.id.as_str(), tc.function.name.as_str()))
                        .collect()
                })
            } else {
                None
            }
        })
        .unwrap_or_default();

    // Collect Tool messages from the end of history, pair them with tool names
    let mut results = Vec::new();
    for msg in history.iter().rev() {
        match msg.role {
            Role::Tool => {
                let tool_name = msg
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| last_tool_call_map.get(id.as_str()))
                    .copied()
                    .unwrap_or("unknown");
                let content = msg.content.as_deref().unwrap_or("");
                let summary = if content.len() > 120 {
                    format!("{}...", safe_truncate(content, 117))
                } else {
                    content.to_string()
                };
                let success = !content.trim_start().starts_with("Error:");
                results.push(ToolResultSummary {
                    tool_name: tool_name.to_string(),
                    success,
                    summary,
                });
            }
            _ => break,
        }
    }
    results.reverse();
    results
}

/// Extract the most recent user message for context.
fn extract_last_user_message(history: &[ChatMessage]) -> Option<String> {
    history
        .iter()
        .rev()
        .find(|msg| matches!(msg.role, Role::User))
        .and_then(|msg| msg.content.as_ref())
        .map(|c| {
            let text: String = c.chars().take(100).collect();
            if c.chars().count() > 100 {
                format!("{}...", text)
            } else {
                text
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_user(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    fn make_assistant(text: &str) -> ChatMessage {
        ChatMessage::assistant_text(text)
    }

    fn make_tool_call_msg(tool_calls: Vec<(&str, &str)>) -> ChatMessage {
        let calls: Vec<ToolCall> = tool_calls
            .into_iter()
            .map(|(id, name)| ToolCall {
                id: id.to_string(),
                call_type: "function".into(),
                function: ToolFunction {
                    name: name.to_string(),
                    arguments: "{}".into(),
                },
            })
            .collect();
        ChatMessage::assistant_tool_calls(calls)
    }

    fn make_tool_result(tool_call_id: &str, content: &str) -> ChatMessage {
        ChatMessage::tool_result(tool_call_id, content)
    }

    #[test]
    fn test_extract_last_user_message_finds_latest() {
        let history = vec![
            make_user("first"),
            make_assistant("ok"),
            make_user("second"),
        ];
        assert_eq!(extract_last_user_message(&history), Some("second".into()));
    }

    #[test]
    fn test_extract_last_user_message_none_when_empty() {
        assert_eq!(extract_last_user_message(&[]), None);
    }

    #[test]
    fn test_extract_last_user_message_truncates_long() {
        let long = "a".repeat(150);
        let history = vec![make_user(&long)];
        let result = extract_last_user_message(&history).unwrap();
        assert_eq!(result.len(), 103); // 100 chars + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_extract_last_turn_results_empty_when_no_tools() {
        let history = vec![make_user("hi"), make_assistant("hello")];
        assert!(extract_last_turn_results(&history).is_empty());
    }

    #[test]
    fn test_extract_last_turn_results_pairs_names_with_results() {
        let history = vec![
            make_user("do it"),
            make_tool_call_msg(vec![("call_1", "read"), ("call_2", "bash")]),
            make_tool_result("call_1", "file contents"),
            make_tool_result("call_2", "Error: not found"),
        ];
        let results = extract_last_turn_results(&history);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_name, "read");
        assert!(results[0].success);
        assert_eq!(results[1].tool_name, "bash");
        assert!(!results[1].success);
    }

    #[test]
    fn test_extract_last_turn_results_truncates_long_content() {
        let long = "x".repeat(200);
        let history = vec![
            make_user("do it"),
            make_tool_call_msg(vec![("c1", "read")]),
            make_tool_result("c1", &long),
        ];
        let results = extract_last_turn_results(&history);
        assert_eq!(results.len(), 1);
        assert!(results[0].summary.ends_with("..."));
        assert_eq!(results[0].summary.len(), 120); // 117 chars + "..."
    }

    #[test]
    fn test_safe_truncate_ascii() {
        assert_eq!(safe_truncate("hello", 3), "hel");
        assert_eq!(safe_truncate("hello", 10), "hello");
        assert_eq!(safe_truncate("hello", 5), "hello");
    }

    #[test]
    fn test_safe_truncate_multibyte() {
        // '€' is 3 bytes (U+20AC)
        let s = "€€€"; // 9 bytes
        assert_eq!(safe_truncate(s, 9), "€€€");
        assert_eq!(safe_truncate(s, 6), "€€");
        assert_eq!(safe_truncate(s, 5), "€"); // can't split mid-char, backs up to 3
        assert_eq!(safe_truncate(s, 3), "€");
        assert_eq!(safe_truncate(s, 2), ""); // can't fit even one '€'
    }

    #[test]
    fn test_safe_truncate_empty() {
        assert_eq!(safe_truncate("", 0), "");
        assert_eq!(safe_truncate("", 10), "");
    }
}
