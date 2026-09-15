use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::llm::*;
use crate::prompt::ToolResultSummary;

// Re-export protocol types used for tool call display
pub use marshaling_protocol::{FileChange, ToolCallDisplay, ToolStatus};

/// Default max steps if not configured.
pub const DEFAULT_MAX_STEPS: usize = 30;
const MAX_STREAM_RETRIES: usize = 5;

/// A fully resolved role ready for use in the agent loop.
/// Owns all data needed to switch the active role per turn.
#[derive(Clone)]
#[allow(dead_code)]
pub struct ResolvedRole {
    pub name: String,
    pub instructions: String,
    pub provider: Arc<dyn LlmProvider>,
    pub model_id: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// Configuration for role-aware loop mode.
/// When present in `run_loop`, the loop enters role-switching mode.
/// The first role is the orchestrator.
#[derive(Clone)]
#[allow(dead_code)]
pub struct RoleLoopConfig {
    pub roles: Vec<ResolvedRole>,
}

#[allow(dead_code)]
impl RoleLoopConfig {
    /// Find a role by name, returning its index.
    pub fn find_role(&self, name: &str) -> Option<usize> {
        self.roles.iter().position(|r| r.name == name)
    }
}

/// Build the tool definition for `switch_role`.
/// This is NOT a normal Tool trait object; it is injected directly into
/// the advertised tool list and its calls are intercepted by the loop.
fn switch_role_tool_def() -> ToolDef {
    ToolDef {
        def_type: "function".into(),
        function: ToolFunctionDef {
            name: "switch_role".into(),
            description: "Switch to a different role and pass it a task. Available roles are listed in the system prompt. Use this to delegate work to specialist roles, then switch back to the orchestrator when done.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "description": "Name of the role to switch to (e.g., 'coder', 'reviewer')"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task description for the target role. This will be presented as the next user message."
                    }
                },
                "required": ["role", "task"]
            }),
        },
    }
}

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

fn role_brief(instructions: &str) -> String {
    let Some(line) = instructions.lines().find(|line| !line.trim().is_empty())
    else {
        return "(no description)".to_string();
    };
    let trimmed = line.trim();
    if trimmed.chars().count() > 80 {
        format!("{}…", trimmed.chars().take(77).collect::<String>())
    } else {
        trimmed.to_string()
    }
}

