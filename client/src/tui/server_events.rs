//! Applying server events to the app state.

use super::*;

pub(super) fn is_terminal_server_event(
    event: &marshaling_protocol::ServerEvent,
) -> bool {
    matches!(
        event,
        marshaling_protocol::ServerEvent::Done { .. }
            | marshaling_protocol::ServerEvent::Cancelled { .. }
            | marshaling_protocol::ServerEvent::NeedsContinuation { .. }
            | marshaling_protocol::ServerEvent::Error { .. }
    )
}

/// Handle a server-sent event.
pub(super) fn handle_server_event(
    app: &mut App,
    event: marshaling_protocol::ServerEvent,
    chat_stream: &mut Option<ChatStream>,
) {
    use marshaling_protocol::ServerEvent;
    match event {
        ServerEvent::RunStarted { run_id, session_id } => {
            app.lost_run = None;
            app.active_run_id = Some(run_id);
            // The server owns the conversation; for a new one it reports the
            // session id, which later requests must send to continue it.
            if let Some(session_id) = session_id {
                app.active_session_id = Some(session_id);
            }
        }
        ServerEvent::RunAttached { run_id } => {
            app.active_run_id = Some(run_id);
        }
        ServerEvent::RunDetached { .. } => {}
        ServerEvent::SessionBusy { run_id } => {
            app.handle_session_busy(run_id);
            app.touch_response_render();
        }
        ServerEvent::TextDelta { data } => {
            app.agent_text_delta(&data);
            app.loading_progress = Some(0.5);
        }
        ServerEvent::ReasoningDelta { data } => {
            app.agent_reasoning_delta(&data);
        }
        ServerEvent::ToolStarted { id, name } => {
            app.agent_tool_started(&id, &name);
            app.loading_progress = Some(0.3);
        }
        ServerEvent::ToolCompleted {
            id,
            result,
            changes,
        } => {
            app.agent_tool_completed(&id, &result, &changes);
            app.loading_progress = Some(0.6);
        }
        ServerEvent::ToolFailed { id, error } => {
            app.agent_tool_failed(&id, &error);
        }
        ServerEvent::TurnDone { text, tool_calls } => {
            app.agent_turn_done(&text, &tool_calls);
            app.loading_progress = Some(0.7);
        }
        ServerEvent::PermissionRequest {
            id,
            tool_name,
            args,
        } => {
            app.show_permission_request(id, tool_name, &args);
        }
        ServerEvent::PermissionPending {
            id,
            tool_name,
            args,
        } => {
            app.show_pending_permission(id, tool_name, &args);
        }
        ServerEvent::SkillsLoaded { .. } => {
            // Skills loaded silently — no user-facing message.
            // Skills are advertised in the system prompt, no need to echo them.
        }
        ServerEvent::SkillSelected { name } => {
            app.current_skill = Some(name);
        }
        ServerEvent::SubagentStarted { id, name } => {
            app.subagent_views.push(self::state::SubagentView {
                id,
                name,
                stream_buffer: String::new(),
                reasoning_buffer: String::new(),
                tool_calls: Vec::new(),
                done: false,
                content: String::new(),
            });
            app.touch_response_render();
        }
        ServerEvent::SubagentTextDelta { id, data } => {
            if let Some(sv) = app.subagent_views.iter_mut().find(|s| s.id == id)
            {
                sv.stream_buffer.push_str(&data);
                app.touch_response_render();
            } else {
                tracing::warn!("SubagentTextDelta for unknown id: {}", id);
            }
        }
        ServerEvent::SubagentReasoningDelta { id, data } => {
            if let Some(sv) = app.subagent_views.iter_mut().find(|s| s.id == id)
            {
                sv.reasoning_buffer.push_str(&data);
                app.touch_response_render();
            } else {
                tracing::warn!("SubagentReasoningDelta for unknown id: {}", id);
            }
        }
        ServerEvent::SubagentToolStarted {
            id,
            sub_id,
            tool_name,
        } => {
            if let Some(sv) = app.subagent_views.iter_mut().find(|s| s.id == id)
            {
                sv.tool_calls.push(marshaling_protocol::ToolCallDisplay {
                    id: sub_id,
                    name: tool_name,
                    status: marshaling_protocol::ToolStatus::Running,
                    changes: Vec::new(),
                });
                app.touch_response_render();
            } else {
                tracing::warn!("SubagentToolStarted for unknown id: {}", id);
            }
        }
        ServerEvent::SubagentToolCompleted {
            id,
            sub_id,
            changes,
            ..
        } => {
            if let Some(sv) = app.subagent_views.iter_mut().find(|s| s.id == id)
            {
                if let Some(tc) =
                    sv.tool_calls.iter_mut().find(|t| t.id == sub_id)
                {
                    tc.status = marshaling_protocol::ToolStatus::Success;
                    tc.changes = changes;
                    app.touch_response_render();
                }
            } else {
                tracing::warn!("SubagentToolCompleted for unknown id: {}", id);
            }
        }
        ServerEvent::SubagentToolFailed { id, sub_id, error } => {
            if let Some(sv) = app.subagent_views.iter_mut().find(|s| s.id == id)
            {
                if let Some(tc) =
                    sv.tool_calls.iter_mut().find(|t| t.id == sub_id)
                {
                    tc.status = marshaling_protocol::ToolStatus::Failed(error);
                    app.touch_response_render();
                }
            } else {
                tracing::warn!("SubagentToolFailed for unknown id: {}", id);
            }
        }
        ServerEvent::SubagentRetrying {
            id,
            reason,
            discarded_output,
        } => {
            // The pane has no turn boundaries to cut back to, so mark the
            // retry inline instead of silently showing text twice.
            if let Some(sv) = app.subagent_views.iter_mut().find(|s| s.id == id)
            {
                let note = if discarded_output {
                    "the partial response above is discarded"
                } else {
                    "retrying"
                };
                sv.stream_buffer.push_str(&format!(
                    "\n[provider error: {reason}; {note}]\n"
                ));
                app.touch_response_render();
            }
        }
        ServerEvent::SubagentDone { id, content } => {
            if let Some(sv) = app.subagent_views.iter_mut().find(|s| s.id == id)
            {
                sv.done = true;
                sv.content = content;
                // Flush any remaining stream buffer text
                if !sv.stream_buffer.is_empty() {
                    if sv.content.is_empty() {
                        sv.content = std::mem::take(&mut sv.stream_buffer);
                    } else {
                        // stream_buffer is delta that was already included in content
                        sv.stream_buffer.clear();
                    }
                }
                // Add subagent result to primary conversation (must be Conversation so it's sent to LLM)
                let name = sv.name.clone();
                let result = sv.content.clone();
                app.messages.push(self::state::DisplayMessage {
                    role: crate::llm::Role::Assistant,
                    content: format!("--- Subagent: {} ---\n{}", name, result),
                    thinking: None,
                    source: self::state::MessageSource::Conversation,
                });
                app.touch_response_render();
            }
        }
        ServerEvent::Done {
            content,
            tokens_input,
            tokens_output,
        }
        | ServerEvent::Cancelled {
            content,
            tokens_input,
            tokens_output,
        }
        | ServerEvent::NeedsContinuation {
            content,
            tokens_input,
            tokens_output,
        } => {
            app.pending_permission = None;
            app.clear_esc_cancel_arm();
            app.agent_done(&content);
            app.active_run_id = None;
            app.tokens_input += tokens_input;
            app.tokens_output += tokens_output;
            app.loading_progress = None;
            app.clear_pending_command_overrides();
            *chat_stream = None;
            // Auto-dequeue one queued prompt after the current assistant turn
            // has been recorded, preserving conversation chronology.
            app.pop_queued_input_as_message();
        }
        ServerEvent::RollbackResult {
            success,
            message,
            changes,
        } => {
            let mut lines = vec![message];
            for ch in changes {
                match ch.kind {
                    marshaling_protocol::FileChangeKind::Added => {
                        lines.push(format!("! new file added: {}", ch.path))
                    }
                    marshaling_protocol::FileChangeKind::Removed => {
                        lines.push(format!("! file removed: {}", ch.path))
                    }
                    marshaling_protocol::FileChangeKind::Modified => {
                        lines.push(format!("~ modified: {}", ch.path))
                    }
                }
            }
            app.messages.push(self::state::DisplayMessage {
                role: crate::llm::Role::Assistant,
                content: lines.join("\n"),
                thinking: None,
                source: if success {
                    self::state::MessageSource::Command
                } else {
                    self::state::MessageSource::Error
                },
            });
            app.touch_response_render();
        }
        ServerEvent::ContextSize { chars } => {
            app.server_context_chars = Some(chars);
        }
        ServerEvent::Retrying {
            attempt,
            max_attempts,
            delay_ms,
            reason,
            discarded_output,
        } => {
            app.agent_retrying(
                attempt,
                max_attempts,
                delay_ms,
                &reason,
                discarded_output,
            );
            app.touch_response_render();
        }
        ServerEvent::Error { message } => {
            app.active_run_id = None;
            app.pending_permission = None;
            app.clear_esc_cancel_arm();
            app.clear_pending_command_overrides();
            app.set_error(&message);
            *chat_stream = None;
        }
        ServerEvent::Unknown => {
            // Unknown event type — ignore for backwards compatibility
        }
    }
}
