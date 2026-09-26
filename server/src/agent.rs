use std::sync::Arc;

use std::collections::HashMap;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Permission;

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

/// First retry delay; doubles on each further retry.
const RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
/// Upper bound for the exponential backoff delay.
const RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);
/// Shortest wait before a retry, even when a provider says `Retry-After: 0`,
/// so retries never go out as a burst.
const RETRY_MIN_DELAY: std::time::Duration =
    std::time::Duration::from_millis(250);
/// Longest `Retry-After` honored. A provider asking for longer (e.g. a quota
/// exhausted for hours) fails the run instead of silently stalling it.
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Delay before the `retry`-th retry (1-based) of a failed provider step, or
/// `None` if the failure must not be retried.
fn retry_delay(
    error: &anyhow::Error,
    retry: usize,
) -> Option<std::time::Duration> {
    match error.downcast_ref::<ProviderError>() {
        Some(provider_error) => match provider_error.kind {
            ProviderErrorKind::RateLimited | ProviderErrorKind::Transient => {
                match provider_error.retry_after {
                    Some(wait) if wait > MAX_RETRY_AFTER => None,
                    Some(wait) => Some(wait.max(RETRY_MIN_DELAY)),
                    None => Some(backoff_delay(retry)),
                }
            }
            ProviderErrorKind::ContextOverflow | ProviderErrorKind::Fatal => {
                None
            }
        },
        // Errors not produced by a provider (should be rare): retry only
        // clear network failures, matched by phrase, never by bare digits.
        None => is_network_failure(error).then(|| backoff_delay(retry)),
    }
}

/// Exponential backoff with up to 25% jitter, so parallel runs hitting the
/// same rate limit do not retry in lockstep.
fn backoff_delay(retry: usize) -> std::time::Duration {
    let exponent = retry.saturating_sub(1).min(16) as u32;
    let base = RETRY_BASE_DELAY
        .saturating_mul(1 << exponent)
        .min(RETRY_MAX_DELAY);
    // Sub-second clock noise is plenty for jitter; no RNG dependency needed.
    let noise = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos()) as f64
        / 1e9;
    base.mul_f64(1.0 + 0.25 * noise).min(RETRY_MAX_DELAY)
}