fn is_retryable_stream_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    [
        "429",
        "500",
        "502",
        "503",
        "504",
        "rate limit",
        "too many requests",
        "overloaded",
        "service unavailable",
        "connection refused",
        "connection reset",
        "connection lost",
        "connection closed",
        "socket hang up",
        "timed out",
        "timeout",
        "network error",
        "stream read error",
        "stream ended before completion marker",
        "stream ended before done=true",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

fn stream_retry_delay(retry: usize) -> std::time::Duration {
    std::time::Duration::from_secs(1_u64 << retry.saturating_sub(1).min(4))
}

fn reap_stream_task(
    mut stream_handle: tokio::task::JoinHandle<()>,
    abort: bool,
) {
    if abort {
        stream_handle.abort();
    }
    tokio::spawn(async move {
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            &mut stream_handle,
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) if error.is_cancelled() => {}
            Ok(Err(error)) => {
                tracing::warn!("agent stream task ended unexpectedly: {error}");
            }
            Err(_) => {
                tracing::warn!(
                    "agent stream task did not stop within five seconds; aborting it"
                );
                stream_handle.abort();
                if let Err(error) = stream_handle.await {
                    if !error.is_cancelled() {
                        tracing::warn!(
                            "agent stream task ended unexpectedly after abort: {error}"
                        );
                    }
                }
            }
        }
    });
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
    // Role-aware loop config. When None, runs in legacy (single-role) mode.
    role_config: Option<RoleLoopConfig>,
) {
    let max_steps = if max_steps == 0 {
        DEFAULT_MAX_STEPS
    } else {
        max_steps
    };
    // Role-aware loop state: tracks which role is currently active.
    // When role_config is None (legacy mode), this remains at 0 and is unused.
    let mut current_role_idx: usize = 0;
    let role_mode = role_config.is_some();
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
            Vec::with_capacity(system_layers.len() + 2 + history.len());

        // Identify the skills layer (layer 6) so we can inject role layers (5)
        // before it: correct order is shared (1-4), role (5), skills (6), reminder (7).
        let has_skills = system_layers
            .last()
            .map_or(false, |l| l.starts_with("Skills available:"));
        let pre_skills_count = if has_skills {
            system_layers.len().saturating_sub(1)
        } else {
            system_layers.len()
        };

        // Inject shared layers (1-4): env, system prompt, global AGENTS.md,
        // workspace AGENTS.md
        for i in 0..pre_skills_count {
            messages.push(ChatMessage::system(&system_layers[i]));
        }

        // Inject role layers (5) — only in role mode
        if let Some(ref rc) = role_config {
            // Role roster
            let mut roster = String::from("Available roles in this agent:\n");
            for role in &rc.roles {
                // Brief description: first non-empty line of instructions (up to 80 chars)
                let brief = role_brief(&role.instructions);
                roster.push_str(&format!("  {} — {}\n", role.name, brief));
            }
            roster
                .push_str("\nUse switch_role to delegate tasks between roles.");
            messages.push(ChatMessage::system(&roster));

            // Current role's specific instructions
            let role_instructions = &rc.roles[current_role_idx].instructions;
            if !role_instructions.is_empty() {
                messages.push(ChatMessage::system(role_instructions));
            }
        }

        // Inject skills layer (6) after role layers
        if has_skills {
            messages.push(ChatMessage::system(
                &system_layers[system_layers.len() - 1],
            ));
        }

        // Build and inject the dynamic system reminder (Layer 7)
        let last_user_msg = extract_last_user_message(&history);
        let last_turn_results = extract_last_turn_results(&history);
        let mut tool_defs = advertised_tool_defs(&tools, &permissions);
        // When in role mode, inject the switch_role tool def so the model can
        // delegate to other roles. This is an internal loop tool, not a filesystem tool.
        if role_mode && !final_text_only_step {
            tool_defs.push(switch_role_tool_def());
        }

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
        let mut opts = if let Some(ref rc) = role_config {
            let role = &rc.roles[current_role_idx];
            ChatOptions {
                model_id: role.model_id.clone(),
                temperature: role.temperature.unwrap_or(options.temperature),
                max_tokens: role.max_tokens.unwrap_or(options.max_tokens),
                tools: Vec::new(), // populated below
            }
        } else {
            options.clone()
        };
        opts.tools = if final_text_only_step {
            Vec::new()
        } else {
            tool_defs
        };
        let model_id = opts.model_id.clone();
        let max_tokens = opts.max_tokens;

        // Clone the Arc for the spawned task
        let prov = if let Some(ref rc) = role_config {
            Arc::clone(&rc.roles[current_role_idx].provider)
        } else {
            Arc::clone(&provider)
        };
        // Process stream events
        let mut text_buf = String::new();
        let mut emitted_stream_data = false;
        let mut retries = 0;
        let result = 'stream_attempt: loop {
            // Each retry gets a fresh channel and task. Retrying only before any
            // streamed output avoids duplicating visible assistant text.
            let (stream_tx, mut stream_rx) =
                tokio::sync::mpsc::unbounded_channel();
            let provider = Arc::clone(&prov);
            let attempt_messages = messages.clone();
            let attempt_opts = opts.clone();
            let stream_handle = tokio::spawn(async move {
                provider
                    .chat_stream(&attempt_messages, &attempt_opts, stream_tx)
                    .await;
            });

            loop {
                let event = tokio::select! {
                    event = stream_rx.recv() => event,
                    changed = cancel_rx.changed() => {
                        if changed.is_ok() && *cancel_rx.borrow() {
                            reap_stream_task(stream_handle, true);
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
                    reap_stream_task(stream_handle, false);
                    if *cancel_rx.borrow() {
                        let _ = events_tx.send(Ok(AgentEvent::Cancelled {
                            content: "(cancelled)".into(),
                            tokens_input: total_input,
                            tokens_output: total_output,
                            history,
                        }));
                        return;
                    }
                    tracing::warn!(
                        step,
                        streamed_chars = text_buf.len(),
                        "agent stream closed without a completion result"
                    );
                    let _ = events_tx.send(Ok(AgentEvent::NeedsContinuation {
                        content: text_buf,
                        tokens_input: total_input,
                        tokens_output: total_output,
                        history,
                    }));
                    return;
                };
                match event {
                    Ok(StreamEvent::Chunk(text)) => {
                        emitted_stream_data = true;
                        text_buf.push_str(&text);
                        let _ = events_tx.send(Ok(AgentEvent::TextDelta(text)));
                    }
                    Ok(StreamEvent::ReasoningChunk(text)) => {
                        emitted_stream_data = true;
                        let _ = events_tx
                            .send(Ok(AgentEvent::ReasoningDelta(text)));
                    }
                    Ok(StreamEvent::Done(result)) => {
                        reap_stream_task(stream_handle, true);
                        break 'stream_attempt result;
                    }
                    Err(error)
                        if !emitted_stream_data
                            && is_retryable_stream_error(&error)
                            && retries < MAX_STREAM_RETRIES =>
                    {
                        reap_stream_task(stream_handle, true);
                        retries += 1;
                        let delay = stream_retry_delay(retries);
                        tracing::warn!(
                            step,
                            retry = retries,
                            max_retries = MAX_STREAM_RETRIES,
                            retry_delay_ms = delay.as_millis(),
                            "retrying transient agent stream failure before output"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            changed = cancel_rx.changed() => {
                                if changed.is_ok() && *cancel_rx.borrow() {
                                    let _ = events_tx.send(Ok(AgentEvent::Cancelled {
                                        content: "(cancelled)".into(),
                                        tokens_input: total_input,
                                        tokens_output: total_output,
                                        history,
                                    }));
                                    return;
                                }
                            }
                        }
                        continue 'stream_attempt;
                    }
                    Err(error) => {
                        reap_stream_task(stream_handle, true);
                        let _ = events_tx.send(Err(error));
                        return;
                    }
                }
            }
        };

        // Accumulate token usage
        total_input += result.usage.prompt_tokens;
        total_output += result.usage.completion_tokens;
        tracing::debug!(
            step,
            model_id = %model_id,
            max_tokens,
            finish_reason = ?result.finish_reason,
            tool_calls = result.tool_calls.len(),
            prompt_tokens = result.usage.prompt_tokens,
            completion_tokens = result.usage.completion_tokens,
            total_input,
            total_output,
            "agent LLM turn completed"
        );

        // Match OpenCode's stop semantics: only end the task when this
        // assistant turn is actually finished *and* there is no pending tool
        // work left to execute. Plain text alone is not sufficient because the
        // model may still intend to continue on the next turn.
        if result.tool_calls.is_empty() {
            let turn_finished = assistant_turn_is_finished(&result);
            let content = assistant_result_text(&result, &text_buf);
            history.push(ChatMessage::assistant_text(content.clone()));
            if turn_finished {
                if result.finish_reason.as_deref() == Some("length") {
                    tracing::warn!(
                        step,
                        model_id = %model_id,
                        max_tokens,
                        completion_tokens = result.usage.completion_tokens,
                        total_output,
                        "agent completed because the model reached its output token limit"
                    );
                } else {
                    tracing::info!(
                        step,
                        model_id = %model_id,
                        finish_reason = ?result.finish_reason,
                        total_input,
                        total_output,
                        "agent completed"
                    );
                }
                let _ = events_tx.send(Ok(AgentEvent::Done {
                    content,
                    tokens_input: total_input,
                    tokens_output: total_output,
                    history,
                }));
                return;
            }
            if final_text_only_step {
                tracing::warn!(
                    step,
                    finish_reason = ?result.finish_reason,
                    total_input,
                    total_output,
                    "agent reached its final text-only step without a terminal finish reason"
                );
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
            tracing::warn!(
                step,
                tool_calls = result.tool_calls.len(),
                total_input,
                total_output,
                "agent requested tools during its final text-only step"
            );
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
        let mut finish_task_answer = None;
        let mut pending_role_switch = None;
        let mut tool_batch_failed = false;
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
                finish_task_answer.get_or_insert(final_answer);
                let result_text = "Task completion acknowledged.";
                history.push(ChatMessage::tool_result(&tc.id, result_text));
                displays.push(ToolCallDisplay {
                    id: tc.id.clone(),
                    name: "finish_task".into(),
                    status: ToolStatus::Success,
                    changes: Vec::new(),
                });
                continue;
            }

            // ── Handle switch_role (role-mode loop-owned tool) ──────
            if tc.function.name == "switch_role" {
                let _ = events_tx.send(Ok(AgentEvent::ToolStarted {
                    id: tc.id.clone(),
                    name: "switch_role".into(),
                }));

                let args: serde_json::Value =
                    match serde_json::from_str(&tc.function.arguments) {
                        Ok(v) => v,
                        Err(e) => {
                            tool_batch_failed = true;
                            let err =
                                format!("Invalid switch_role arguments: {e}");
                            let _ =
                                events_tx.send(Ok(AgentEvent::ToolFailed {
                                    id: tc.id.clone(),
                                    error: err.clone(),
                                }));
                            history
                                .push(ChatMessage::tool_result(&tc.id, &err));
                            displays.push(ToolCallDisplay {
                                id: tc.id.clone(),
                                name: "switch_role".into(),
                                status: ToolStatus::Failed(err),
                                changes: Vec::new(),
                            });
                            continue;
                        }
                    };
                let target_role =
                    args.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let task =
                    args.get("task").and_then(|v| v.as_str()).unwrap_or("");

                match role_config.as_ref().and_then(|rc| {
                    rc.find_role(target_role).map(|idx| (rc, idx))
                }) {
                    Some((_rc, idx)) => {
                        // Valid role — switch and inject task
                        let task_msg = if task.is_empty() {
                            format!("Switched to role: {}", target_role)
                        } else {
                            task.to_string()
                        };
                        let result_text =
                            format!("Switched to role: {}", target_role);
                        // Push tool result first — providers expect tool
                        // results to immediately follow the assistant tool_call.
                        history.push(ChatMessage::tool_result(
                            &tc.id,
                            &result_text,
                        ));
                        let _ = events_tx.send(Ok(AgentEvent::ToolCompleted {
                            id: tc.id.clone(),
                            name: "switch_role".into(),
                            result: result_text,
                            changes: Vec::new(),
                            rollback_entries: Vec::new(),
                        }));
                        displays.push(ToolCallDisplay {
                            id: tc.id.clone(),
                            name: "switch_role".into(),
                            status: ToolStatus::Success,
                            changes: Vec::new(),
                        });
                        // Apply this after every result from the current
                        // assistant tool-call message has been recorded.
                        // If several switches are emitted, the last valid one
                        // wins because it is the last requested transition.
                        pending_role_switch = Some((idx, task_msg));
                    }
                    None => {
                        tool_batch_failed = true;
                        let available = role_config
                            .as_ref()
                            .map(|rc| {
                                rc.roles
                                    .iter()
                                    .map(|r| r.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_else(|| "none".into());
                        let err = format!(
                            "Unknown role: '{}'. Available: {}",
                            target_role, available
                        );
                        let _ = events_tx.send(Ok(AgentEvent::ToolFailed {
                            id: tc.id.clone(),
                            error: err.clone(),
                        }));
                        history.push(ChatMessage::tool_result(&tc.id, &err));
                        displays.push(ToolCallDisplay {
                            id: tc.id.clone(),
                            name: "switch_role".into(),
                            status: ToolStatus::Failed(err),
                            changes: Vec::new(),
                        });
                    }
                }
                continue;
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
                    tool_batch_failed = true;
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
                        tool_batch_failed = true;
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
                    tool_batch_failed = true;
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
                        tool_batch_failed = true;
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
            let execution = tool.execute(args);
            tokio::pin!(execution);
            let mut cancel_open = true;
            let execution = loop {
                tokio::select! {
                    output = &mut execution => break Some(output),
                    changed = cancel_rx.changed(), if cancel_open => {
                        match changed {
                            Ok(()) if *cancel_rx.borrow() => break None,
                            Ok(()) => continue,
                            Err(_) => cancel_open = false,
                        }
                    }
                }
            };
            let Some(execution) = execution else {
                let _ = events_tx.send(Ok(AgentEvent::Cancelled {
                    content: "(cancelled)".into(),
                    tokens_input: total_input,
                    tokens_output: total_output,
                    history,
                }));
                return;
            };
            match execution {
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
                    tool_batch_failed = true;
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

        if let Some(final_answer) =
            finish_task_answer.filter(|_| !tool_batch_failed)
        {
            history.push(ChatMessage::assistant_text(final_answer.clone()));
            tracing::info!(
                step,
                total_input,
                total_output,
                "agent completed via finish_task"
            );
            let _ = events_tx.send(Ok(AgentEvent::Done {
                content: final_answer,
                tokens_input: total_input,
                tokens_output: total_output,
                history,
            }));
            return;
        }

        if let Some((role_idx, task_msg)) = pending_role_switch {
            current_role_idx = role_idx;
            history.push(ChatMessage::role_task(task_msg));
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
            // After a switch_role turn, history ends with a trailing
            // user(task) message. Skip it and keep scanning for tool
            // results — it's a boundary marker, not actual user input.
            Role::User if results.is_empty() => {
                continue;
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

    struct BlockingTool;

    #[async_trait]
    impl Tool for BlockingTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                def_type: "function".into(),
                function: ToolFunctionDef {
                    name: "block".into(),
                    description: "blocking test tool".into(),
                    parameters: serde_json::json!({"type":"object"}),
                },
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<ToolExecutionResult> {
            std::future::pending().await
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
            None,
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

    struct RetryOnceProvider {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl LlmProvider for RetryOnceProvider {
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
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                let _ = sender.send(Err(anyhow::anyhow!(
                    "provider API error (503): temporarily unavailable"
                )));
                return;
            }
            let _ = sender.send(Ok(StreamEvent::Done(ChatResult {
                content: Some("recovered".into()),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: Some("stop".into()),
                reasoning_content: None,
            })));
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
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
            None,
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
            None,
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

    #[tokio::test]
    async fn test_run_loop_retries_transient_pre_output_stream_failure() {
        let calls = Arc::new(Mutex::new(0usize));
        let provider: Arc<dyn LlmProvider> = Arc::new(RetryOnceProvider {
            calls: Arc::clone(&calls),
        });
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_loop(
                provider,
                Arc::new(Vec::new()),
                Vec::new(),
                "hi".into(),
                Vec::new(),
                ChatOptions::default(),
                events_tx,
                cancel_rx,
                perm_rx,
                std::collections::HashMap::new(),
                2,
                "/tmp".into(),
                None,
            ),
        )
        .await
        .expect("retry should complete before the timeout");

        let mut done_content = None;
        while let Some(event) = events_rx.recv().await {
            if let AgentEvent::Done { content, .. } = event.unwrap() {
                done_content = Some(content);
                break;
            }
        }
        assert_eq!(done_content.as_deref(), Some("recovered"));
        assert_eq!(*calls.lock().unwrap(), 2);
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
            None,
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
            None,
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
            None,
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
            None,
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

    #[tokio::test]
    async fn test_run_loop_completes_batched_tools_before_finish_task() {
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            calls: Arc::new(Mutex::new(0)),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [ChatResult {
                    content: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "finish".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: "finish_task".into(),
                                arguments: r#"{"final_answer":"complete"}"#
                                    .into(),
                            },
                        },
                        ToolCall {
                            id: "read".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: "read".into(),
                                arguments: "{}".into(),
                            },
                        },
                    ],
                    usage: Usage::default(),
                    finish_reason: Some("tool_calls".into()),
                    reasoning_content: None,
                }],
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
            None,
        )
        .await;

        let history = loop {
            if let Ok(AgentEvent::Done { history, .. }) =
                events_rx.recv().await.unwrap()
            {
                break history;
            }
        };
        let tool_results: Vec<_> = history
            .iter()
            .filter(|message| matches!(message.role, Role::Tool))
            .collect();
        assert_eq!(tool_results.len(), 2);
        assert_eq!(tool_results[0].tool_call_id.as_deref(), Some("finish"));
        assert_eq!(tool_results[1].tool_call_id.as_deref(), Some("read"));
    }

    #[tokio::test]
    async fn test_run_loop_does_not_finish_when_later_batched_tool_fails() {
        let calls = Arc::new(Mutex::new(0));
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            calls: Arc::clone(&calls),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [
                    ChatResult {
                        content: None,
                        tool_calls: vec![
                            ToolCall {
                                id: "finish".into(),
                                call_type: "function".into(),
                                function: ToolFunction {
                                    name: "finish_task".into(),
                                    arguments:
                                        r#"{"final_answer":"premature"}"#.into(),
                                },
                            },
                            ToolCall {
                                id: "denied".into(),
                                call_type: "function".into(),
                                function: ToolFunction {
                                    name: "read".into(),
                                    arguments: "{}".into(),
                                },
                            },
                        ],
                        usage: Usage::default(),
                        finish_reason: Some("tool_calls".into()),
                        reasoning_content: None,
                    },
                    ChatResult {
                        content: Some("recovered after tool failure".into()),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        finish_reason: Some("stop".into()),
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
            ("read".to_string(), crate::config::Permission::Deny),
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
            None,
        )
        .await;

        let content = loop {
            if let Ok(AgentEvent::Done { content, .. }) =
                events_rx.recv().await.unwrap()
            {
                break content;
            }
        };
        assert_eq!(content, "recovered after tool failure");
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn test_run_loop_defers_role_task_until_all_tool_results() {
        let provider = Arc::new(ScriptedProvider {
            calls: Arc::new(Mutex::new(0)),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from([
                ChatResult {
                    content: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "switch".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: "switch_role".into(),
                                arguments: r#"{"role":"worker","task":"inspect it"}"#.into(),
                            },
                        },
                        ToolCall {
                            id: "read".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: "read".into(),
                                arguments: "{}".into(),
                            },
                        },
                    ],
                    usage: Usage::default(),
                    finish_reason: Some("tool_calls".into()),
                    reasoning_content: None,
                },
                ChatResult {
                    content: Some("done".into()),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    finish_reason: Some("stop".into()),
                    reasoning_content: None,
                },
            ]))),
        });
        let provider_for_loop: Arc<dyn LlmProvider> = provider.clone();
        let role_config = RoleLoopConfig {
            roles: vec![
                ResolvedRole {
                    name: "orchestrator".into(),
                    instructions: String::new(),
                    provider: provider.clone(),
                    model_id: "test".into(),
                    temperature: None,
                    max_tokens: None,
                },
                ResolvedRole {
                    name: "worker".into(),
                    instructions: String::new(),
                    provider,
                    model_id: "test".into(),
                    temperature: None,
                    max_tokens: None,
                },
            ],
        };
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
            provider_for_loop,
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
            Some(role_config),
        )
        .await;

        let history = loop {
            if let Ok(AgentEvent::Done { history, .. }) =
                events_rx.recv().await.unwrap()
            {
                break history;
            }
        };
        let role_task_idx = history
            .iter()
            .position(|message| message.internal_role_task)
            .unwrap();
        let read_result_idx = history
            .iter()
            .position(|message| message.tool_call_id.as_deref() == Some("read"))
            .unwrap();
        assert!(read_result_idx < role_task_idx);
        assert_eq!(
            history[role_task_idx].content.as_deref(),
            Some("inspect it")
        );
    }

    #[tokio::test]
    async fn test_run_loop_cancels_during_active_tool_execution() {
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            calls: Arc::new(Mutex::new(0)),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [ChatResult {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "block".into(),
                        call_type: "function".into(),
                        function: ToolFunction {
                            name: "block".into(),
                            arguments: "{}".into(),
                        },
                    }],
                    usage: Usage::default(),
                    finish_reason: Some("tool_calls".into()),
                    reasoning_content: None,
                }],
            ))),
        });
        let tools: Arc<Vec<Box<dyn Tool>>> =
            Arc::new(vec![Box::new(BlockingTool)]);
        let permissions = std::collections::HashMap::from([(
            "block".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_task = tokio::spawn(run_loop(
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
            None,
        ));

        loop {
            if matches!(
                events_rx.recv().await.unwrap(),
                Ok(AgentEvent::ToolStarted { .. })
            ) {
                break;
            }
        }
        cancel_tx.send(true).unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            events_rx.recv(),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert!(matches!(event, AgentEvent::Cancelled { .. }));
        tokio::time::timeout(std::time::Duration::from_secs(1), loop_task)
            .await
            .unwrap()
            .unwrap();
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
    fn test_role_brief_truncates_multibyte_text_at_character_boundary() {
        let instructions = "你".repeat(81);

        let brief = role_brief(&instructions);

        assert_eq!(brief, format!("{}…", "你".repeat(77)));
        assert_eq!(brief.chars().count(), 78);
    }

    #[test]
    fn test_role_brief_preserves_exactly_eighty_multibyte_characters() {
        let instructions = "你".repeat(80);

        assert_eq!(role_brief(&instructions), instructions);
    }

    #[test]
    fn test_safe_truncate_empty() {
        assert_eq!(safe_truncate("", 0), "");
        assert_eq!(safe_truncate("", 10), "");
    }

    #[test]
    fn test_role_loop_config_new() {
        let config = RoleLoopConfig { roles: vec![] };
        assert_eq!(config.find_role("nonexistent"), None);
        assert_eq!(config.roles.len(), 0);
    }

    #[test]
    fn test_switch_role_tool_def_schema() {
        let def = switch_role_tool_def();
        assert_eq!(def.function.name, "switch_role");
        assert!(
            def.function
                .description
                .contains("Switch to a different role")
        );
        // Verify required fields
        let params = &def.function.parameters;
        assert_eq!(params["type"], "object");
        let required = params["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("role")));
        assert!(required.iter().any(|v| v.as_str() == Some("task")));
    }

    #[test]
    fn test_role_loop_config_find_role() {
        let config = RoleLoopConfig { roles: vec![] };
        assert_eq!(config.find_role("orchestrator"), None);
        assert_eq!(config.find_role("coder"), None);
    }
}
