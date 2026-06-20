pub mod keybinding;
pub mod render;
pub mod state;

use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    Event, EventStream, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use tokio::time::MissedTickBehavior;

use self::keybinding::{Action, Keybindings};
use self::state::{App, AppState, ServerHealth, SlashAction};
use crate::client::{ChatStream, MoteClient};

/// Run the TUI event loop. Returns when the user quits.
pub async fn run_tui(mut app: App, client: &MoteClient) -> Result<App> {
    // Setup terminal
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture,
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Build keybinding map from keybindings.toml (optional)
    let raw_bindings = crate::config::load_keybindings();
    let keybindings = Keybindings::from_config(raw_bindings.as_ref());
    let mut reader = EventStream::new();

    // Agent / WS chat channels
    let mut chat_stream: Option<ChatStream> = None;

    // Health check ticker
    let mut health_interval = tokio::time::interval(Duration::from_secs(5));
    health_interval.reset();

    let mut animation_interval =
        tokio::time::interval(Duration::from_millis(120));
    animation_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        let result = terminal.draw(|f| render::render(f, &mut app));
        if result.is_err() {
            break;
        }
        if app.state == AppState::Quitting {
            break;
        }

        // Process events
        let animate_loading =
            should_animate_loading(&app, chat_stream.is_some());

        if let Some(ref mut stream) = chat_stream {
            tokio::select! {
                event = reader.next() => {
                    if let Some(Ok(ev)) = event {
                        handle_key_event(&mut app, &keybindings, ev, &mut chat_stream);
                    } else {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                _ = animation_interval.tick(), if animate_loading => {}
                server_event = stream.rx.recv() => {
                    match server_event {
                        Some(event) => handle_server_event(&mut app, event, &mut chat_stream),
                        None => {
                            if let Some(run_id) = app.active_run_id.clone() {
                                match client.chat_stream(build_attach_request(&app, run_id)).await {
                                    Ok(new_stream) => {
                                        *stream = new_stream;
                                        continue;
                                    }
                                    Err(e) => {
                                        app.server_health = ServerHealth::Disconnected(format!(
                                            "stream reconnect failed: {e}"
                                        ));
                                    }
                                }
                            }
                            app.pending_permission = None;
                            // Flush any buffered content before going idle
                            if !app.stream_buffer.is_empty() {
                                let text = std::mem::take(&mut app.stream_buffer);
                                app.messages.push(self::state::DisplayMessage {
                                    role: crate::llm::Role::Assistant,
                                    content: text,
                                    thinking: None,
                                    source: self::state::MessageSource::Conversation,
                                });
                            }
                            chat_stream = None;
                            app.state = AppState::Idle;
                        }
                    }
                }
            }
        } else {
            tokio::select! {
                event = reader.next() => {
                    if let Some(Ok(ev)) = event {
                        handle_key_event(&mut app, &keybindings, ev, &mut chat_stream);
                    } else {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                _ = animation_interval.tick(), if animate_loading => {}
                _ = health_interval.tick() => {
                    let healthy = client.health().await;
                    app.server_health = if healthy {
                        ServerHealth::Connected
                    } else {
                        ServerHealth::Disconnected("no connection".into())
                    };
                    if !healthy {
                        // If we were in a chat stream, it would have disconnected.
                        // Just update the status bar.
                    }
                }
            }
        }

        // Handle pending async slash actions
        if let Some(action) = app.pending_slash.take() {
            match action {
                SlashAction::FetchModels => match client.list_models().await {
                    Ok(models) => {
                        if models.is_empty() {
                            app.messages.push(
                                self::state::DisplayMessage::command(
                                    crate::llm::Role::Assistant,
                                    "No models available.".into(),
                                ),
                            );
                        } else {
                            app.open_model_picker(models);
                        }
                    }
                    Err(e) => app.messages.push(self::state::DisplayMessage {
                        role: crate::llm::Role::Assistant,
                        content: format!("Error fetching models: {e}"),
                        thinking: None,
                        source: self::state::MessageSource::Error,
                    }),
                },
                SlashAction::OpenSessions => {
                    if chat_stream.is_some() || app.state != AppState::Idle {
                        app.messages.push(
                            self::state::DisplayMessage::command(
                                crate::llm::Role::Assistant,
                                "Cannot open sessions while agent is running."
                                    .into(),
                            ),
                        );
                    } else {
                        match client
                            .list_sessions(&app.runtime_session_key)
                            .await
                        {
                            Ok(sessions) => app.open_session_picker(sessions),
                            Err(e) => {
                                app.messages.push(self::state::DisplayMessage {
                                    role: crate::llm::Role::Assistant,
                                    content: format!("Error: {e}"),
                                    thinking: None,
                                    source: self::state::MessageSource::Error,
                                })
                            }
                        }
                    }
                }
                SlashAction::LoadSession(id) => {
                    if chat_stream.is_some() || app.state != AppState::Idle {
                        app.messages.push(
                            self::state::DisplayMessage::command(
                                crate::llm::Role::Assistant,
                                "Cannot load a session while agent is running."
                                    .into(),
                            ),
                        );
                        continue;
                    }
                    let result = client
                        .load_session(&app.runtime_session_key, &id)
                        .await;
                    match result {
                        Ok(session) => {
                            app.reset_for_loaded_session();
                            for hm in &session.messages {
                                let role = match hm.role.as_str() {
                                    "user" => crate::llm::Role::User,
                                    "assistant" => crate::llm::Role::Assistant,
                                    _ => continue,
                                };
                                app.messages.push(self::state::DisplayMessage {
                                    role,
                                    content: hm.content.clone(),
                                    thinking: None,
                                    source: self::state::MessageSource::Conversation,
                                });
                            }
                            app.active_session_id = Some(id.clone());
                            app.scroll_to_bottom();
                            app.messages.push(
                                self::state::DisplayMessage::command(
                                    crate::llm::Role::Assistant,
                                    format!("Resumed session: {id}"),
                                ),
                            );
                        }
                        Err(e) => {
                            app.messages.push(self::state::DisplayMessage {
                                role: crate::llm::Role::Assistant,
                                content: format!(
                                    "Failed to load session {id}: {e}"
                                ),
                                thinking: None,
                                source: self::state::MessageSource::Error,
                            })
                        }
                    }
                }
                SlashAction::SaveCredential(provider, key, value) => {
                    let result = match client
                        .save_credential(&provider, &key, &value)
                        .await
                    {
                        Ok(()) => format!(
                            "✅ {} API key saved to auth.json.",
                            provider
                        ),
                        Err(e) => format!("Error: {e}"),
                    };
                    app.messages.push(self::state::DisplayMessage {
                        role: crate::llm::Role::Assistant,
                        content: result,
                        thinking: None,
                        source: self::state::MessageSource::Command,
                    });
                }
                SlashAction::RollbackLast => {
                    let result = match client
                        .rollback_last(&app.runtime_session_key)
                        .await
                    {
                        Ok(payload) => {
                            let mut lines = vec![payload.message];
                            for ch in payload.changes {
                                match ch.kind {
                                    marshaling_protocol::FileChangeKind::Added => lines.push(format!("! new file added: {}", ch.path)),
                                    marshaling_protocol::FileChangeKind::Removed => lines.push(format!("! file removed: {}", ch.path)),
                                    marshaling_protocol::FileChangeKind::Modified => lines.push(format!("~ modified: {}", ch.path)),
                                }
                            }
                            lines.join("\n")
                        }
                        Err(e) => format!("Rollback failed: {e}"),
                    };
                    app.messages.push(self::state::DisplayMessage {
                        role: crate::llm::Role::Assistant,
                        content: result,
                        thinking: None,
                        source: self::state::MessageSource::Command,
                    });
                }
                SlashAction::RunShell(command) => {
                    let result =
                        run_shell_command(&command, &app.workspace_root).await;
                    let (content, source) = match result {
                        Ok(output) => {
                            (output, self::state::MessageSource::Command)
                        }
                        Err(e) => (
                            format!("Shell command failed to start: {e:#}"),
                            self::state::MessageSource::Error,
                        ),
                    };
                    app.messages.push(self::state::DisplayMessage {
                        role: crate::llm::Role::Assistant,
                        content,
                        thinking: None,
                        source,
                    });
                    app.scroll_to_bottom();
                }
            }
        }

        // Send pending permission response if any
        if let Some((id, allowed, remember)) =
            app.pending_permission_response.take()
        {
            if let Some(ref mut stream) = chat_stream {
                let resp =
                    marshaling_protocol::ClientEvent::PermissionResponse {
                        id,
                        allowed,
                        remember,
                    };
                // Send synchronously — quick operation, won't block
                if let Err(e) = stream.send(resp).await {
                    tracing::warn!("Failed to send permission response: {e}");
                }
            }
        }

        // Send cancel signal if user pressed Escape/CancelAgent during streaming
        if app.pending_cancel {
            app.pending_cancel = false;
            if let Some(ref mut stream) = chat_stream {
                let cancel_event = marshaling_protocol::ClientEvent::Cancel;
                if let Err(e) = stream.send(cancel_event).await {
                    tracing::warn!("Failed to send cancel event: {e}");
                }
            }
        }

        // After idle + no active chat: check if user sent a message → start chat
        if chat_stream.is_none() && app.state == AppState::Idle {
            if let Some(last) = app.messages.last() {
                if last.role == crate::llm::Role::User
                    && last.source == self::state::MessageSource::Conversation
                {
                    start_chat(client, &mut app, &mut chat_stream).await;
                }
            }
        }
    }

    // Cleanup
    let _ = terminal.show_cursor();
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show,
        crossterm::event::DisableMouseCapture,
    )?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(app)
}

/// Start a chat via WebSocket, returning a receiver for server events.
async fn start_chat(
    client: &MoteClient,
    app: &mut App,
    chat_stream: &mut Option<ChatStream>,
) {
    let user_msg = match app.messages.last() {
        Some(m) if m.role == crate::llm::Role::User => m.content.clone(),
        _ => return,
    };

    app.start_agent();

    let request = build_chat_request(app, user_msg.clone());

    match client.chat_stream(request.clone()).await {
        Ok(stream) => {
            app.server_health = ServerHealth::Connected;
            *chat_stream = Some(stream);
        }
        Err(_) => {
            // Retry once after a brief delay
            tokio::time::sleep(Duration::from_millis(500)).await;
            match client.chat_stream(request).await {
                Ok(stream) => {
                    app.server_health = ServerHealth::Connected;
                    *chat_stream = Some(stream);
                }
                Err(e) => {
                    app.server_health =
                        ServerHealth::Disconnected(format!("{:#}", e));
                    app.set_error(&format!("Failed to connect: {:#}", e));
                }
            }
        }
    }
}

fn should_animate_loading(app: &App, chat_stream_active: bool) -> bool {
    app.loading_progress.is_some() || chat_stream_active
}

fn build_chat_request(
    app: &App,
    user_msg: String,
) -> marshaling_protocol::ChatRequest {
    let (model_override, provider_override) =
        app.current_model_override_parts();
    // Build conversation history from prior display messages (excluding the latest user message).
    // Only include Conversation-sourced messages — skip command outputs and errors.
    let history: Vec<marshaling_protocol::HistoryMessage> = app.messages
        [..app.messages.len().saturating_sub(1)]
        .iter()
        .filter(|m| m.source == self::state::MessageSource::Conversation)
        .map(|m| marshaling_protocol::HistoryMessage {
            role: match m.role {
                crate::llm::Role::User => "user".into(),
                crate::llm::Role::Assistant => "assistant".into(),
            },
            content: m.content.clone(),
        })
        .collect();

    marshaling_protocol::ChatRequest {
        message: user_msg,
        agent: app.current_agent.clone(),
        model_override,
        provider_override,
        session_id: app.active_session_id.clone(),
        history,
        workspace_root: Some(app.workspace_root.clone()),
        repo_agents_md: app.repo_agents_md.clone(),
        runtime_session_key: Some(app.runtime_session_key.clone()),
        run_id: None,
    }
}

fn build_attach_request(
    app: &App,
    run_id: String,
) -> marshaling_protocol::ChatRequest {
    let (model_override, provider_override) =
        app.current_model_override_parts();
    marshaling_protocol::ChatRequest {
        message: String::new(),
        agent: app.current_agent.clone(),
        model_override,
        provider_override,
        session_id: app.active_session_id.clone(),
        history: Vec::new(),
        workspace_root: Some(app.workspace_root.clone()),
        repo_agents_md: app.repo_agents_md.clone(),
        runtime_session_key: Some(app.runtime_session_key.clone()),
        run_id: Some(run_id),
    }
}

/// Handle a server-sent event.
fn handle_server_event(
    app: &mut App,
    event: marshaling_protocol::ServerEvent,
    chat_stream: &mut Option<ChatStream>,
) {
    use marshaling_protocol::ServerEvent;
    match event {
        ServerEvent::RunStarted { run_id }
        | ServerEvent::RunAttached { run_id } => {
            app.active_run_id = Some(run_id);
        }
        ServerEvent::RunDetached { .. } => {}
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
            // If user previously chose "Allow Always" for this tool, auto-allow
            if app.auto_allowed_tools.contains(&tool_name) {
                app.pending_permission_response = Some((id, true, true));
            } else {
                app.pending_permission = Some(self::state::PendingPermission {
                    id,
                    tool_name,
                    args: args.to_string(),
                    confirming_always: false,
                });
            }
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
        }
        ServerEvent::SubagentTextDelta { id, data } => {
            if let Some(sv) = app.subagent_views.iter_mut().find(|s| s.id == id)
            {
                sv.stream_buffer.push_str(&data);
            } else {
                tracing::warn!("SubagentTextDelta for unknown id: {}", id);
            }
        }
        ServerEvent::SubagentReasoningDelta { id, data } => {
            if let Some(sv) = app.subagent_views.iter_mut().find(|s| s.id == id)
            {
                sv.reasoning_buffer.push_str(&data);
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
                }
            } else {
                tracing::warn!("SubagentToolFailed for unknown id: {}", id);
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
            *chat_stream = None;
            // Auto-dequeue: if there are queued messages, add one as a user message
            if !app.input_queue.is_empty() {
                let next = app.input_queue.pop_front().unwrap();
                app.messages.push(self::state::DisplayMessage {
                    role: crate::llm::Role::User,
                    content: next,
                    thinking: None,
                    source: self::state::MessageSource::Conversation,
                });
            }
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
        }
        ServerEvent::Error { message } => {
            app.pending_permission = None;
            app.clear_esc_cancel_arm();
            app.set_error(&message);
            *chat_stream = None;
        }
        ServerEvent::Unknown => {
            // Unknown event type — ignore for backwards compatibility
        }
    }
}

/// Handle keyboard and mouse events.
fn handle_key_event(
    app: &mut App,
    keys: &Keybindings,
    event: Event,
    _chat_stream: &mut Option<ChatStream>,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if app.session_picker_open {
                match key.code {
                    crossterm::event::KeyCode::Up => app.session_picker_up(),
                    crossterm::event::KeyCode::Down => {
                        app.session_picker_down()
                    }
                    crossterm::event::KeyCode::Esc => {
                        app.close_session_picker();
                    }
                    crossterm::event::KeyCode::Enter => {
                        if let Some(s) = app
                            .session_picker_items
                            .get(app.session_picker_index)
                            .cloned()
                        {
                            app.pending_slash =
                                Some(SlashAction::LoadSession(s.id));
                        }
                        app.close_session_picker();
                    }
                    _ => {}
                }
                return;
            }
            if app.model_picker_open {
                match key.code {
                    crossterm::event::KeyCode::Up => app.model_picker_up(),
                    crossterm::event::KeyCode::Down => app.model_picker_down(),
                    crossterm::event::KeyCode::Esc => app.close_model_picker(),
                    crossterm::event::KeyCode::Enter => {
                        if let Some(choice) = app.selected_model_choice() {
                            app.apply_model_choice(choice);
                        }
                        app.close_model_picker();
                    }
                    _ => {}
                }
                return;
            }
            let action = keys.lookup(key.code, key.modifiers);
            handle_action(app, action, key.code, key.modifiers);
        }
        Event::Mouse(m) => {
            app.mouse_position = Some((m.column, m.row));
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if handle_permission_mouse_click(app, m.column, m.row) {
                        return;
                    }
                }
                MouseEventKind::ScrollDown => {
                    app.scroll_down(3);
                }
                MouseEventKind::ScrollUp => {
                    app.scroll_up(3);
                }
                _ => {} // Ignore clicks/drags so Shift+click still selects text
            }
        }
        _ => {}
    }
}

