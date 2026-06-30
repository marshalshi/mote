use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::llm::*;
use crate::prompt::ToolResultSummary;

// Re-export protocol types used for tool call display
pub use marshaling_protocol::{FileChange, ToolCallDisplay, ToolStatus};

/// Default max steps if not configured.
pub const DEFAULT_MAX_STEPS: usize = 30;

const MAX_STEPS_PROMPT: &str = r#"CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY."#;

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

fn advertised_tool_defs(
    tools: &[Box<dyn Tool>],
    permissions: &std::collections::HashMap<String, crate::config::Permission>,
) -> Vec<ToolDef> {
    tools
        .iter()
        .filter_map(|tool| {
            let def = tool.def();
            let perm = permissions
                .get(&def.function.name)
                .copied()
                .unwrap_or(crate::config::Permission::Ask);
            (perm != crate::config::Permission::Deny).then_some(def)
        })
        .collect()
}

fn assistant_turn_is_finished(result: &ChatResult) -> bool {
    result.tool_calls.is_empty()
        && matches!(
            result.finish_reason.as_deref(),
            Some("stop" | "length" | "content_filter")
        )
}

fn assistant_result_text(result: &ChatResult, streamed_text: &str) -> String {
    match result.content.as_deref() {
        Some(content) if !content.is_empty() || streamed_text.is_empty() => {
            content.to_string()
        }
        _ => streamed_text.to_string(),
    }
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
    /// The agent was explicitly cancelled by the user.
    Cancelled {
        content: String,
        tokens_input: u64,
        tokens_output: u64,
        history: Vec<ChatMessage>,
    },
    /// The loop stopped before an explicit finish_task completion.
    NeedsContinuation {
        content: String,
        tokens_input: u64,
        tokens_output: u64,
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

    let mut step = 0usize;
    loop {
        step += 1;
        tracing::debug!(
            "agent turn {} (soft budget {}): {} tools, {} history messages",
            step,
            max_steps,
            tools.len(),
            history.len()
        );

        // Check cancel
        if *cancel_rx.borrow() {
            let _ = events_tx.send(Ok(AgentEvent::Cancelled {
                content: "(cancelled)".into(),
                tokens_input: total_input,
                tokens_output: total_output,
                history,
            }));
            return;
        }

        let soft_final_step = max_steps.saturating_add(1);
        // Hard turn budget fallback: after max_steps normal turns, the next
        // turn is a soft, text-only finalization step. If the model still
        // fails to produce a terminal text response there, stop before
        // exceeding the fallback budget.
        if step > soft_final_step {
            let _ = events_tx.send(Ok(AgentEvent::NeedsContinuation {
                content: "(max steps reached)".into(),
                tokens_input: total_input,
                tokens_output: total_output,
                history,
            }));
            return;
        }
        let final_text_only_step = step == soft_final_step;

        // ── Phase 1: Build messages and stream from LLM ─────────

        // Build messages: system + reminder + history
        // Use iter().cloned() to avoid allocating an intermediate Vec from history.clone()
        let mut messages: Vec<ChatMessage> =
            Vec::with_capacity(system_layers.len() + 1 + history.len());
        messages.extend(system_layers.iter().map(|l| ChatMessage::system(l)));

        // Build and inject the dynamic system reminder (Layer 7)
        let last_user_msg = extract_last_user_message(&history);
        let last_turn_results = extract_last_turn_results(&history);
        let tool_defs = advertised_tool_defs(&tools, &permissions);

        let reminder_ctx = crate::prompt::ReminderContext {
            step,
            max_steps,
            working_directory: working_directory.clone(),
            tool_defs: &tool_defs,
            last_turn_results,
            last_user_message: last_user_msg,
        };
        let reminder = crate::prompt::build_system_reminder(&reminder_ctx);
        messages.push(ChatMessage::system(&reminder));

        messages.extend(history.iter().cloned());
        if final_text_only_step {
            messages.push(ChatMessage::assistant_text(MAX_STEPS_PROMPT));
        }

        // Build tool definitions for the API
        let mut opts = options.clone();
        opts.tools = if final_text_only_step {
            Vec::new()
        } else {
            tool_defs
        };

        // Create channel for stream events
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::unbounded_channel();

        // Clone the Arc for the spawned task
        let prov = Arc::clone(&provider);
        let stream_handle = tokio::spawn(async move {
            prov.chat_stream(&messages, &opts, stream_tx).await;
        });

        // Process stream events
        let mut text_buf = String::new();
        let mut result: Option<ChatResult> = None;

        loop {
            let event = tokio::select! {
                event = stream_rx.recv() => event,
                changed = cancel_rx.changed() => {
                    if changed.is_ok() && *cancel_rx.borrow() {
                        stream_handle.abort();
                        let _ = events_tx.send(Ok(AgentEvent::Cancelled {
                            content: "(cancelled)".into(),
                            tokens_input: total_input,
                            tokens_output: total_output,
                            history,
                        }));
                        return;
                    }
                    continue;
                }
            };
            let Some(event) = event else {
                break;
            };
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
                let _ = events_tx.send(Ok(AgentEvent::NeedsContinuation {
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

        // Match OpenCode's stop semantics: only end the task when this
        // assistant turn is actually finished *and* there is no pending tool
        // work left to execute. Plain text alone is not sufficient because the
        // model may still intend to continue on the next turn.
        if result.tool_calls.is_empty() {
            let turn_finished = assistant_turn_is_finished(&result);
            let content = assistant_result_text(&result, &text_buf);
            history.push(ChatMessage::assistant_text(content.clone()));
            if turn_finished {
                let _ = events_tx.send(Ok(AgentEvent::Done {
                    content,
                    tokens_input: total_input,
                    tokens_output: total_output,
                    history,
                }));
                return;
            }
            if final_text_only_step {
                let _ = events_tx.send(Ok(AgentEvent::NeedsContinuation {
                    content,
                    tokens_input: total_input,
                    tokens_output: total_output,
                    history,
                }));
                return;
            }
            let _ = events_tx.send(Ok(AgentEvent::TurnDone {
                text: content,
                tool_calls: Vec::new(),
            }));
            continue;
        }

        if final_text_only_step {
            let content = assistant_result_text(&result, &text_buf);
            history.push(ChatMessage::assistant_text(content.clone()));
            let _ = events_tx.send(Ok(AgentEvent::NeedsContinuation {
                content: if content.is_empty() {
                    "(max steps reached)".into()
                } else {
                    content
                },
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
        let turn_text = assistant_result_text(&result, &text_buf);
        let mut msg = ChatMessage::assistant_tool_calls_with_content(
            result.tool_calls.clone(),
            (!turn_text.is_empty()).then_some(turn_text.clone()),
        );
        msg.reasoning_content = result.reasoning_content;
        history.push(msg);

        // Execute each tool
        let mut displays = Vec::new();
        for tc in &result.tool_calls {
            if *cancel_rx.borrow() {
                let _ = events_tx.send(Ok(AgentEvent::Cancelled {
                    content: "(cancelled)".into(),
                    tokens_input: total_input,
                    tokens_output: total_output,
                    history,
                }));
                return;
            }

            if tc.function.name == "finish_task" {
                let final_answer = serde_json::from_str::<serde_json::Value>(
                    &tc.function.arguments,
                )
                .ok()
                .and_then(|args| {
                    args.get("final_answer")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .or_else(|| result.content.clone())
                .unwrap_or_else(|| "(task finished)".to_string());
                history.push(ChatMessage::assistant_text(final_answer.clone()));
                let _ = events_tx.send(Ok(AgentEvent::Done {
                    content: final_answer,
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
                .unwrap_or(crate::config::Permission::Ask);
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
                                    let _ = events_tx.send(Ok(AgentEvent::Cancelled { content: "(cancelled)".into(), tokens_input: total_input, tokens_output: total_output, history }));
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
            text: turn_text,
            tool_calls: displays,
        }));

        // History now contains tool results — loop continues
    }
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
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

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

    struct MockProvider {
        seen_tools: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &ChatOptions,
        ) -> Result<ChatResult> {
            unreachable!("run_loop uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            options: &ChatOptions,
            sender: tokio::sync::mpsc::UnboundedSender<Result<StreamEvent>>,
        ) {
            let names: Vec<String> = options
                .tools
                .iter()
                .map(|t| t.function.name.clone())
                .collect();
            self.seen_tools.lock().unwrap().extend(names);
            let _ = sender.send(Ok(StreamEvent::Done(ChatResult {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_finish".into(),
                    call_type: "function".into(),
                    function: ToolFunction {
                        name: "finish_task".into(),
                        arguments: r#"{"final_answer":"final answer"}"#.into(),
                    },
                }],
                usage: Usage {
                    prompt_tokens: 1,
                    completion_tokens: 2,
                    total_tokens: 3,
                },
                finish_reason: Some("tool_calls".into()),
                reasoning_content: None,
            })));
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                def_type: "function".into(),
                function: ToolFunctionDef {
                    name: self.0.into(),
                    description: "test tool".into(),
                    parameters: serde_json::json!({"type":"object"}),
                },
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<ToolExecutionResult> {
            Ok(ToolExecutionResult {
                output: "ok".into(),
                changes: Vec::new(),
                rollback_entries: Vec::new(),
            })
        }
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
    fn test_advertised_tool_defs_excludes_denied_tools() {
        let tools: Vec<Box<dyn Tool>> =
            vec![Box::new(NamedTool("read")), Box::new(NamedTool("bash"))];
        let permissions = std::collections::HashMap::from([
            ("read".to_string(), crate::config::Permission::Allow),
            ("bash".to_string(), crate::config::Permission::Deny),
        ]);

        let advertised = advertised_tool_defs(&tools, &permissions);

        assert_eq!(advertised.len(), 1);
        assert_eq!(advertised[0].function.name, "read");
    }

    #[test]
    fn test_assistant_result_text_prefers_final_content() {
        let result = ChatResult {
            content: Some("final plain text".into()),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: Some("stop".into()),
            reasoning_content: None,
        };

        assert_eq!(
            assistant_result_text(&result, "partial stream"),
            "final plain text"
        );
    }

    #[test]
    fn test_assistant_result_text_falls_back_to_streamed_text() {
        let result = ChatResult {
            content: None,
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: Some("stop".into()),
            reasoning_content: None,
        };

        assert_eq!(assistant_result_text(&result, "streamed"), "streamed");
    }

    #[tokio::test]
    async fn test_run_loop_persists_final_assistant_message() {
        let seen_tools = Arc::new(Mutex::new(Vec::new()));
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider {
            seen_tools: Arc::clone(&seen_tools),
        });
        let tools: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
            Box::new(NamedTool("read")),
            Box::new(NamedTool("bash")),
            Box::new(NamedTool("finish_task")),
        ]);
        let permissions = std::collections::HashMap::from([
            ("read".to_string(), crate::config::Permission::Allow),
            ("bash".to_string(), crate::config::Permission::Deny),
            ("finish_task".to_string(), crate::config::Permission::Allow),
        ]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();

        run_loop(
            provider,
            tools,
            Vec::new(),
            "hello".into(),
            Vec::new(),
            ChatOptions::default(),
            events_tx,
            cancel_rx,
            perm_rx,
            permissions,
            2,
            "/tmp".into(),
        )
        .await;

        let mut done_history = None;
        while let Some(event) = events_rx.recv().await {
            if let AgentEvent::Done { history, .. } = event.unwrap() {
                done_history = Some(history);
                break;
            }
        }
        let history = done_history.expect("Done event should be emitted");
        assert!(matches!(history[0].role, Role::User));
        assert!(matches!(history.last().unwrap().role, Role::Assistant));
        assert_eq!(
            history.last().unwrap().content.as_deref(),
            Some("final answer")
        );
        assert_eq!(
            seen_tools.lock().unwrap().as_slice(),
            ["read", "finish_task"]
        );
    }

    struct ScriptedProvider {
        calls: Arc<Mutex<usize>>,
        responses: Arc<Mutex<std::collections::VecDeque<ChatResult>>>,
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &ChatOptions,
        ) -> Result<ChatResult> {
            unreachable!("run_loop uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _options: &ChatOptions,
            sender: tokio::sync::mpsc::UnboundedSender<Result<StreamEvent>>,
        ) {
            *self.calls.lock().unwrap() += 1;
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response missing");
            let _ = sender.send(Ok(StreamEvent::Done(response)));
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    struct ObservingScriptedProvider {
        calls: Arc<Mutex<usize>>,
        tool_counts: Arc<Mutex<Vec<usize>>>,
        saw_max_steps_prompt: Arc<Mutex<bool>>,
        responses: Arc<Mutex<std::collections::VecDeque<ChatResult>>>,
    }

    #[async_trait]
    impl LlmProvider for ObservingScriptedProvider {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &ChatOptions,
        ) -> Result<ChatResult> {
            unreachable!("run_loop uses chat_stream")
        }

        async fn chat_stream(
            &self,
            messages: &[ChatMessage],
            options: &ChatOptions,
            sender: tokio::sync::mpsc::UnboundedSender<Result<StreamEvent>>,
        ) {
            *self.calls.lock().unwrap() += 1;
            self.tool_counts.lock().unwrap().push(options.tools.len());
            if messages
                .iter()
                .any(|msg| msg.content.as_deref() == Some(MAX_STEPS_PROMPT))
            {
                *self.saw_max_steps_prompt.lock().unwrap() = true;
            }
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response missing");
            let _ = sender.send(Ok(StreamEvent::Done(response)));
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// Emits a single (non-finish_task) tool call on every call, so the loop
    /// would run forever without a hard step budget.
    struct LoopingToolProvider {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl LlmProvider for LoopingToolProvider {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _options: &ChatOptions,
        ) -> Result<ChatResult> {
            unreachable!("run_loop uses chat_stream")
        }

        async fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _options: &ChatOptions,
            sender: tokio::sync::mpsc::UnboundedSender<Result<StreamEvent>>,
        ) {
            let id = {
                let mut n = self.calls.lock().unwrap();
                *n += 1;
                format!("call_{}", *n)
            };
            let _ = sender.send(Ok(StreamEvent::Done(ChatResult {
                content: None,
                tool_calls: vec![ToolCall {
                    id,
                    call_type: "function".into(),
                    function: ToolFunction {
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                }],
                usage: Usage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                },
                finish_reason: Some("tool_calls".into()),
                reasoning_content: None,
            })));
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// A text-only turn without a terminal finish reason is not enough to end
    /// the task. The loop continues until an actual finish signal or the hard
    /// step budget stops it.
    #[tokio::test]
    async fn test_run_loop_plain_text_without_finish_reason_continues() {
        let calls = Arc::new(Mutex::new(0usize));
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            calls: Arc::clone(&calls),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [
                    ChatResult {
                        content: Some("working...".into()),
                        tool_calls: Vec::new(),
                        usage: Usage {
                            prompt_tokens: 1,
                            completion_tokens: 1,
                            total_tokens: 2,
                        },
                        finish_reason: None,
                        reasoning_content: None,
                    },
                    ChatResult {
                        content: Some("still working...".into()),
                        tool_calls: Vec::new(),
                        usage: Usage {
                            prompt_tokens: 1,
                            completion_tokens: 1,
                            total_tokens: 2,
                        },
                        finish_reason: None,
                        reasoning_content: None,
                    },
                    ChatResult {
                        content: Some(
                            "Maximum steps reached. Summary before continuation."
                                .into(),
                        ),
                        tool_calls: Vec::new(),
                        usage: Usage {
                            prompt_tokens: 1,
                            completion_tokens: 1,
                            total_tokens: 2,
                        },
                        finish_reason: Some("stop".into()),
                        reasoning_content: None,
                    },
                ],
            ))),
        });
        let tools: Arc<Vec<Box<dyn Tool>>> =
            Arc::new(vec![Box::new(NamedTool("read"))]);
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();

        run_loop(
            provider,
            tools,
            Vec::new(),
            "hi".into(),
            Vec::new(),
            ChatOptions::default(),
            events_tx,
            cancel_rx,
            perm_rx,
            permissions,
            2,
            "/tmp".into(),
        )
        .await;

        let mut saw_turn_done = 0;
        let mut terminal = None;
        while let Some(event) = events_rx.recv().await {
            match event.unwrap() {
                AgentEvent::TurnDone { .. } => saw_turn_done += 1,
                AgentEvent::Done { content, .. } => {
                    terminal = Some(content);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(saw_turn_done, 2);
        assert_eq!(
            terminal.as_deref(),
            Some("Maximum steps reached. Summary before continuation.")
        );
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn test_run_loop_terminal_finish_reason_ends_text_only_turn() {
        let calls = Arc::new(Mutex::new(0usize));
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            calls: Arc::clone(&calls),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [ChatResult {
                    content: Some("all done".into()),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        total_tokens: 2,
                    },
                    finish_reason: Some("stop".into()),
                    reasoning_content: None,
                }],
            ))),
        });
        let tools: Arc<Vec<Box<dyn Tool>>> =
            Arc::new(vec![Box::new(NamedTool("read"))]);
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();

        run_loop(
            provider,
            tools,
            Vec::new(),
            "hi".into(),
            Vec::new(),
            ChatOptions::default(),
            events_tx,
            cancel_rx,
            perm_rx,
            permissions,
            10,
            "/tmp".into(),
        )
        .await;

        let mut done_content = None;
        while let Some(event) = events_rx.recv().await {
            match event.unwrap() {
                AgentEvent::Done { content, .. } => {
                    done_content = Some(content);
                    break;
                }
                AgentEvent::TurnDone { .. } => {
                    panic!(
                        "terminal finish reason should not continue the loop"
                    )
                }
                _ => {}
            }
        }
        assert_eq!(done_content.as_deref(), Some("all done"));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    /// Even if a provider mislabels the finish reason as "stop", pending tool
    /// calls keep the loop alive until the follow-up turn completes.
    #[tokio::test]
    async fn test_run_loop_tool_calls_override_terminal_finish_reason() {
        let calls = Arc::new(Mutex::new(0usize));
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            calls: Arc::clone(&calls),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [
                    ChatResult {
                        content: Some("checking".into()),
                        tool_calls: vec![ToolCall {
                            id: "call_read".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: "read".into(),
                                arguments: "{}".into(),
                            },
                        }],
                        usage: Usage {
                            prompt_tokens: 1,
                            completion_tokens: 1,
                            total_tokens: 2,
                        },
                        finish_reason: Some("stop".into()),
                        reasoning_content: None,
                    },
                    ChatResult {
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: "call_finish".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: "finish_task".into(),
                                arguments: r#"{"final_answer":"final answer"}"#
                                    .into(),
                            },
                        }],
                        usage: Usage {
                            prompt_tokens: 1,
                            completion_tokens: 1,
                            total_tokens: 2,
                        },
                        finish_reason: Some("tool_calls".into()),
                        reasoning_content: None,
                    },
                ],
            ))),
        });
        let tools: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
            Box::new(NamedTool("read")),
            Box::new(NamedTool("finish_task")),
        ]);
        let permissions = std::collections::HashMap::from([
            ("read".to_string(), crate::config::Permission::Allow),
            ("finish_task".to_string(), crate::config::Permission::Allow),
        ]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();

        run_loop(
            provider,
            tools,
            Vec::new(),
            "hi".into(),
            Vec::new(),
            ChatOptions::default(),
            events_tx,
            cancel_rx,
            perm_rx,
            permissions,
            10,
            "/tmp".into(),
        )
        .await;

        let mut done_content = None;
        let mut tool_turn_text = None;
        while let Some(event) = events_rx.recv().await {
            match event.unwrap() {
                AgentEvent::TurnDone { text, tool_calls } => {
                    if !tool_calls.is_empty() {
                        tool_turn_text = Some(text);
                    }
                }
                AgentEvent::Done { content, .. } => {
                    done_content = Some(content);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(tool_turn_text.as_deref(), Some("checking"));
        assert_eq!(done_content.as_deref(), Some("final answer"));
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn test_run_loop_tool_call_turn_preserves_assistant_text_in_history()
    {
        let calls = Arc::new(Mutex::new(0usize));
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            calls: Arc::clone(&calls),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [
                    ChatResult {
                        content: Some(
                            "I need to inspect the file first".into(),
                        ),
                        tool_calls: vec![ToolCall {
                            id: "call_read".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: "read".into(),
                                arguments: "{}".into(),
                            },
                        }],
                        usage: Usage::default(),
                        finish_reason: Some("tool_calls".into()),
                        reasoning_content: None,
                    },
                    ChatResult {
                        content: Some("The answer after reading.".into()),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        finish_reason: Some("stop".into()),
                        reasoning_content: None,
                    },
                ],
            ))),
        });
        let tools: Arc<Vec<Box<dyn Tool>>> =
            Arc::new(vec![Box::new(NamedTool("read"))]);
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();

        run_loop(
            provider,
            tools,
            Vec::new(),
            "hi".into(),
            Vec::new(),
            ChatOptions::default(),
            events_tx,
            cancel_rx,
            perm_rx,
            permissions,
            10,
            "/tmp".into(),
        )
        .await;

        let mut done_history = None;
        while let Some(event) = events_rx.recv().await {
            if let AgentEvent::Done { history, .. } = event.unwrap() {
                done_history = Some(history);
                break;
            }
        }

        let history = done_history.expect("Done event should include history");
        let tool_call_message = history
            .iter()
            .find(|msg| msg.tool_calls.is_some())
            .expect("tool-call assistant message should be persisted");
        assert_eq!(
            tool_call_message.content.as_deref(),
            Some("I need to inspect the file first")
        );
        assert_eq!(
            history.last().and_then(|msg| msg.content.as_deref()),
            Some("The answer after reading.")
        );
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn test_run_loop_final_step_disables_tools_and_requests_text_only() {
        let calls = Arc::new(Mutex::new(0usize));
        let tool_counts = Arc::new(Mutex::new(Vec::new()));
        let saw_max_steps_prompt = Arc::new(Mutex::new(false));
        let provider: Arc<dyn LlmProvider> = Arc::new(ObservingScriptedProvider {
            calls: Arc::clone(&calls),
            tool_counts: Arc::clone(&tool_counts),
            saw_max_steps_prompt: Arc::clone(&saw_max_steps_prompt),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [
                    ChatResult {
                        content: Some("checking".into()),
                        tool_calls: vec![ToolCall {
                            id: "call_read".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: "read".into(),
                                arguments: "{}".into(),
                            },
                        }],
                        usage: Usage::default(),
                        finish_reason: Some("tool_calls".into()),
                        reasoning_content: None,
                    },
                    ChatResult {
                        content: Some(
                            "Maximum steps reached. I checked the file and need user continuation."
                                .into(),
                        ),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        finish_reason: Some("stop".into()),
                        reasoning_content: None,
                    },
                ],
            ))),
        });
        let tools: Arc<Vec<Box<dyn Tool>>> =
            Arc::new(vec![Box::new(NamedTool("read"))]);
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();

        run_loop(
            provider,
            tools,
            Vec::new(),
            "hi".into(),
            Vec::new(),
            ChatOptions::default(),
            events_tx,
            cancel_rx,
            perm_rx,
            permissions,
            1,
            "/tmp".into(),
        )
        .await;

        let mut done_content = None;
        while let Some(event) = events_rx.recv().await {
            if let AgentEvent::Done { content, .. } = event.unwrap() {
                done_content = Some(content);
                break;
            }
        }

        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(tool_counts.lock().unwrap().as_slice(), [1, 0]);
        assert!(*saw_max_steps_prompt.lock().unwrap());
        assert_eq!(
            done_content.as_deref(),
            Some(
                "Maximum steps reached. I checked the file and need user continuation."
            )
        );
    }

    /// A model that ignores the text-only final step is stopped at the hard
    /// fallback, which caps the number of LLM calls and emits the max-steps
    /// sentinel.
    #[tokio::test]
    async fn test_run_loop_hard_stops_at_max_steps() {
        let calls = Arc::new(Mutex::new(0usize));
        let provider: Arc<dyn LlmProvider> = Arc::new(LoopingToolProvider {
            calls: Arc::clone(&calls),
        });
        let tools: Arc<Vec<Box<dyn Tool>>> =
            Arc::new(vec![Box::new(NamedTool("read"))]);
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();

        let max_steps = 3usize;
        run_loop(
            provider,
            tools,
            Vec::new(),
            "go".into(),
            Vec::new(),
            ChatOptions::default(),
            events_tx,
            cancel_rx,
            perm_rx,
            permissions,
            max_steps,
            "/tmp".into(),
        )
        .await;

        let mut needs_continuation = None;
        while let Some(event) = events_rx.recv().await {
            if let AgentEvent::NeedsContinuation { content, .. } =
                event.unwrap()
            {
                needs_continuation = Some(content);
                break;
            }
        }
        assert_eq!(needs_continuation.as_deref(), Some("(max steps reached)"));
        // The loop gets max_steps normal calls plus one text-only finalization
        // call, then stops if the model still tries to use tools.
        assert_eq!(*calls.lock().unwrap(), max_steps + 1);
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