fn is_network_failure(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    [
        "connection refused",
        "connection reset",
        "connection closed",
        "socket hang up",
        "timed out",
        "network error",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

/// Routes user permission responses to whichever agent loop is waiting.
///
/// A parent run and its subagents share one broker: while a subagent waits on
/// a prompt, the parent loop is blocked inside the `subagent` tool and could
/// not forward a response itself. Each waiter registers a one-shot channel
/// under a unique permission id; the websocket handler resolves it by id.
#[derive(Clone, Default)]
pub struct PermissionBroker {
    pending: Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                tokio::sync::oneshot::Sender<bool>,
            >,
        >,
    >,
    /// Prepended to permission ids so subagent ids cannot collide with the
    /// parent's (providers may reuse tool-call ids such as `call_0`).
    id_prefix: String,
    /// Shared sequence number making every permission id unique within the
    /// run, so a late answer to an old prompt can never match a new one.
    next_seq: Arc<std::sync::atomic::AtomicU64>,
}

impl PermissionBroker {
    /// A broker sharing this one's pending map, with ids prefixed by `prefix`.
    pub fn scoped(&self, prefix: &str) -> Self {
        Self {
            pending: Arc::clone(&self.pending),
            id_prefix: format!("{}{prefix}", self.id_prefix),
            next_seq: Arc::clone(&self.next_seq),
        }
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>,
    > {
        // The map stays consistent even if a holder panicked, so recover
        // from poisoning instead of propagating the panic.
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register a wait for the tool call `tool_call_id`. Returns the
    /// permission id to send to the client and the receiver for the answer.
    pub fn register(
        &self,
        tool_call_id: &str,
    ) -> (String, tokio::sync::oneshot::Receiver<bool>) {
        let seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("{}perm_{seq}_{tool_call_id}", self.id_prefix);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.lock().insert(id.clone(), tx);
        (id, rx)
    }

    /// Deliver the user's answer. Returns false if nobody is waiting on `id`.
    pub fn resolve(&self, id: &str, allowed: bool) -> bool {
        self.lock()
            .remove(id)
            .is_some_and(|tx| tx.send(allowed).is_ok())
    }

    /// Drop a wait that will never be answered (e.g. on cancellation).
    pub fn forget(&self, id: &str) {
        self.lock().remove(id);
    }
}

/// Extract a readable message from a caught panic payload.
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
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
    tools: &ToolRegistry,
    permissions: &HashMap<String, Permission>,
) -> Vec<ToolDef> {
    tools
        .defs()
        .iter()
        .filter(|def| {
            permissions
                .get(&def.function.name)
                .copied()
                .unwrap_or(Permission::Ask)
                != Permission::Deny
        })
        .cloned()
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
    /// The wait for permission `id` ended (answered, or the responder went
    /// away). Internal bookkeeping only; never sent to clients. Emitted
    /// before the approved tool runs, so observers can tell user wait time
    /// apart from tool run time.
    PermissionResolved { id: String },
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
    /// A subagent's provider request is being retried; with
    /// `discarded_output`, its partial output for the current turn is void.
    SubagentRetrying {
        id: String,
        reason: String,
        discarded_output: bool,
    },
    /// A subagent has finished.
    SubagentDone { id: String, content: String },
    /// A provider step failed and will be retried after `delay`. When
    /// `discarded_output` is set, text already streamed for this turn is
    /// void: the retry regenerates the whole turn.
    Retrying {
        attempt: usize,
        max_attempts: usize,
        delay: std::time::Duration,
        reason: String,
        discarded_output: bool,
    },
    /// The run failed with an unrecoverable error (e.g. a provider error
    /// that cannot be retried). Carries the history so far so it can be
    /// saved.
    Failed {
        error: anyhow::Error,
        tokens_input: u64,
        tokens_output: u64,
        history: Vec<ChatMessage>,
    },
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

/// Static inputs of one agent run.
pub struct RunConfig {
    pub provider: Arc<dyn LlmProvider>,
    pub tools: Arc<ToolRegistry>,
    /// Assembled system prompt layers (see `PromptAssembler`).
    pub system_layers: Vec<String>,
    pub options: ChatOptions,
    /// Pre-resolved permission map: tool name → permission.
    pub permissions: HashMap<String, Permission>,
    /// Normal tool-capable turn budget; 0 means `DEFAULT_MAX_STEPS`.
    pub max_steps: usize,
    /// Workspace shown in the per-turn system reminder.
    pub working_directory: String,
    /// Role-aware loop config. When None, runs in single-role mode.
    pub role_config: Option<RoleLoopConfig>,
}

/// Channels connecting a run to whoever owns it.
pub struct RunChannels {
    pub events_tx: UnboundedSender<Result<AgentEvent>>,
    pub cancel_rx: tokio::sync::watch::Receiver<bool>,
    pub permission_broker: PermissionBroker,
}

/// Run the agent loop.
///
/// Appends `user_message` to `history`, then alternates LLM turns and tool
/// execution until the task finishes, is cancelled, fails, or runs out of
/// steps. Streams `AgentEvent`s through `channels.events_tx`; the last event
/// is always exactly one terminal event carrying the final history.
pub async fn run_loop(
    config: RunConfig,
    channels: RunChannels,
    user_message: String,
    history: Vec<ChatMessage>,
) {
    AgentRun::new(config, channels, history)
        .run(user_message)
        .await;
}

/// How a run ends. [`AgentRun::finish`] turns it into the terminal event.
enum Terminal {
    Done(String),
    Cancelled,
    NeedsContinuation(String),
    Failed(anyhow::Error),
}

/// Everything one step sends to the provider.
struct StepRequest {
    provider: Arc<dyn LlmProvider>,
    messages: Vec<ChatMessage>,
    options: ChatOptions,
}

/// A completed provider response plus the text streamed while producing it.
struct StepResponse {
    result: ChatResult,
    streamed_text: String,
}

/// Outcome of one tool call within a batch.
struct ToolOutcome {
    display: ToolCallDisplay,
    failed: bool,
    finish_answer: Option<String>,
    role_switch: Option<(usize, String)>,
}

impl ToolOutcome {
    fn succeeded(tc: &ToolCall, name: &str, changes: Vec<FileChange>) -> Self {
        Self {
            display: ToolCallDisplay {
                id: tc.id.clone(),
                name: name.into(),
                status: ToolStatus::Success,
                changes,
            },
            failed: false,
            finish_answer: None,
            role_switch: None,
        }
    }
}

/// Combined outcome of all tool calls from one assistant message.
#[derive(Default)]
struct ToolBatch {
    displays: Vec<ToolCallDisplay>,
    /// True if any call in the batch failed.
    failed: bool,
    /// The first `finish_task` answer in the batch.
    finish_answer: Option<String>,
    /// The last valid `switch_role` request in the batch.
    role_switch: Option<(usize, String)>,
}

/// Wait until cancellation is requested. Never resolves once the cancel
/// sender is gone (`changed()` would then resolve immediately forever, so it
/// must stop being polled). Cancel-safe, for use in `select!`.
async fn wait_for_cancel(
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    cancel_open: &mut bool,
) {
    loop {
        if !*cancel_open {
            return std::future::pending().await;
        }
        match cancel_rx.changed().await {
            Ok(()) if *cancel_rx.borrow() => return,
            Ok(()) => {}
            Err(_) => *cancel_open = false,
        }
    }
}

/// Skill names advertised in the "Skills available:" system layer.
fn skill_names(system_layers: &[String]) -> Vec<String> {
    system_layers
        .iter()
        .filter(|layer| layer.starts_with("Skills available:"))
        .flat_map(|layer| {
            // Extract skill names from lines like "  name — desc"
            layer.lines().skip(1).filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || !line.contains(" — ") {
                    return None;
                }
                line.split(" — ").next().map(|s| s.to_string())
            })
        })
        .collect()
}

/// State of one running agent loop.
struct AgentRun {
    cfg: RunConfig,
    events_tx: UnboundedSender<Result<AgentEvent>>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    permission_broker: PermissionBroker,
    history: Vec<ChatMessage>,
    total_input: u64,
    total_output: u64,
    /// False once the cancel sender is dropped (see [`wait_for_cancel`]).
    cancel_open: bool,
    /// Active role in role mode; stays 0 in single-role mode.
    current_role_idx: usize,
}

impl AgentRun {
    fn new(
        mut cfg: RunConfig,
        channels: RunChannels,
        history: Vec<ChatMessage>,
    ) -> Self {
        if cfg.max_steps == 0 {
            cfg.max_steps = DEFAULT_MAX_STEPS;
        }
        Self {
            cfg,
            events_tx: channels.events_tx,
            cancel_rx: channels.cancel_rx,
            permission_broker: channels.permission_broker,
            history,
            total_input: 0,
            total_output: 0,
            cancel_open: true,
            current_role_idx: 0,
        }
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.events_tx.send(Ok(event));
    }

    fn cancel_requested(&self) -> bool {
        *self.cancel_rx.borrow()
    }

    /// The single exit path: send the terminal event with the final history.
    fn finish(self, terminal: Terminal) {
        let AgentRun {
            events_tx,
            history,
            total_input: tokens_input,
            total_output: tokens_output,
            ..
        } = self;
        let event = match terminal {
            Terminal::Done(content) => AgentEvent::Done {
                content,
                tokens_input,
                tokens_output,
                history,
            },
            Terminal::Cancelled => AgentEvent::Cancelled {
                content: "(cancelled)".into(),
                tokens_input,
                tokens_output,
                history,
            },
            Terminal::NeedsContinuation(content) => {
                AgentEvent::NeedsContinuation {
                    content,
                    tokens_input,
                    tokens_output,
                    history,
                }
            }
            Terminal::Failed(error) => AgentEvent::Failed {
                error,
                tokens_input,
                tokens_output,
                history,
            },
        };
        let _ = events_tx.send(Ok(event));
    }

    async fn run(mut self, user_message: String) {
        self.history.push(ChatMessage::user(&user_message));

        let skill_names = skill_names(&self.cfg.system_layers);
        if !skill_names.is_empty() {
            self.emit(AgentEvent::SkillsLoaded { names: skill_names });
        }

        let max_steps = self.cfg.max_steps;
        // Hard turn budget fallback: after max_steps normal turns, the next
        // turn is a soft, text-only finalization step. If the model still
        // fails to produce a terminal text response there, stop before
        // exceeding the fallback budget.
        let soft_final_step = max_steps.saturating_add(1);
        let mut step = 0usize;
        loop {
            step += 1;
            tracing::debug!(
                "agent turn {} (soft budget {}): {} tools, {} history messages",
                step,
                max_steps,
                self.cfg.tools.defs().len(),
                self.history.len()
            );

            if self.cancel_requested() {
                return self.finish(Terminal::Cancelled);
            }
            if step > soft_final_step {
                return self.finish(Terminal::NeedsContinuation(
                    "(max steps reached)".into(),
                ));
            }
            let final_text_only_step = step == soft_final_step;

            let request = self.build_step_request(step, final_text_only_step);
            let response = match self.stream_step(step, &request).await {
                Ok(response) => response,
                Err(terminal) => return self.finish(terminal),
            };
            if let Some(terminal) = self
                .handle_response(
                    step,
                    final_text_only_step,
                    &request.options,
                    response,
                )
                .await
            {
                return self.finish(terminal);
            }
        }
    }

    /// Assemble the messages, options, and provider for one step.
    fn build_step_request(
        &self,
        step: usize,
        final_text_only_step: bool,
    ) -> StepRequest {
        let system_layers = &self.cfg.system_layers;
        let role_config = self.cfg.role_config.as_ref();

        // Build messages: system + reminder + history
        let mut messages: Vec<ChatMessage> =
            Vec::with_capacity(system_layers.len() + 2 + self.history.len());

        // Identify the skills layer (layer 6) so we can inject role layers (5)
        // before it: correct order is shared (1-4), role (5), skills (6), reminder (7).
        let has_skills = system_layers
            .last()
            .is_some_and(|l| l.starts_with("Skills available:"));
        let pre_skills_count = if has_skills {
            system_layers.len().saturating_sub(1)
        } else {
            system_layers.len()
        };

        // Inject shared layers (1-4): env, system prompt, global AGENTS.md,
        // workspace AGENTS.md
        for layer in &system_layers[..pre_skills_count] {
            messages.push(ChatMessage::system(layer));
        }

        // Inject role layers (5) — only in role mode
        if let Some(rc) = role_config {
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
            let role_instructions =
                &rc.roles[self.current_role_idx].instructions;
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
        let mut tool_defs =
            advertised_tool_defs(&self.cfg.tools, &self.cfg.permissions);
        // When in role mode, inject the switch_role tool def so the model can
        // delegate to other roles. This is an internal loop tool, not a filesystem tool.
        if role_config.is_some() && !final_text_only_step {
            tool_defs.push(switch_role_tool_def());
        }
        let reminder_ctx = crate::prompt::ReminderContext {
            step,
            max_steps: self.cfg.max_steps,
            working_directory: self.cfg.working_directory.clone(),
            tool_defs: &tool_defs,
            last_turn_results: extract_last_turn_results(&self.history),
            last_user_message: extract_last_user_message(&self.history),
        };
        let reminder = crate::prompt::build_system_reminder(&reminder_ctx);
        messages.push(ChatMessage::system(&reminder));

        messages.extend(self.history.iter().cloned());
        if final_text_only_step {
            messages.push(ChatMessage::assistant_text(MAX_STEPS_PROMPT));
        }

        let base = &self.cfg.options;
        let (mut options, provider) = match role_config {
            Some(rc) => {
                let role = &rc.roles[self.current_role_idx];
                (
                    ChatOptions {
                        model_id: role.model_id.clone(),
                        temperature: role
                            .temperature
                            .unwrap_or(base.temperature),
                        max_tokens: role.max_tokens.unwrap_or(base.max_tokens),
                        tools: Vec::new(), // populated below
                    },
                    Arc::clone(&role.provider),
                )
            }
            None => (base.clone(), Arc::clone(&self.cfg.provider)),
        };
        options.tools = if final_text_only_step {
            Vec::new()
        } else {
            tool_defs
        };

        StepRequest {
            provider,
            messages,
            options,
        }
    }

    /// Stream one provider response, forwarding deltas as events. Transient
    /// failures before any output are retried with backoff.
    async fn stream_step(
        &mut self,
        step: usize,
        request: &StepRequest,
    ) -> std::result::Result<StepResponse, Terminal> {
        let mut text_buf = String::new();
        let mut emitted_stream_data = false;
        let mut retries = 0;
        'stream_attempt: loop {
            // Each retry gets a fresh channel and task. Retrying only before any
            // streamed output avoids duplicating visible assistant text.
            let (stream_tx, mut stream_rx) =
                tokio::sync::mpsc::unbounded_channel();
            let provider = Arc::clone(&request.provider);
            let attempt_messages = request.messages.clone();
            let attempt_opts = request.options.clone();
            let stream_handle = tokio::spawn(async move {
                provider
                    .chat_stream(&attempt_messages, &attempt_opts, stream_tx)
                    .await;
            });

            loop {
                let event = tokio::select! {
                    event = stream_rx.recv() => event,
                    _ = wait_for_cancel(&mut self.cancel_rx, &mut self.cancel_open) => {
                        reap_stream_task(stream_handle, true);
                        return Err(Terminal::Cancelled);
                    }
                };
                let Some(event) = event else {
                    reap_stream_task(stream_handle, false);
                    if self.cancel_requested() {
                        return Err(Terminal::Cancelled);
                    }
                    tracing::warn!(
                        step,
                        streamed_chars = text_buf.len(),
                        "agent stream closed without a completion result"
                    );
                    return Err(Terminal::NeedsContinuation(text_buf));
                };
                match event {
                    Ok(StreamEvent::Chunk(text)) => {
                        emitted_stream_data = true;
                        text_buf.push_str(&text);
                        self.emit(AgentEvent::TextDelta(text));
                    }
                    Ok(StreamEvent::ReasoningChunk(text)) => {
                        emitted_stream_data = true;
                        self.emit(AgentEvent::ReasoningDelta(text));
                    }
                    Ok(StreamEvent::Done(result)) => {
                        reap_stream_task(stream_handle, true);
                        return Ok(StepResponse {
                            result,
                            streamed_text: text_buf,
                        });
                    }
                    Err(error) => {
                        reap_stream_task(stream_handle, true);
                        let delay = if retries < MAX_STREAM_RETRIES {
                            retry_delay(&error, retries + 1)
                        } else {
                            None
                        };
                        let Some(delay) = delay else {
                            return Err(Terminal::Failed(error));
                        };
                        retries += 1;
                        // Output already shown for this attempt is dropped:
                        // the retry regenerates the whole turn, and the
                        // client discards its partial copy on `Retrying`.
                        let discarded_output = emitted_stream_data;
                        text_buf.clear();
                        emitted_stream_data = false;
                        let reason = format!("{error:#}");
                        tracing::warn!(
                            step,
                            retry = retries,
                            max_retries = MAX_STREAM_RETRIES,
                            retry_delay_ms = delay.as_millis(),
                            status = error
                                .downcast_ref::<ProviderError>()
                                .and_then(|e| e.status),
                            discarded_output,
                            "retrying failed agent stream: {reason}"
                        );
                        self.emit(AgentEvent::Retrying {
                            attempt: retries,
                            max_attempts: MAX_STREAM_RETRIES,
                            delay,
                            reason: safe_truncate(&reason, 300).to_string(),
                            discarded_output,
                        });
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = wait_for_cancel(&mut self.cancel_rx, &mut self.cancel_open) => {
                                return Err(Terminal::Cancelled);
                            }
                        }
                        continue 'stream_attempt;
                    }
                }
            }
        }
    }

    /// Record the assistant turn and run its tool calls. Returns the
    /// terminal state if the run ends with this step.
    async fn handle_response(
        &mut self,
        step: usize,
        final_text_only_step: bool,
        options: &ChatOptions,
        response: StepResponse,
    ) -> Option<Terminal> {
        let StepResponse {
            result,
            streamed_text,
        } = response;
        let model_id = &options.model_id;
        let max_tokens = options.max_tokens;

        // Accumulate token usage
        self.total_input += result.usage.prompt_tokens;
        self.total_output += result.usage.completion_tokens;
        let (total_input, total_output) = (self.total_input, self.total_output);
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
            let content = assistant_result_text(&result, &streamed_text);
            self.history
                .push(ChatMessage::assistant_text(content.clone()));
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
                return Some(Terminal::Done(content));
            }
            if final_text_only_step {
                tracing::warn!(
                    step,
                    finish_reason = ?result.finish_reason,
                    total_input,
                    total_output,
                    "agent reached its final text-only step without a terminal finish reason"
                );
                return Some(Terminal::NeedsContinuation(content));
            }
            self.emit(AgentEvent::TurnDone {
                text: content,
                tool_calls: Vec::new(),
            });
            return None;
        }

        if final_text_only_step {
            let content = assistant_result_text(&result, &streamed_text);
            self.history
                .push(ChatMessage::assistant_text(content.clone()));
            tracing::warn!(
                step,
                tool_calls = result.tool_calls.len(),
                total_input,
                total_output,
                "agent requested tools during its final text-only step"
            );
            return Some(Terminal::NeedsContinuation(if content.is_empty() {
                "(max steps reached)".into()
            } else {
                content
            }));
        }

        // There are tool calls — add the assistant message to history (with reasoning content)
        tracing::debug!("→ {} tool call(s) from LLM", result.tool_calls.len());
        for tc in &result.tool_calls {
            tracing::debug!(
                "  tool: {} (args: {})",
                tc.function.name,
                safe_truncate(&tc.function.arguments, 120)
            );
        }
        let turn_text = assistant_result_text(&result, &streamed_text);
        let ChatResult {
            content,
            tool_calls,
            reasoning_content,
            ..
        } = result;
        let mut msg = ChatMessage::assistant_tool_calls_with_content(
            tool_calls.clone(),
            (!turn_text.is_empty()).then_some(turn_text.clone()),
        );
        msg.reasoning_content = reasoning_content;
        self.history.push(msg);

        let batch = match self.execute_tool_batch(&tool_calls, &content).await {
            Ok(batch) => batch,
            Err(terminal) => return Some(terminal),
        };

        if let Some(final_answer) =
            batch.finish_answer.filter(|_| !batch.failed)
        {
            self.history
                .push(ChatMessage::assistant_text(final_answer.clone()));
            tracing::info!(
                step,
                total_input,
                total_output,
                "agent completed via finish_task"
            );
            return Some(Terminal::Done(final_answer));
        }

        if let Some((role_idx, task_msg)) = batch.role_switch {
            self.current_role_idx = role_idx;
            self.history.push(ChatMessage::role_task(task_msg));
        }

        // History now contains tool results — the loop continues.
        self.emit(AgentEvent::TurnDone {
            text: turn_text,
            tool_calls: batch.displays,
        });
        None
    }

    /// Run every tool call of one assistant message, in order.
    async fn execute_tool_batch(
        &mut self,
        tool_calls: &[ToolCall],
        content: &Option<String>,
    ) -> std::result::Result<ToolBatch, Terminal> {
        let mut batch = ToolBatch::default();
        for tc in tool_calls {
            if self.cancel_requested() {
                return Err(Terminal::Cancelled);
            }
            let outcome = match tc.function.name.as_str() {
                "finish_task" => self.finish_task_call(tc, content),
                "switch_role" => self.switch_role_call(tc),
                _ => self.regular_tool_call(tc).await?,
            };
            batch.failed |= outcome.failed;
            if let Some(answer) = outcome.finish_answer {
                batch.finish_answer.get_or_insert(answer);
            }
            if let Some(switch) = outcome.role_switch {
                // Applied after every result from this assistant message is
                // recorded; if several switches are emitted, the last valid
                // one wins because it is the last requested transition.
                batch.role_switch = Some(switch);
            }
            batch.displays.push(outcome.display);
        }
        Ok(batch)
    }

    /// Report a failed tool call to the client and to the model.
    fn record_tool_failure(
        &mut self,
        tc: &ToolCall,
        error: String,
        history_text: String,
    ) -> ToolOutcome {
        self.emit(AgentEvent::ToolFailed {
            id: tc.id.clone(),
            error: error.clone(),
        });
        self.history
            .push(ChatMessage::tool_result(&tc.id, history_text));
        ToolOutcome {
            display: ToolCallDisplay {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                status: ToolStatus::Failed(error),
                changes: Vec::new(),
            },
            failed: true,
            finish_answer: None,
            role_switch: None,
        }
    }

    /// `finish_task` is an internal completion marker handled by the loop.
    fn finish_task_call(
        &mut self,
        tc: &ToolCall,
        content: &Option<String>,
    ) -> ToolOutcome {
        let final_answer =
            serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                .ok()
                .and_then(|args| {
                    args.get("final_answer")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .or_else(|| content.clone())
                .unwrap_or_else(|| "(task finished)".to_string());
        self.history.push(ChatMessage::tool_result(
            &tc.id,
            "Task completion acknowledged.",
        ));
        ToolOutcome {
            finish_answer: Some(final_answer),
            ..ToolOutcome::succeeded(tc, "finish_task", Vec::new())
        }
    }

    /// `switch_role` is a loop-owned tool in role mode.
    fn switch_role_call(&mut self, tc: &ToolCall) -> ToolOutcome {
        self.emit(AgentEvent::ToolStarted {
            id: tc.id.clone(),
            name: "switch_role".into(),
        });

        let args: serde_json::Value =
            match serde_json::from_str(&tc.function.arguments) {
                Ok(v) => v,
                Err(e) => {
                    let err = format!("Invalid switch_role arguments: {e}");
                    return self.record_tool_failure(tc, err.clone(), err);
                }
            };
        let target_role =
            args.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");

        let role_config = self.cfg.role_config.as_ref();
        let Some(idx) = role_config.and_then(|rc| rc.find_role(target_role))
        else {
            let available = role_config
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
            return self.record_tool_failure(tc, err.clone(), err);
        };

        // Valid role — switch and inject task
        let task_msg = if task.is_empty() {
            format!("Switched to role: {}", target_role)
        } else {
            task.to_string()
        };
        let result_text = format!("Switched to role: {}", target_role);
        // Push tool result first — providers expect tool results to
        // immediately follow the assistant tool_call.
        self.history
            .push(ChatMessage::tool_result(&tc.id, &result_text));
        self.emit(AgentEvent::ToolCompleted {
            id: tc.id.clone(),
            name: "switch_role".into(),
            result: result_text,
            changes: Vec::new(),
            rollback_entries: Vec::new(),
        });
        ToolOutcome {
            role_switch: Some((idx, task_msg)),
            ..ToolOutcome::succeeded(tc, "switch_role", Vec::new())
        }
    }

    /// A registry tool: look up, parse arguments, check permission, execute.
    async fn regular_tool_call(
        &mut self,
        tc: &ToolCall,
    ) -> std::result::Result<ToolOutcome, Terminal> {
        let name = tc.function.name.as_str();

        // Emit skill selected event when use_skill is called
        if name == "use_skill" {
            if let Some(skill) = serde_json::from_str::<serde_json::Value>(
                &tc.function.arguments,
            )
            .ok()
            .as_ref()
            .and_then(|args| args.get("skill_name"))
            .and_then(|v| v.as_str())
            {
                self.emit(AgentEvent::SkillSelected {
                    name: skill.to_string(),
                });
                // Also send as reasoning so the TUI shows grey thinking text
                self.emit(AgentEvent::ReasoningDelta(format!(
                    "[Skill selected: {}]",
                    skill
                )));
            }
        }

        self.emit(AgentEvent::ToolStarted {
            id: tc.id.clone(),
            name: name.to_string(),
        });

        if !self.cfg.tools.contains(name) {
            let err = format!("Unknown tool: {}", name);
            return Ok(self.record_tool_failure(
                tc,
                err.clone(),
                format!("Error: {}", err),
            ));
        }

        let args: serde_json::Value =
            match serde_json::from_str(&tc.function.arguments) {
                Ok(v) => v,
                Err(e) => {
                    let err = format!("Failed to parse arguments: {}", e);
                    return Ok(self.record_tool_failure(
                        tc,
                        err.clone(),
                        format!("Error: {}", err),
                    ));
                }
            };

        if let Some(err) = self.check_permission(tc, &args).await? {
            return Ok(self.record_tool_failure(
                tc,
                err.clone(),
                format!("Error: {}", err),
            ));
        }

        let tools = Arc::clone(&self.cfg.tools);
        let execution = tokio::select! {
            output = tools.execute(name, args) => output,
            _ = wait_for_cancel(&mut self.cancel_rx, &mut self.cancel_open) => {
                return Err(Terminal::Cancelled);
            }
        };
        match execution {
            Ok(output) => {
                self.emit(AgentEvent::ToolCompleted {
                    id: tc.id.clone(),
                    name: name.to_string(),
                    result: output.output.clone(),
                    changes: output.changes.clone(),
                    rollback_entries: output.rollback_entries.clone(),
                });
                self.history
                    .push(ChatMessage::tool_result(&tc.id, &output.output));
                Ok(ToolOutcome::succeeded(tc, name, output.changes))
            }
            Err(e) => {
                let err = format!("{:#}", e);
                Ok(self.record_tool_failure(
                    tc,
                    err.clone(),
                    format!("Error: {}", err),
                ))
            }
        }
    }

    /// Apply the tool's permission, asking the user when needed. Returns the
    /// denial message if the call may not run.
    async fn check_permission(
        &mut self,
        tc: &ToolCall,
        args: &serde_json::Value,
    ) -> std::result::Result<Option<String>, Terminal> {
        let name = &tc.function.name;
        let perm = self
            .cfg
            .permissions
            .get(name)
            .copied()
            .unwrap_or(Permission::Ask);
        match perm {
            Permission::Allow => Ok(None),
            Permission::Deny => Ok(Some(format!(
                "Permission denied: '{}' is not allowed for this agent",
                name
            ))),
            Permission::Ask => {
                let (perm_id, response_rx) =
                    self.permission_broker.register(&tc.id);
                self.emit(AgentEvent::PermissionRequest {
                    id: perm_id.clone(),
                    tool_name: name.clone(),
                    args: args.clone(),
                });
                // A dropped responder (broker entry replaced or cleared)
                // counts as a denial.
                let allowed = tokio::select! {
                    response = response_rx => response.unwrap_or(false),
                    _ = wait_for_cancel(&mut self.cancel_rx, &mut self.cancel_open) => {
                        self.permission_broker.forget(&perm_id);
                        return Err(Terminal::Cancelled);
                    }
                };
                self.emit(AgentEvent::PermissionResolved { id: perm_id });
                Ok((!allowed).then(|| {
                    format!("Permission denied by user for tool '{}'", name)
                }))
            }
        }
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

    struct PanickingTool;

    #[async_trait]
    impl Tool for PanickingTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                def_type: "function".into(),
                function: ToolFunctionDef {
                    name: "boom".into(),
                    description: "panicking test tool".into(),
                    parameters: serde_json::json!({"type":"object"}),
                },
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<ToolExecutionResult> {
            panic!("tool exploded");
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
        let tools = ToolRegistry::new(vec![
            Box::new(NamedTool("read")),
            Box::new(NamedTool("bash")),
        ]);
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
        let tools: Arc<ToolRegistry> = Arc::new(ToolRegistry::new(vec![
            Box::new(NamedTool("read")),
            Box::new(NamedTool("bash")),
            Box::new(NamedTool("finish_task")),
        ]));
        let permissions = std::collections::HashMap::from([
            ("read".to_string(), crate::config::Permission::Allow),
            ("bash".to_string(), crate::config::Permission::Deny),
            ("finish_task".to_string(), crate::config::Permission::Allow),
        ]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 2,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hello".into(),
            Vec::new(),
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

    /// Always fails with a non-retryable provider error.
    struct FailingProvider;

    #[async_trait]
    impl LlmProvider for FailingProvider {
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
            let _ = sender.send(Err(anyhow::anyhow!(
                "provider API error (401): invalid api key"
            )));
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                def_type: "function".into(),
                function: ToolFunctionDef {
                    name: "slow".into(),
                    description: "slow test tool".into(),
                    parameters: serde_json::json!({"type":"object"}),
                },
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<ToolExecutionResult> {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(ToolExecutionResult {
                output: "slow done".into(),
                changes: Vec::new(),
                rollback_entries: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn test_dropping_cancel_sender_mid_tool_lets_run_finish() {
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(SlowTool)]));
        let permissions = std::collections::HashMap::from([(
            "slow".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let loop_task = tokio::spawn(run_loop(
            RunConfig {
                provider: single_tool_call_then_stop("slow"),
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: PermissionBroker::default(),
            },
            "hi".into(),
            Vec::new(),
        ));
        loop {
            if matches!(
                events_rx.recv().await.unwrap(),
                Ok(AgentEvent::ToolStarted { .. })
            ) {
                break;
            }
        }
        // No cancellation can arrive any more; the run must neither treat
        // this as a cancel nor spin, and must complete normally.
        drop(cancel_tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), loop_task)
            .await
            .expect("run must finish")
            .unwrap();
        let mut saw_completed = false;
        let mut last = None;
        while let Ok(event) = events_rx.try_recv() {
            let event = event.unwrap();
            if matches!(&event, AgentEvent::ToolCompleted { result, .. } if result == "slow done")
            {
                saw_completed = true;
            }
            last = Some(event);
        }
        assert!(saw_completed);
        assert!(matches!(last, Some(AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn test_run_loop_ends_with_failed_on_non_retryable_error() {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        run_loop(
            RunConfig {
                provider: Arc::new(FailingProvider),
                tools: Arc::new(ToolRegistry::new(Vec::new())),
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions: std::collections::HashMap::new(),
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: PermissionBroker::default(),
            },
            "hi".into(),
            Vec::new(),
        )
        .await;
        let mut last = None;
        while let Ok(event) = events_rx.try_recv() {
            last = Some(event.unwrap());
        }
        match last {
            Some(AgentEvent::Failed { error, history, .. }) => {
                assert!(format!("{error:#}").contains("invalid api key"));
                // History up to the failure travels with it so it can be saved.
                assert_eq!(
                    history.last().and_then(|m| m.content.as_deref()),
                    Some("hi")
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    type AttemptScript =
        Vec<Box<dyn Fn() -> Result<StreamEvent> + Send + Sync>>;

    /// Replays one scripted list of stream events per attempt.
    struct StreamScriptProvider {
        calls: Arc<Mutex<usize>>,
        attempts: Mutex<std::collections::VecDeque<AttemptScript>>,
        /// Script reused once `attempts` runs out.
        repeat: Option<AttemptScript>,
    }

    #[async_trait]
    impl LlmProvider for StreamScriptProvider {
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
            let script = self.attempts.lock().unwrap().pop_front();
            let script = script.as_ref().or(self.repeat.as_ref());
            for event in script.expect("attempt script missing") {
                let _ = sender.send(event());
            }
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    fn provider_error(
        kind: ProviderErrorKind,
        retry_after_ms: Option<u64>,
    ) -> anyhow::Error {
        let mut error = ProviderError::new(kind, format!("{kind:?} failure"));
        error.retry_after =
            retry_after_ms.map(std::time::Duration::from_millis);
        error.into()
    }

    fn done_with(text: &'static str) -> Result<StreamEvent> {
        Ok(StreamEvent::Done(ChatResult {
            content: Some(text.into()),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: Some("stop".into()),
            reasoning_content: None,
        }))
    }

    /// Run a tool-less loop against `provider` and collect every event.
    async fn run_scripted(provider: StreamScriptProvider) -> Vec<AgentEvent> {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_loop(
                RunConfig {
                    provider: Arc::new(provider),
                    tools: Arc::new(ToolRegistry::new(Vec::new())),
                    system_layers: Vec::new(),
                    options: ChatOptions::default(),
                    permissions: std::collections::HashMap::new(),
                    max_steps: 5,
                    working_directory: "/tmp".into(),
                    role_config: None,
                },
                RunChannels {
                    events_tx,
                    cancel_rx,
                    permission_broker: PermissionBroker::default(),
                },
                "hi".into(),
                Vec::new(),
            ),
        )
        .await
        .expect("run must finish");
        let mut events = Vec::new();
        while let Ok(event) = events_rx.try_recv() {
            events.push(event.unwrap());
        }
        events
    }

    #[tokio::test]
    async fn test_retry_after_partial_output_discards_it_and_regenerates() {
        let calls = Arc::new(Mutex::new(0));
        let attempts: Vec<AttemptScript> = vec![
            vec![
                Box::new(|| Ok(StreamEvent::Chunk("partial ".into()))),
                Box::new(|| {
                    Err(provider_error(ProviderErrorKind::Transient, Some(5)))
                }),
            ],
            vec![
                Box::new(|| Ok(StreamEvent::Chunk("full answer".into()))),
                Box::new(|| done_with("full answer")),
            ],
        ];
        let events = run_scripted(StreamScriptProvider {
            calls: Arc::clone(&calls),
            attempts: Mutex::new(attempts.into()),
            repeat: None,
        })
        .await;
        assert_eq!(*calls.lock().unwrap(), 2);
        let retry = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    AgentEvent::Retrying {
                        attempt: 1,
                        max_attempts: MAX_STREAM_RETRIES,
                        discarded_output: true,
                        ..
                    }
                )
            })
            .expect("a Retrying event");
        assert!(
            matches!(&events[retry - 1], AgentEvent::TextDelta(t) if t == "partial ")
        );
        assert!(
            matches!(&events[retry + 1], AgentEvent::TextDelta(t) if t == "full answer")
        );
        match events.last() {
            Some(AgentEvent::Done {
                content, history, ..
            }) => {
                assert_eq!(content, "full answer");
                assert!(!history.iter().any(|m| {
                    m.content.as_deref().is_some_and(|c| c.contains("partial"))
                }));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_context_overflow_and_long_retry_after_are_not_retried() {
        for (kind, retry_after_ms) in [
            (ProviderErrorKind::ContextOverflow, None),
            (ProviderErrorKind::Fatal, None),
            // Asking for longer than MAX_RETRY_AFTER fails fast.
            (ProviderErrorKind::RateLimited, Some(120_000)),
        ] {
            let calls = Arc::new(Mutex::new(0));
            let events = run_scripted(StreamScriptProvider {
                calls: Arc::clone(&calls),
                attempts: Mutex::new(Default::default()),
                repeat: Some(vec![Box::new(move || {
                    Err(provider_error(kind, retry_after_ms))
                })]),
            })
            .await;
            assert_eq!(*calls.lock().unwrap(), 1, "{kind:?}");
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, AgentEvent::Retrying { .. })),
                "{kind:?}"
            );
            assert!(matches!(events.last(), Some(AgentEvent::Failed { .. })));
        }
    }

    #[tokio::test]
    async fn test_retries_stop_after_max_attempts() {
        let calls = Arc::new(Mutex::new(0));
        let events = run_scripted(StreamScriptProvider {
            calls: Arc::clone(&calls),
            attempts: Mutex::new(Default::default()),
            repeat: Some(vec![Box::new(|| {
                Err(provider_error(ProviderErrorKind::RateLimited, Some(1)))
            })]),
        })
        .await;
        assert_eq!(*calls.lock().unwrap(), MAX_STREAM_RETRIES + 1);
        let retries = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Retrying { .. }))
            .count();
        assert_eq!(retries, MAX_STREAM_RETRIES);
        assert!(matches!(events.last(), Some(AgentEvent::Failed { .. })));
    }

    #[test]
    fn test_retry_delay_policy() {
        // Untyped errors: retried only for clear network failures, never
        // because of digits such as "500" inside "70500".
        assert_eq!(
            retry_delay(&anyhow::anyhow!("bad parameter value 70500"), 1),
            None
        );
        assert!(
            retry_delay(&anyhow::anyhow!("connection reset by peer"), 1)
                .is_some()
        );
        // Retry-After wins over backoff.
        assert_eq!(
            retry_delay(
                &provider_error(ProviderErrorKind::RateLimited, Some(1500)),
                3
            ),
            Some(std::time::Duration::from_millis(1500))
        );
        // `Retry-After: 0` still waits a little, never a burst.
        assert_eq!(
            retry_delay(
                &provider_error(ProviderErrorKind::Transient, Some(0)),
                1
            ),
            Some(RETRY_MIN_DELAY)
        );
        let first = backoff_delay(1);
        assert!(
            first >= RETRY_BASE_DELAY
                && first <= RETRY_BASE_DELAY.mul_f64(1.25)
        );
        assert!(backoff_delay(3) >= RETRY_BASE_DELAY * 4);
        assert!(backoff_delay(50) <= RETRY_MAX_DELAY);
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
                let mut error = ProviderError::transient(
                    "provider API error (503): temporarily unavailable",
                );
                error.retry_after = Some(std::time::Duration::from_millis(10));
                let _ = sender.send(Err(error.into()));
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
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(NamedTool("read"))]));
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 2,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hi".into(),
            Vec::new(),
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
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(NamedTool("read"))]));
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hi".into(),
            Vec::new(),
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
        let perm_broker = PermissionBroker::default();

        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_loop(
                RunConfig {
                    provider,
                    tools: Arc::new(ToolRegistry::new(Vec::new())),
                    system_layers: Vec::new(),
                    options: ChatOptions::default(),
                    permissions: std::collections::HashMap::new(),
                    max_steps: 2,
                    working_directory: "/tmp".into(),
                    role_config: None,
                },
                RunChannels {
                    events_tx,
                    cancel_rx,
                    permission_broker: perm_broker,
                },
                "hi".into(),
                Vec::new(),
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
        let tools: Arc<ToolRegistry> = Arc::new(ToolRegistry::new(vec![
            Box::new(NamedTool("read")),
            Box::new(NamedTool("finish_task")),
        ]));
        let permissions = std::collections::HashMap::from([
            ("read".to_string(), crate::config::Permission::Allow),
            ("finish_task".to_string(), crate::config::Permission::Allow),
        ]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hi".into(),
            Vec::new(),
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
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(NamedTool("read"))]));
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hi".into(),
            Vec::new(),
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
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(NamedTool("read"))]));
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 1,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hi".into(),
            Vec::new(),
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
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(NamedTool("read"))]));
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        let max_steps = 3usize;
        run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "go".into(),
            Vec::new(),
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
        let tools: Arc<ToolRegistry> = Arc::new(ToolRegistry::new(vec![
            Box::new(NamedTool("read")),
            Box::new(NamedTool("finish_task")),
        ]));
        let permissions = std::collections::HashMap::from([
            ("read".to_string(), crate::config::Permission::Allow),
            ("finish_task".to_string(), crate::config::Permission::Allow),
        ]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hi".into(),
            Vec::new(),
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
        let tools: Arc<ToolRegistry> = Arc::new(ToolRegistry::new(vec![
            Box::new(NamedTool("read")),
            Box::new(NamedTool("finish_task")),
        ]));
        let permissions = std::collections::HashMap::from([
            ("read".to_string(), crate::config::Permission::Deny),
            ("finish_task".to_string(), crate::config::Permission::Allow),
        ]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hi".into(),
            Vec::new(),
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
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(NamedTool("read"))]));
        let permissions = std::collections::HashMap::from([(
            "read".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();

        run_loop(
            RunConfig {
                provider: provider_for_loop,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: Some(role_config),
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hi".into(),
            Vec::new(),
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
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(BlockingTool)]));
        let permissions = std::collections::HashMap::from([(
            "block".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();
        let loop_task = tokio::spawn(run_loop(
            RunConfig {
                provider,
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: perm_broker,
            },
            "hi".into(),
            Vec::new(),
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

    #[tokio::test]
    async fn test_run_loop_reports_tool_panic_as_tool_failure() {
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider {
            calls: Arc::new(Mutex::new(0)),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [
                    ChatResult {
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: "call_boom".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: "boom".into(),
                                arguments: "{}".into(),
                            },
                        }],
                        usage: Usage::default(),
                        finish_reason: Some("tool_calls".into()),
                        reasoning_content: None,
                    },
                    ChatResult {
                        content: Some("recovered".into()),
                        tool_calls: Vec::new(),
                        usage: Usage::default(),
                        finish_reason: Some("stop".into()),
                        reasoning_content: None,
                    },
                ],
            ))),
        });
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(PanickingTool)]));
        let permissions = std::collections::HashMap::from([(
            "boom".to_string(),
            crate::config::Permission::Allow,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let perm_broker = PermissionBroker::default();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_loop(
                RunConfig {
                    provider,
                    tools,
                    system_layers: Vec::new(),
                    options: ChatOptions::default(),
                    permissions,
                    max_steps: 10,
                    working_directory: "/tmp".into(),
                    role_config: None,
                },
                RunChannels {
                    events_tx,
                    cancel_rx,
                    permission_broker: perm_broker,
                },
                "hi".into(),
                Vec::new(),
            ),
        )
        .await
        .expect("run_loop must finish after a tool panic");

        let mut saw_failure = false;
        let mut done_history = None;
        while let Ok(event) = events_rx.try_recv() {
            match event.unwrap() {
                AgentEvent::ToolFailed { id, error } => {
                    assert_eq!(id, "call_boom");
                    assert!(error.contains("panicked"), "{error}");
                    assert!(error.contains("tool exploded"), "{error}");
                    saw_failure = true;
                }
                AgentEvent::Done { history, .. } => {
                    done_history = Some(history)
                }
                _ => {}
            }
        }
        assert!(saw_failure);
        let history = done_history.expect("loop should reach Done");
        assert!(history.iter().any(|m| {
            m.tool_call_id.as_deref() == Some("call_boom")
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.starts_with("Error:"))
        }));
    }

    #[tokio::test]
    async fn test_permission_broker_routes_by_id() {
        let broker = PermissionBroker::default();
        let (id, rx) = broker.register("call_1");
        assert_eq!(id, "perm_0_call_1");
        assert!(!broker.resolve("perm_other", true));
        assert!(broker.resolve(&id, true));
        assert!(rx.await.unwrap());
        // Already answered: a duplicate response finds nobody waiting.
        assert!(!broker.resolve(&id, false));

        let (id, rx) = broker.register("call_2");
        broker.forget(&id);
        assert!(!broker.resolve(&id, true));
        assert!(rx.await.is_err());
    }

    #[tokio::test]
    async fn test_permission_broker_scoped_ids_share_pending_map() {
        let parent = PermissionBroker::default();
        let sub = parent.scoped("sub_1:");
        let (parent_id, _parent_rx) = parent.register("call_0");
        let (sub_id, sub_rx) = sub.register("call_0");
        assert_eq!(sub_id, "sub_1:perm_1_call_0");
        assert_ne!(parent_id, sub_id);
        // The websocket handler only holds the parent broker.
        assert!(parent.resolve(&sub_id, false));
        assert!(!sub_rx.await.unwrap());
    }

    fn single_tool_call_then_stop(tool: &str) -> Arc<dyn LlmProvider> {
        Arc::new(ScriptedProvider {
            calls: Arc::new(Mutex::new(0)),
            responses: Arc::new(Mutex::new(std::collections::VecDeque::from(
                [
                    ChatResult {
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: "call_0".into(),
                            call_type: "function".into(),
                            function: ToolFunction {
                                name: tool.into(),
                                arguments: "{}".into(),
                            },
                        }],
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
                ],
            ))),
        })
    }

    /// Run a loop whose only tool needs approval; answer the prompt with
    /// `allowed` through the (scoped) broker and return all events.
    async fn run_with_permission_answer(
        broker: PermissionBroker,
        answer_via: PermissionBroker,
        allowed: bool,
    ) -> Vec<AgentEvent> {
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(NamedTool("guarded"))]));
        let permissions = std::collections::HashMap::from([(
            "guarded".to_string(),
            crate::config::Permission::Ask,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let loop_task = tokio::spawn(run_loop(
            RunConfig {
                provider: single_tool_call_then_stop("guarded"),
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: broker,
            },
            "hi".into(),
            Vec::new(),
        ));
        let mut events = Vec::new();
        while let Some(event) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            events_rx.recv(),
        )
        .await
        .expect("loop stalled")
        {
            let event = event.unwrap();
            if let AgentEvent::PermissionRequest { id, .. } = &event {
                assert!(answer_via.resolve(id, allowed));
            }
            events.push(event);
        }
        loop_task.await.unwrap();
        events
    }

    #[tokio::test]
    async fn test_run_loop_waits_for_permission_and_executes_when_allowed() {
        let broker = PermissionBroker::default();
        let events =
            run_with_permission_answer(broker.clone(), broker, true).await;
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::PermissionRequest { id, .. } if id == "perm_0_call_0"
        )));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCompleted { id, .. } if id == "call_0")));
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
        // The wait ends before the tool runs, so observers can separate user
        // wait time from tool run time.
        let resolved = events.iter().position(|e| {
            matches!(e, AgentEvent::PermissionResolved { id } if id == "perm_0_call_0")
        });
        let completed = events
            .iter()
            .position(|e| matches!(e, AgentEvent::ToolCompleted { .. }));
        assert!(resolved.unwrap() < completed.unwrap());
    }

    #[tokio::test]
    async fn test_scoped_run_loop_permission_denied_through_parent_broker() {
        // Mirrors a subagent: its loop uses a scoped broker while the answer
        // arrives through the parent's broker.
        let parent = PermissionBroker::default();
        let events =
            run_with_permission_answer(parent.scoped("sub_x:"), parent, false)
                .await;
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::PermissionRequest { id, .. } if id == "sub_x:perm_0_call_0"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolFailed { error, .. } if error.contains("Permission denied by user")
        )));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCompleted { .. }))
        );
    }

    #[tokio::test]
    async fn test_cancel_during_permission_wait_forgets_request() {
        let broker = PermissionBroker::default();
        let tools: Arc<ToolRegistry> =
            Arc::new(ToolRegistry::new(vec![Box::new(NamedTool("guarded"))]));
        let permissions = std::collections::HashMap::from([(
            "guarded".to_string(),
            crate::config::Permission::Ask,
        )]);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let loop_task = tokio::spawn(run_loop(
            RunConfig {
                provider: single_tool_call_then_stop("guarded"),
                tools,
                system_layers: Vec::new(),
                options: ChatOptions::default(),
                permissions,
                max_steps: 10,
                working_directory: "/tmp".into(),
                role_config: None,
            },
            RunChannels {
                events_tx,
                cancel_rx,
                permission_broker: broker.clone(),
            },
            "hi".into(),
            Vec::new(),
        ));
        let perm_id = loop {
            if let Ok(AgentEvent::PermissionRequest { id, .. }) =
                events_rx.recv().await.unwrap()
            {
                break id;
            }
        };
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
        loop_task.await.unwrap();
        assert!(!broker.resolve(&perm_id, true));
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