fn handle_action(
    app: &mut App,
    action: Option<Action>,
    code: crossterm::event::KeyCode,
    modifiers: KeyModifiers,
) {
    // If there's a pending permission prompt, handle Y/A/N here regardless of state
    if app.pending_permission.is_some() {
        let mut perm = app.pending_permission.take().unwrap();
        if perm.confirming_always {
            // Confirmation mode: [Y] Confirm / [N] Cancel
            let should_confirm = matches!(
                (action, code),
                (Some(Action::SendMessage), _)
                    | (
                        None,
                        crossterm::event::KeyCode::Char('y')
                            | crossterm::event::KeyCode::Char('Y')
                    )
            );
            let should_cancel = matches!(
                (action, code),
                (Some(Action::Quit), _)
                    | (
                        None,
                        crossterm::event::KeyCode::Char('n')
                            | crossterm::event::KeyCode::Char('N')
                    )
            );
            if should_confirm {
                // User confirmed "Allow Always" — remember for the session
                app.auto_allowed_tools.insert(perm.tool_name.clone());
                app.pending_permission_response =
                    Some((perm.id.clone(), true, true));
            } else if should_cancel {
                // Cancel confirmation — back to permission prompt
                perm.confirming_always = false;
                app.pending_permission = Some(perm);
            } else {
                // Unhandled key — stay in confirmation
                app.pending_permission = Some(perm);
            }
        } else {
            // Permission prompt: [Y] Once / [A] Always / [N] Deny
            let should_allow = matches!(
                (action, code),
                (Some(Action::SendMessage), _)
                    | (
                        None,
                        crossterm::event::KeyCode::Char('y')
                            | crossterm::event::KeyCode::Char('Y')
                    )
            );
            let should_always = matches!(
                (action, code),
                (
                    None,
                    crossterm::event::KeyCode::Char('a')
                        | crossterm::event::KeyCode::Char('A')
                )
            );
            let should_deny = matches!(
                (action, code),
                (Some(Action::Quit), _)
                    | (
                        None,
                        crossterm::event::KeyCode::Char('n')
                            | crossterm::event::KeyCode::Char('N')
                    )
            );
            if should_always {
                // Switch to confirmation mode
                perm.confirming_always = true;
                app.pending_permission = Some(perm);
            } else if should_allow || should_deny {
                app.pending_permission_response =
                    Some((perm.id.clone(), should_allow, false));
            } else {
                // Unhandled key — restore permission
                app.pending_permission = Some(perm);
            }
        }
        return;
    }

    // Quit and scroll work in any state
    // During agent running, Ctrl+C cancels immediately and Esc requires a double-tap
    match action {
        Some(Action::Quit)
            if app.state == AppState::AgentRunning
                || app.state == AppState::WaitingResponse =>
        {
            // Ctrl+C cancels immediately while running.
            app.pending_cancel = true;
            app.clear_esc_cancel_arm();
            return;
        }
        Some(Action::CancelAgent) => {
            if app.state == AppState::AgentRunning
                || app.state == AppState::WaitingResponse
            {
                if app.esc_cancel_step() {
                    app.pending_cancel = true;
                    app.clear_esc_cancel_arm();
                } else {
                    app.messages.push(self::state::DisplayMessage::command(
                        crate::llm::Role::Assistant,
                        "Press Esc again within 2s to stop running agent."
                            .into(),
                    ));
                }
            }
            return;
        }
        Some(Action::Quit) => {
            app.clear_esc_cancel_arm();
            app.state = AppState::Quitting;
            return;
        }
        Some(Action::ScrollUp) => {
            app.scroll_up(10);
            return;
        }
        Some(Action::ScrollDown) => {
            app.scroll_down(10);
            return;
        }
        Some(Action::ScrollToBottom) => {
            app.scroll_to_bottom();
            return;
        }
        Some(Action::SwitchView) => {
            // Cycle through views: primary → subagent 0 → subagent 1 → ... → primary
            if app.subagent_views.is_empty() {
                return;
            }
            match app.current_subagent_index {
                None => app.current_subagent_index = Some(0),
                Some(idx) if idx + 1 < app.subagent_views.len() => {
                    app.current_subagent_index = Some(idx + 1)
                }
                Some(_) => app.current_subagent_index = None,
            }
            // Reset scroll so each view starts at the bottom
            app.scroll_offset = 0;
            app.auto_scroll = true;
            return;
        }
        _ => {}
    }

    // During agent running
    if app.state == AppState::AgentRunning {
        match action {
            Some(Action::SendMessage) => {
                let text = app.submit_input();
                if !text.is_empty() {
                    app.queue_input(&text);
                }
            }
            Some(Action::InsertNewline) => app.insert_newline(),
            Some(Action::CursorLeft) => app.cursor_left(),
            Some(Action::CursorRight) => app.cursor_right(),
            Some(Action::CursorHome) => app.cursor_home(),
            Some(Action::CursorEnd) => app.cursor_end(),
            Some(Action::KillLine) => app.kill_line(),
            Some(Action::DeleteBefore) => app.delete_before(),
            Some(Action::DeleteAfter) => app.delete_after(),
            Some(Action::HistoryUp) => app.history_up(),
            Some(Action::HistoryDown) => app.history_down(),
            None => {
                if let crossterm::event::KeyCode::Char(c) = code {
                    let clean = modifiers == KeyModifiers::NONE
                        || modifiers == KeyModifiers::SHIFT;
                    if clean {
                        app.insert_char(c);
                    }
                }
            }
            _ => {}
        }
        return;
    }

    // During waiting response, still allow input history navigation
    if app.state == AppState::WaitingResponse {
        match action {
            Some(Action::HistoryUp) => app.history_up(),
            Some(Action::HistoryDown) => app.history_down(),
            _ => {}
        }
        return;
    }

    // ── Suggestion mode ───────────────────────────────────
    if !app.suggestions.is_empty() {
        match action {
            Some(Action::HistoryUp) | Some(Action::HistoryDown) => {
                if action == Some(Action::HistoryUp) {
                    app.suggestion_prev();
                } else {
                    app.suggestion_next();
                }
            }
            Some(Action::Complete) | Some(Action::SendMessage) => {
                if app.suggestion_index > 0 {
                    app.accept_suggestion();
                } else if app.suggestions.len() == 1 {
                    app.suggestion_index = 1;
                    app.accept_suggestion();
                }
            }
            Some(Action::AgentCommand) => {
                app.suggestions.clear();
                app.suggestion_index = 0;
            }
            _ => normal_action(app, action, code, modifiers),
        }
        return;
    }

    normal_action(app, action, code, modifiers);
}

fn normal_action(
    app: &mut App,
    action: Option<Action>,
    code: crossterm::event::KeyCode,
    modifiers: KeyModifiers,
) {
    match action {
        Some(Action::SendMessage) => {
            let text = app.submit_input();
            if text.is_empty() && !app.handled_slash_command {
                app.state = AppState::Quitting;
            }
        }
        Some(Action::InsertNewline) => app.insert_newline(),
        Some(Action::CursorLeft) => app.cursor_left(),
        Some(Action::CursorRight) => app.cursor_right(),
        Some(Action::CursorHome) => app.cursor_home(),
        Some(Action::CursorEnd) => app.cursor_end(),
        Some(Action::KillLine) => app.kill_line(),
        Some(Action::DeleteBefore) => app.delete_before(),
        Some(Action::DeleteAfter) => app.delete_after(),
        Some(Action::HistoryUp) => app.history_up(),
        Some(Action::HistoryDown) => app.history_down(),
        Some(Action::AgentCommand) => {
            if app.input.is_empty() {
                app.input.push('/');
                app.input_cursor = 1;
                app.update_suggestions();
            }
        }
        Some(Action::Complete) => {
            if app.state == AppState::Idle {
                app.cycle_agent();
            }
        }
        None => {
            if let crossterm::event::KeyCode::Char(c) = code {
                let clean = modifiers == KeyModifiers::NONE
                    || modifiers == KeyModifiers::SHIFT;
                if clean && app.state == AppState::Idle {
                    app.insert_char(c);
                }
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionMouseAction {
    AllowOnce,
    AllowAlways,
    Deny,
    ConfirmAlways,
    CancelAlways,
}

fn handle_permission_mouse_click(app: &mut App, column: u16, row: u16) -> bool {
    let Some(perm) = app.pending_permission.as_ref() else {
        return false;
    };
    let Some(action) = permission_popup_action_at(perm, column, row) else {
        return false;
    };

    let mut perm = app.pending_permission.take().unwrap();
    match (perm.confirming_always, action) {
        (true, PermissionMouseAction::ConfirmAlways) => {
            app.auto_allowed_tools.insert(perm.tool_name.clone());
            app.pending_permission_response =
                Some((perm.id.clone(), true, true));
        }
        (true, PermissionMouseAction::CancelAlways) => {
            perm.confirming_always = false;
            app.pending_permission = Some(perm);
        }
        (false, PermissionMouseAction::AllowAlways) => {
            perm.confirming_always = true;
            app.pending_permission = Some(perm);
        }
        (false, PermissionMouseAction::AllowOnce) => {
            app.pending_permission_response =
                Some((perm.id.clone(), true, false));
        }
        (false, PermissionMouseAction::Deny) => {
            app.pending_permission_response =
                Some((perm.id.clone(), false, false));
        }
        _ => {
            app.pending_permission = Some(perm);
        }
    }
    true
}

fn permission_popup_action_at(
    perm: &self::state::PendingPermission,
    column: u16,
    row: u16,
) -> Option<PermissionMouseAction> {
    let (term_width, term_height) = crossterm::terminal::size().ok()?;
    let area = Rect::new(0, 0, term_width, term_height);
    let rect = centered_rect_local(
        area,
        area.width.min(88).max(46),
        area.height
            .min(if perm.confirming_always { 18 } else { 20 })
            .max(10),
    );
    let inner = inset_local(rect, 3, 1);
    let content_width = inner.width.saturating_sub(2) as usize;

    let mut button_row_index: usize = 4;
    let mut args_lines =
        render::json_to_yaml_lines_for_popup(&perm.args, content_width);
    let max_args =
        inner
            .height
            .saturating_sub(if perm.confirming_always { 8 } else { 7 })
            as usize;
    if args_lines.len() > max_args {
        args_lines.truncate(max_args.saturating_sub(1));
        args_lines.push("... (args truncated)".into());
    }
    if !args_lines.is_empty() {
        button_row_index += 1 + args_lines.len() + 1;
    }
    if perm.confirming_always {
        button_row_index += 1 + 1;
    }

    let button_row = inner.y + button_row_index as u16;
    if row != button_row {
        return None;
    }

    if perm.confirming_always {
        button_hit(
            column,
            inner.x,
            &[
                (" Y ", PermissionMouseAction::ConfirmAlways),
                (" Confirm   ", PermissionMouseAction::ConfirmAlways),
                (" N ", PermissionMouseAction::CancelAlways),
                (" Cancel", PermissionMouseAction::CancelAlways),
            ],
        )
    } else {
        button_hit(
            column,
            inner.x,
            &[
                (" Y ", PermissionMouseAction::AllowOnce),
                (" Allow once   ", PermissionMouseAction::AllowOnce),
                (" A ", PermissionMouseAction::AllowAlways),
                (" Allow always   ", PermissionMouseAction::AllowAlways),
                (" N ", PermissionMouseAction::Deny),
                (" Deny", PermissionMouseAction::Deny),
            ],
        )
    }
}

fn button_hit(
    column: u16,
    start_x: u16,
    segments: &[(&str, PermissionMouseAction)],
) -> Option<PermissionMouseAction> {
    let mut x = start_x;
    for (text, action) in segments {
        let width = text.chars().count() as u16;
        if column >= x && column < x.saturating_add(width) {
            return Some(*action);
        }
        x = x.saturating_add(width);
    }
    None
}

fn centered_rect_local(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    )
}

fn inset_local(rect: Rect, x: u16, y: u16) -> Rect {
    Rect::new(
        rect.x + x,
        rect.y + y,
        rect.width.saturating_sub(x.saturating_mul(2)),
        rect.height.saturating_sub(y.saturating_mul(2)),
    )
}

async fn run_shell_command(
    command: &str,
    workspace_root: &str,
) -> Result<String> {
    const MAX_OUTPUT_BYTES: usize = 16 * 1024;
    let output = tokio::process::Command::new("/bin/bash")
        .arg("-lc")
        .arg(command)
        .current_dir(workspace_root)
        .output()
        .await
        .with_context(|| format!("failed to run `{command}`"))?;

    let mut sections = Vec::new();
    let stdout = String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string();
    let stderr = String::from_utf8_lossy(&output.stderr)
        .trim_end()
        .to_string();
    if !stdout.is_empty() {
        sections.push(stdout);
    }
    if !stderr.is_empty() {
        sections.push(format!("stderr:\n{stderr}"));
    }
    if sections.is_empty() {
        sections.push("(no output)".into());
    }
    if !output.status.success() {
        sections.push(format!(
            "exit status: {}",
            output.status.code().map_or_else(
                || "terminated by signal".into(),
                |c| c.to_string()
            )
        ));
    }

    let mut text = sections.join("\n\n");
    if text.len() > MAX_OUTPUT_BYTES {
        text.truncate(MAX_OUTPUT_BYTES);
        text.push_str("\n... (output truncated)");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_ui_config() -> marshaling_protocol::UiConfig {
        marshaling_protocol::UiConfig {
            input_accent: "cyan".into(),
            user_accent: "cyan".into(),
            model_info: "deepseek/deepseek-chat".into(),
            agent_names: vec!["review".into()],
            subagent_names: vec![],
            agent_model_info: HashMap::from([
                ("default".into(), "deepseek/deepseek-chat".into()),
                ("review".into(), "github/gpt-4o".into()),
            ]),
        }
    }

    #[test]
    fn test_build_chat_request_includes_active_session_id() {
        let cfg = test_ui_config();
        let mut app = App::new_with_workspace(
            &cfg,
            cfg.model_info.clone(),
            "/tmp/ws".into(),
            None,
            "runtime-key".into(),
        );
        app.messages.push(super::state::DisplayMessage {
            role: crate::llm::Role::User,
            content: "hello".into(),
            thinking: None,
            source: super::state::MessageSource::Conversation,
        });
        app.active_session_id = Some("sess-abc".into());

        let req = build_chat_request(&app, "hello".into());
        assert_eq!(req.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(req.runtime_session_key.as_deref(), Some("runtime-key"));
    }

    #[test]
    fn test_build_chat_request_excludes_command_messages() {
        let cfg = test_ui_config();
        let mut app = App::new_with_workspace(
            &cfg,
            cfg.model_info.clone(),
            "/tmp/ws".into(),
            None,
            "runtime-key".into(),
        );
        app.messages.push(super::state::DisplayMessage::command(
            crate::llm::Role::User,
            "$ ls".into(),
        ));
        app.messages.push(super::state::DisplayMessage {
            role: crate::llm::Role::User,
            content: "hello".into(),
            thinking: None,
            source: super::state::MessageSource::Conversation,
        });

        let req = build_chat_request(&app, "hello".into());

        assert!(req.history.is_empty());
    }

    #[test]
    fn test_should_animate_loading_when_progress_present() {
        let cfg = test_ui_config();
        let mut app = App::new_with_workspace(
            &cfg,
            cfg.model_info.clone(),
            "/tmp/ws".into(),
            None,
            "runtime-key".into(),
        );

        assert!(!should_animate_loading(&app, false));
        app.loading_progress = Some(0.2);
        assert!(should_animate_loading(&app, false));
    }

    #[test]
    fn test_should_animate_loading_when_chat_stream_active() {
        let cfg = test_ui_config();
        let app = App::new_with_workspace(
            &cfg,
            cfg.model_info.clone(),
            "/tmp/ws".into(),
            None,
            "runtime-key".into(),
        );

        assert!(should_animate_loading(&app, true));
    }

    #[test]
    fn test_build_chat_request_uses_active_agent_override_only() {
        let cfg = test_ui_config();
        let mut app = App::new_with_workspace(
            &cfg,
            cfg.model_info.clone(),
            "/tmp/ws".into(),
            None,
            "runtime-key".into(),
        );
        app.agent_model_overrides.insert(
            "default".into(),
            super::state::AgentModelOverride {
                provider: Some("deepseek".into()),
                model_id: "deepseek-reasoner".into(),
            },
        );
        app.agent_model_overrides.insert(
            "review".into(),
            super::state::AgentModelOverride {
                provider: Some("github".into()),
                model_id: "gpt-4.1".into(),
            },
        );

        app.current_agent = "review".into();
        let req = build_chat_request(&app, "hello".into());
        assert_eq!(req.agent, "review");
        assert_eq!(req.model_override.as_deref(), Some("gpt-4.1"));
        assert_eq!(req.provider_override.as_deref(), Some("github"));
    }

    #[test]
    fn test_button_hit_maps_permission_segments() {
        let action = button_hit(
            5,
            0,
            &[
                (" Y ", PermissionMouseAction::AllowOnce),
                (" Allow once   ", PermissionMouseAction::AllowOnce),
                (" A ", PermissionMouseAction::AllowAlways),
            ],
        );
        assert_eq!(action, Some(PermissionMouseAction::AllowOnce));

        let action = button_hit(
            17,
            0,
            &[
                (" Y ", PermissionMouseAction::AllowOnce),
                (" Allow once   ", PermissionMouseAction::AllowOnce),
                (" A ", PermissionMouseAction::AllowAlways),
            ],
        );
        assert_eq!(action, Some(PermissionMouseAction::AllowAlways));
    }
}
