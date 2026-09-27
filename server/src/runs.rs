//! Agent runs: the run registry, event log and replay, attaching
//! sockets, the permission watchdog, and forwarding loop events into
//! persistence and the run log.

use super::*;

pub(crate) const COMPLETED_RUN_RETENTION: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunStatus {
    Done,
    Cancelled,
    NeedsContinuation,
    Failed,
}

pub(crate) struct ActiveRun {
    pub(crate) runtime_session_key: String,
    /// Client instance that started this run; at most one unfinished run is
    /// allowed per instance.
    pub(crate) client_instance_id: Option<String>,
    /// Transcript this run appends to; at most one unfinished run per
    /// session.
    pub(crate) session_path: Option<PathBuf>,
    pub(crate) events: Vec<marshaling_protocol::ServerEvent>,
    pub(crate) tx: broadcast::Sender<marshaling_protocol::ServerEvent>,
    pub(crate) cancel_tx: watch::Sender<bool>,
    pub(crate) permission_broker: agent::PermissionBroker,
    /// Delivers notes for the model into the running loop.
    pub(crate) notes_tx: mpsc::UnboundedSender<String>,
    /// Notes that reached the run after its loop stopped taking them; the
    /// forwarder writes them to the transcript when the run ends.
    pub(crate) pending_notes: Vec<String>,
    /// False once the forwarder has flushed `pending_notes` for the last
    /// time; later notes are appended to the transcript directly.
    pub(crate) accepting_notes: bool,
    /// Permission requests that have not been answered yet, by id. Re-sent
    /// to every newly attached socket as `PermissionPending`.
    pub(crate) pending_permission_tools: HashMap<String, PendingPermission>,
    /// Set once a terminal event has been recorded.
    pub(crate) finished: bool,
    /// Bumped on every socket attach; lets a detached-permission watchdog
    /// detect that a client came back in the meantime.
    pub(crate) attach_generation: u64,
    /// Attach generation a detached-permission watchdog is already armed
    /// for, so repeated triggers do not spawn duplicate watchdogs.
    pub(crate) watchdog_armed_for: Option<u64>,
}

pub(crate) struct PendingPermission {
    pub(crate) tool_name: String,
    pub(crate) args: serde_json::Value,
}

impl ActiveRun {
    pub(crate) fn new(
        runtime_session_key: String,
        client_instance_id: Option<String>,
        session_path: Option<PathBuf>,
        cancel_tx: watch::Sender<bool>,
        permission_broker: agent::PermissionBroker,
        notes_tx: mpsc::UnboundedSender<String>,
    ) -> Self {
        let (tx, _) = broadcast::channel(512);
        Self {
            runtime_session_key,
            client_instance_id,
            session_path,
            events: Vec::new(),
            tx,
            cancel_tx,
            permission_broker,
            notes_tx,
            pending_notes: Vec::new(),
            accepting_notes: true,
            pending_permission_tools: HashMap::new(),
            finished: false,
            attach_generation: 0,
            watchdog_armed_for: None,
        }
    }

    /// No client is attached while the run waits on a permission prompt, so
    /// nobody can answer it.
    pub(crate) fn is_detached_awaiting_permission(&self) -> bool {
        !self.finished && self.tx.receiver_count() == 0 && !self.pending_permission_tools.is_empty()
    }

    /// Register a new subscriber. Returns its live receiver, the log events
    /// to replay from `replay_from`, and a `PermissionPending` notice for
    /// every unanswered prompt. Must run under the `runs` lock so the
    /// snapshot and the subscription line up exactly.
    pub(crate) fn attach(
        &mut self,
        replay_from: usize,
    ) -> (
        broadcast::Receiver<marshaling_protocol::ServerEvent>,
        Vec<marshaling_protocol::ServerEvent>,
        Vec<marshaling_protocol::ServerEvent>,
    ) {
        self.attach_generation += 1;
        // A finished run gets nothing new on the live stream, so always
        // replay at least its terminal event (the last one) or the socket
        // would wait forever.
        let start = if self.finished {
            replay_from.min(self.events.len().saturating_sub(1))
        } else {
            replay_from
        };
        let pending = self
            .pending_permission_tools
            .iter()
            .map(
                |(id, pending)| marshaling_protocol::ServerEvent::PermissionPending {
                    id: id.clone(),
                    tool_name: pending.tool_name.clone(),
                    args: pending.args.clone(),
                },
            )
            .collect();
        (
            self.tx.subscribe(),
            self.events.get(start..).unwrap_or_default().to_vec(),
            pending,
        )
    }

    /// If a watchdog is needed and not yet armed for the current attach
    /// generation, mark it armed and return that generation.
    pub(crate) fn arm_detached_permission_watchdog(&mut self) -> Option<u64> {
        let generation = self.attach_generation;
        if !self.is_detached_awaiting_permission() || self.watchdog_armed_for == Some(generation) {
            return None;
        }
        self.watchdog_armed_for = Some(generation);
        Some(generation)
    }
}

/// Why a new run could not start.
#[derive(Debug, PartialEq)]
pub(crate) enum RunSlotConflict {
    /// The same client instance already has this unfinished run.
    SameClient(String),
    /// Another client has an unfinished run on the same session.
    SessionBusy(String),
}

/// Insert `run` unless its client instance or its session already has an
/// unfinished run. Check and insert happen under one lock so two concurrent
/// requests cannot both start a run.
pub(crate) fn claim_run_slot(
    runs: &mut HashMap<String, ActiveRun>,
    run_id: &str,
    run: ActiveRun,
) -> std::result::Result<(), RunSlotConflict> {
    if let Some(instance) = run.client_instance_id.as_deref()
        && let Some((busy_id, _)) = runs
            .iter()
            .find(|(_, r)| !r.finished && r.client_instance_id.as_deref() == Some(instance))
    {
        return Err(RunSlotConflict::SameClient(busy_id.clone()));
    }
    if let Some(path) = run.session_path.as_deref()
        && let Some((busy_id, _)) = runs
            .iter()
            .find(|(_, r)| !r.finished && r.session_path.as_deref() == Some(path))
    {
        return Err(RunSlotConflict::SessionBusy(busy_id.clone()));
    }
    runs.insert(run_id.to_string(), run);
    Ok(())
}

/// Cancel a run that stays detached while waiting on a permission prompt for
/// longer than the configured timeout; otherwise it would wait forever.
pub(crate) fn spawn_detached_permission_watchdog(
    state: &Arc<AppState>,
    run_id: &str,
    generation: u64,
) {
    let timeout =
        std::time::Duration::from_secs(state.config.server.detached_permission_timeout_secs);
    let state = Arc::clone(state);
    let run_id = run_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        let mut runs = state.runs.lock().await;
        let Some(run) = runs.get_mut(&run_id) else {
            return;
        };
        if run.watchdog_armed_for == Some(generation) {
            run.watchdog_armed_for = None;
        }
        if run.attach_generation == generation && run.is_detached_awaiting_permission() {
            tracing::warn!(
                run_id = %run_id,
                timeout_secs = timeout.as_secs(),
                "cancelling run: permission prompt unanswered while detached"
            );
            let _ = run.cancel_tx.send(true);
        }
    });
}

pub(crate) fn new_run_id() -> String {
    format!("run_{}", chrono::Local::now().format("%Y%m%d%H%M%S%6f"))
}

pub(crate) fn terminal_status(event: &marshaling_protocol::ServerEvent) -> Option<RunStatus> {
    match event {
        marshaling_protocol::ServerEvent::Done { .. } => Some(RunStatus::Done),
        marshaling_protocol::ServerEvent::Cancelled { .. } => Some(RunStatus::Cancelled),
        marshaling_protocol::ServerEvent::NeedsContinuation { .. } => {
            Some(RunStatus::NeedsContinuation)
        }
        marshaling_protocol::ServerEvent::Error { .. } => Some(RunStatus::Failed),
        _ => None,
    }
}

pub(crate) fn is_terminal_event(event: &marshaling_protocol::ServerEvent) -> bool {
    terminal_status(event).is_some()
}

pub(crate) async fn record_run_event(
    state: &Arc<AppState>,
    run_id: &str,
    event: marshaling_protocol::ServerEvent,
) {
    debug_assert!(
        event.is_run_log_event(),
        "connection-scoped event recorded into run log: {event:?}"
    );
    let terminal = is_terminal_event(&event);
    if let Some(status) = terminal_status(&event) {
        tracing::info!(run_id, ?status, "agent run reached terminal state");
    }
    let mut runs = state.runs.lock().await;
    let Some(run) = runs.get_mut(run_id) else {
        return;
    };

    if let marshaling_protocol::ServerEvent::PermissionRequest {
        id,
        tool_name,
        args,
    } = &event
    {
        run.pending_permission_tools.insert(
            id.clone(),
            PendingPermission {
                tool_name: tool_name.clone(),
                args: args.clone(),
            },
        );
    }
    if terminal {
        run.finished = true;
        run.pending_permission_tools.clear();
    }

    run.events.push(event.clone());
    let _ = run.tx.send(event);
    let watchdog_generation = run.arm_detached_permission_watchdog();
    drop(runs);
    if let Some(generation) = watchdog_generation {
        spawn_detached_permission_watchdog(state, run_id, generation);
    }

    if terminal {
        let expired = {
            let mut completed = state.completed_run_ids.lock().await;
            completed.push_back(run_id.to_string());
            let mut expired = Vec::new();
            while completed.len() > COMPLETED_RUN_RETENTION {
                if let Some(id) = completed.pop_front() {
                    expired.push(id);
                }
            }
            expired
        };
        if !expired.is_empty() {
            let mut runs = state.runs.lock().await;
            for id in expired {
                runs.remove(&id);
            }
        }
    }
}

/// Persists one run's conversation as the loop commits it.
pub(crate) struct RunPersistence {
    pub(crate) session_lock: Arc<tokio::sync::Mutex<()>>,
    /// Taken while a blocking write is in flight.
    pub(crate) writer: Option<store::TranscriptWriter>,
    pub(crate) runtime_session_key: String,
    pub(crate) model_provider: String,
    pub(crate) model_id: String,
    /// Size of the conversation the model will see next (for clients'
    /// compaction hints).
    pub(crate) context_chars: u64,
}

impl RunPersistence {
    /// Run a blocking transcript write under the session lock. Failures are
    /// logged: the run itself continues.
    pub(crate) async fn write(
        &mut self,
        what: &str,
        f: impl FnOnce(&mut store::TranscriptWriter) -> Result<()> + Send + 'static,
    ) {
        let lock = Arc::clone(&self.session_lock);
        let _guard = lock.lock().await;
        self.write_locked(what, f).await;
    }

    /// Write notes that arrived after the loop stopped taking them, and
    /// stop accepting more (later ones are appended directly). Holding the
    /// session lock across both closes the race with a concurrent rollback.
    pub(crate) async fn flush_pending_notes(&mut self, state: &Arc<AppState>, run_id: &str) {
        let lock = Arc::clone(&self.session_lock);
        let _guard = lock.lock().await;
        let notes = {
            let mut runs = state.runs.lock().await;
            runs.get_mut(run_id)
                .map(|run| {
                    run.accepting_notes = false;
                    std::mem::take(&mut run.pending_notes)
                })
                .unwrap_or_default()
        };
        if notes.is_empty() {
            return;
        }
        let messages: Vec<llm::ChatMessage> = notes
            .into_iter()
            .map(llm::ChatMessage::internal_note)
            .collect();
        self.write_locked("late notes", move |writer| {
            writer.append_messages(&messages)
        })
        .await;
    }

    /// `write` for callers that already hold the session lock.
    pub(crate) async fn write_locked(
        &mut self,
        what: &str,
        f: impl FnOnce(&mut store::TranscriptWriter) -> Result<()> + Send + 'static,
    ) {
        let Some(mut writer) = self.writer.take() else {
            tracing::warn!("transcript writer unavailable; {what} not saved");
            return;
        };
        match tokio::task::spawn_blocking(move || {
            let result = f(&mut writer);
            (writer, result)
        })
        .await
        {
            Ok((writer, result)) => {
                if let Err(e) = result {
                    tracing::warn!(
                        "Failed to save {what} to {}: {e:#}",
                        writer.path().display()
                    );
                }
                self.writer = Some(writer);
            }
            Err(e) => tracing::warn!("transcript write task failed: {e}"),
        }
    }

    pub(crate) async fn append_messages(&mut self, messages: Vec<llm::ChatMessage>) {
        self.write("messages", move |writer| writer.append_messages(&messages))
            .await;
    }

    pub(crate) async fn record_run_end(&mut self, tokens_input: u64, tokens_output: u64) {
        let record = store::Record::RunEnd {
            ts: chrono::Utc::now(),
            tokens_input,
            tokens_output,
            model_provider: self.model_provider.clone(),
            model_id: self.model_id.clone(),
        };
        self.write("run end", move |writer| writer.append_record(record))
            .await;
    }
}

/// Handle one event from the agent loop. Returns true when the event was
/// terminal and forwarding should stop.
pub(crate) async fn forward_agent_event(
    state: &Arc<AppState>,
    run_id: &str,
    save_ctx: &mut RunPersistence,
    agent_event: Result<agent::AgentEvent>,
) -> bool {
    match agent_event {
        Ok(agent::AgentEvent::MessagesCommitted(messages)) => {
            save_ctx.context_chars += store::history_chars(&messages);
            save_ctx.append_messages(messages).await;
            record_run_event(
                state,
                run_id,
                marshaling_protocol::ServerEvent::ContextSize {
                    chars: save_ctx.context_chars,
                },
            )
            .await;
            false
        }
        Ok(agent::AgentEvent::Done {
            content,
            tokens_input,
            tokens_output,
        }) => {
            save_ctx.flush_pending_notes(state, run_id).await;
            save_ctx.record_run_end(tokens_input, tokens_output).await;
            record_run_event(
                state,
                run_id,
                marshaling_protocol::ServerEvent::Done {
                    content,
                    tokens_input,
                    tokens_output,
                },
            )
            .await;
            true
        }
        Ok(agent::AgentEvent::Cancelled {
            content,
            tokens_input,
            tokens_output,
        }) => {
            save_ctx.flush_pending_notes(state, run_id).await;
            save_ctx.record_run_end(tokens_input, tokens_output).await;
            record_run_event(
                state,
                run_id,
                marshaling_protocol::ServerEvent::Cancelled {
                    content,
                    tokens_input,
                    tokens_output,
                },
            )
            .await;
            true
        }
        Ok(agent::AgentEvent::NeedsContinuation {
            content,
            tokens_input,
            tokens_output,
        }) => {
            save_ctx.flush_pending_notes(state, run_id).await;
            save_ctx.record_run_end(tokens_input, tokens_output).await;
            record_run_event(
                state,
                run_id,
                marshaling_protocol::ServerEvent::NeedsContinuation {
                    content,
                    tokens_input,
                    tokens_output,
                },
            )
            .await;
            true
        }
        Ok(agent::AgentEvent::ToolCompleted {
            id,
            name,
            result,
            changes,
            rollback_entries,
        }) => {
            if !rollback_entries.is_empty() {
                let mut sessions = state.runtime_states.lock().await;
                let session_state = sessions
                    .entry(save_ctx.runtime_session_key.clone())
                    .or_default();
                session_state.rollback_journal.push(RollbackChangeSet {
                    id: id.clone(),
                    tool_name: name,
                    entries: rollback_entries,
                    display_changes: changes.clone(),
                    transcript: save_ctx
                        .writer
                        .as_ref()
                        .map(|writer| writer.path().to_path_buf()),
                });
            }
            record_run_event(
                state,
                run_id,
                marshaling_protocol::ServerEvent::ToolCompleted {
                    id,
                    result,
                    changes,
                },
            )
            .await;
            false
        }
        Ok(agent::AgentEvent::SubagentToolCompleted {
            id,
            sub_id,
            tool_name,
            result,
            changes,
            rollback_entries,
        }) => {
            // Subagent edits are undoable like the parent's own.
            if !rollback_entries.is_empty() {
                let mut sessions = state.runtime_states.lock().await;
                let session_state = sessions
                    .entry(save_ctx.runtime_session_key.clone())
                    .or_default();
                session_state.rollback_journal.push(RollbackChangeSet {
                    id: sub_id.clone(),
                    tool_name,
                    entries: rollback_entries,
                    display_changes: changes.clone(),
                    transcript: save_ctx
                        .writer
                        .as_ref()
                        .map(|writer| writer.path().to_path_buf()),
                });
            }
            record_run_event(
                state,
                run_id,
                marshaling_protocol::ServerEvent::SubagentToolCompleted {
                    id,
                    sub_id,
                    result,
                    changes,
                },
            )
            .await;
            false
        }
        Ok(agent::AgentEvent::PermissionResolved { id }) => {
            // Covers answers that never reach `handle_client_event_for_run`
            // (e.g. subagent prompts resolved some other way), so the pending
            // set only holds prompts that are really still open.
            if let Some(run) = state.runs.lock().await.get_mut(run_id) {
                run.pending_permission_tools.remove(&id);
            }
            false
        }
        Ok(agent::AgentEvent::Failed {
            error,
            tokens_input,
            tokens_output,
        }) => {
            // The work done so far was already committed step by step.
            save_ctx.flush_pending_notes(state, run_id).await;
            save_ctx.record_run_end(tokens_input, tokens_output).await;
            tracing::error!(
                run_id = %run_id,
                error_kind = "agent_stream_error",
                "agent run failed: {error:#}"
            );
            record_run_event(
                state,
                run_id,
                marshaling_protocol::ServerEvent::Error {
                    message: format!("{:#}", error),
                },
            )
            .await;
            true
        }
        Err(e) => {
            save_ctx.flush_pending_notes(state, run_id).await;
            tracing::error!(
                run_id = %run_id,
                error_kind = "agent_stream_error",
                "agent run failed"
            );
            record_run_event(
                state,
                run_id,
                marshaling_protocol::ServerEvent::Error {
                    message: format!("{:#}", e),
                },
            )
            .await;
            true
        }
        Ok(event) => {
            if let Some(event) = agent_event_to_server_event(event) {
                record_run_event(state, run_id, event).await;
            }
            false
        }
    }
}

/// Forward agent-loop events into the run's event log until a terminal event.
///
/// The run task is supervised as well: if it panics or returns without a
/// terminal event, an `Error` is recorded so subscribers never wait forever.
/// Watching the task handle (not only channel closure) matters because other
/// senders of `agent_rx` (e.g. the subagent tool) may outlive the loop.
pub(crate) async fn forward_agent_events(
    state: Arc<AppState>,
    run_id: String,
    mut agent_rx: mpsc::UnboundedReceiver<Result<agent::AgentEvent>>,
    mut run_handle: tokio::task::JoinHandle<()>,
    mut save_ctx: RunPersistence,
) {
    let join_result = loop {
        tokio::select! {
            agent_event = agent_rx.recv() => {
                match agent_event {
                    Some(event) => {
                        if forward_agent_event(&state, &run_id, &mut save_ctx, event).await {
                            return;
                        }
                    }
                    None => break (&mut run_handle).await,
                }
            }
            joined = &mut run_handle => break joined,
        }
    };

    // The loop task has finished. Events it sent before returning are still
    // buffered in the channel, so drain them before deciding it ended badly.
    while let Ok(event) = agent_rx.try_recv() {
        if forward_agent_event(&state, &run_id, &mut save_ctx, event).await {
            return;
        }
    }

    save_ctx.flush_pending_notes(&state, &run_id).await;
    let message = match join_result {
        Ok(()) => "agent run ended without a final result".to_string(),
        Err(e) if e.is_panic() => format!(
            "agent run panicked: {}",
            agent::panic_message(e.into_panic().as_ref())
        ),
        Err(e) => format!("agent run ended unexpectedly: {e}"),
    };
    tracing::error!(run_id = %run_id, "{message}");
    record_run_event(
        &state,
        &run_id,
        marshaling_protocol::ServerEvent::Error { message },
    )
    .await;
}

/// If the forwarder task itself dies (e.g. a panic while saving), nothing
/// would ever mark the run finished: its client instance would be told
/// "busy" forever and the loop would run on with nobody draining its events.
/// Stop the loop and record a terminal error instead.
pub(crate) async fn supervise_forwarder(
    state: Arc<AppState>,
    run_id: String,
    forwarder: tokio::task::JoinHandle<()>,
) {
    let Err(e) = forwarder.await else {
        return;
    };
    let message = if e.is_panic() {
        format!(
            "agent run event forwarder panicked: {}",
            agent::panic_message(e.into_panic().as_ref())
        )
    } else {
        format!("agent run event forwarder ended unexpectedly: {e}")
    };
    let unfinished = {
        let runs = state.runs.lock().await;
        runs.get(&run_id)
            .filter(|run| !run.finished)
            .map(|run| {
                let _ = run.cancel_tx.send(true);
            })
            .is_some()
    };
    if unfinished {
        tracing::error!(run_id = %run_id, "{message}");
        record_run_event(
            &state,
            &run_id,
            marshaling_protocol::ServerEvent::Error { message },
        )
        .await;
    }
}

/// Attach a websocket to a run: replay its log from `replay_from`, then stream
/// live events until a terminal event or disconnect.
///
/// Subscribing and snapshotting the log happen under one lock, so the replay
/// and the live stream neither overlap nor leave a gap.
pub(crate) async fn attach_socket_to_run(
    mut socket: WebSocket,
    state: Arc<AppState>,
    run_id: String,
    replay_from: usize,
) {
    let (rx, replay, pending, runtime_session_key) = {
        let mut runs = state.runs.lock().await;
        let Some(run) = runs.get_mut(&run_id) else {
            drop(runs);
            send_error(&mut socket, format!("Unknown run_id: {run_id}")).await;
            return;
        };
        let (rx, replay, pending) = run.attach(replay_from);
        (rx, replay, pending, run.runtime_session_key.clone())
    };

    stream_run_to_socket(
        &mut socket,
        &state,
        &run_id,
        &runtime_session_key,
        rx,
        replay,
        pending,
    )
    .await;

    // The subscription is dropped by now; if nobody else is attached while
    // the run waits on a prompt, arm the watchdog.
    let watchdog_generation = {
        let mut runs = state.runs.lock().await;
        runs.get_mut(&run_id)
            .and_then(ActiveRun::arm_detached_permission_watchdog)
    };
    if let Some(generation) = watchdog_generation {
        spawn_detached_permission_watchdog(&state, &run_id, generation);
    }

    let detached = marshaling_protocol::ServerEvent::RunDetached { run_id };
    if let Ok(json) = serde_json::to_string(&detached) {
        let _ = socket.send(Message::Text(json.into())).await;
    }
}

/// Send the replay, re-announce unanswered prompts, then forward live events
/// and client messages until a terminal event, lag, or disconnect.
pub(crate) async fn stream_run_to_socket(
    socket: &mut WebSocket,
    state: &Arc<AppState>,
    run_id: &str,
    runtime_session_key: &str,
    mut rx: broadcast::Receiver<marshaling_protocol::ServerEvent>,
    replay: Vec<marshaling_protocol::ServerEvent>,
    pending: Vec<marshaling_protocol::ServerEvent>,
) {
    for event in replay {
        let terminal = is_terminal_event(&event);
        let json = match serde_json::to_string(&event) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("Failed to serialize replay event: {e}");
                continue;
            }
        };
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
        if terminal {
            return;
        }
    }

    let attached = marshaling_protocol::ServerEvent::RunAttached {
        run_id: run_id.to_string(),
    };
    for event in pending.into_iter().chain(std::iter::once(attached)) {
        if let Ok(json) = serde_json::to_string(&event)
            && socket.send(Message::Text(json.into())).await.is_err()
        {
            return;
        }
    }

    loop {
        tokio::select! {
            run_event = rx.recv() => {
                match run_event {
                    Ok(event) => {
                        let terminal = is_terminal_event(&event);
                        let json = match serde_json::to_string(&event) {
                            Ok(json) => json,
                            Err(e) => {
                                tracing::warn!("Failed to serialize run event: {e}");
                                continue;
                            }
                        };
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            return;
                        }
                        if terminal {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Skipping events would silently corrupt the client's
                        // view. Close instead: the client reattaches with its
                        // event count and gets an exact replay.
                        tracing::warn!("Websocket subscriber lagged by {skipped} run events; closing so it can resume");
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            ws_msg = socket.recv() => {
                match ws_msg {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(Message::Text(text))) => {
                        handle_client_event_for_run(
                            state,
                            run_id,
                            runtime_session_key,
                            socket,
                            &text,
                        )
                        .await;
                    }
                    _ => {}
                }
            }
        }
    }
}

pub(crate) async fn handle_client_event_for_run(
    state: &Arc<AppState>,
    run_id: &str,
    runtime_session_key: &str,
    socket: &mut WebSocket,
    text: &str,
) {
    let Ok(client_event) = serde_json::from_str::<marshaling_protocol::ClientEvent>(text) else {
        return;
    };

    match client_event {
        marshaling_protocol::ClientEvent::PermissionResponse {
            id,
            allowed,
            remember,
        } => {
            let (permission_broker, remembered_tool) = {
                let mut runs = state.runs.lock().await;
                let Some(run) = runs.get_mut(run_id) else {
                    return;
                };
                (
                    run.permission_broker.clone(),
                    run.pending_permission_tools
                        .remove(&id)
                        .map(|pending| pending.tool_name),
                )
            };
            if remember
                && allowed
                && let Some(tool_name) = remembered_tool
            {
                let mut sessions = state.runtime_states.lock().await;
                let sess = sessions.entry(runtime_session_key.to_string()).or_default();
                sess.remember_allow_tools.insert(tool_name);
            }
            if !permission_broker.resolve(&id, allowed) {
                debug!("Permission response for {id} had no waiting request");
            }
        }
        marshaling_protocol::ClientEvent::Cancel => {
            debug!("Client requested cancellation for run {run_id}");
            let cancel_tx = {
                let runs = state.runs.lock().await;
                runs.get(run_id).map(|run| run.cancel_tx.clone())
            };
            if let Some(cancel_tx) = cancel_tx {
                let _ = cancel_tx.send(true);
            }
        }
        marshaling_protocol::ClientEvent::RollbackLast {
            runtime_session_key: requested_key,
        } => {
            let key = requested_key.unwrap_or_else(|| runtime_session_key.to_string());
            let payload = apply_rollback_last(state, &key).await;
            let evt = marshaling_protocol::ServerEvent::RollbackResult {
                success: payload.success,
                message: payload.message,
                changes: payload.changes,
            };
            if let Ok(json) = serde_json::to_string(&evt) {
                let _ = socket.send(Message::Text(json.into())).await;
            }
        }
    }
}

/// Map an agent event to its client-facing form. Returns `None` for internal
/// bookkeeping events that clients never see.
pub(crate) fn agent_event_to_server_event(
    event: agent::AgentEvent,
) -> Option<marshaling_protocol::ServerEvent> {
    use agent::AgentEvent;
    let event = match event {
        AgentEvent::PermissionResolved { .. } | AgentEvent::MessagesCommitted(_) => return None,
        AgentEvent::Failed { error, .. } => marshaling_protocol::ServerEvent::Error {
            message: format!("{error:#}"),
        },
        AgentEvent::SubagentRetrying {
            id,
            reason,
            discarded_output,
        } => marshaling_protocol::ServerEvent::SubagentRetrying {
            id,
            reason,
            discarded_output,
        },
        AgentEvent::Retrying {
            attempt,
            max_attempts,
            delay,
            reason,
            discarded_output,
        } => marshaling_protocol::ServerEvent::Retrying {
            attempt: u32::try_from(attempt).unwrap_or(u32::MAX),
            max_attempts: u32::try_from(max_attempts).unwrap_or(u32::MAX),
            delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            reason,
            discarded_output,
        },
        AgentEvent::TextDelta(text) => marshaling_protocol::ServerEvent::TextDelta { data: text },
        AgentEvent::ReasoningDelta(text) => {
            marshaling_protocol::ServerEvent::ReasoningDelta { data: text }
        }
        AgentEvent::ToolStarted { id, name } => {
            marshaling_protocol::ServerEvent::ToolStarted { id, name }
        }
        AgentEvent::ToolCompleted {
            id,
            result,
            changes,
            ..
        } => marshaling_protocol::ServerEvent::ToolCompleted {
            id,
            result,
            changes,
        },
        AgentEvent::ToolFailed { id, error } => {
            marshaling_protocol::ServerEvent::ToolFailed { id, error }
        }
        AgentEvent::PermissionRequest {
            id,
            tool_name,
            args,
        } => marshaling_protocol::ServerEvent::PermissionRequest {
            id,
            tool_name,
            args,
        },
        AgentEvent::SkillsLoaded { names } => {
            marshaling_protocol::ServerEvent::SkillsLoaded { names }
        }
        AgentEvent::SkillSelected { name } => {
            marshaling_protocol::ServerEvent::SkillSelected { name }
        }
        AgentEvent::SubagentStarted { id, name } => {
            marshaling_protocol::ServerEvent::SubagentStarted { id, name }
        }
        AgentEvent::SubagentTextDelta { id, data } => {
            marshaling_protocol::ServerEvent::SubagentTextDelta { id, data }
        }
        AgentEvent::SubagentReasoningDelta { id, data } => {
            marshaling_protocol::ServerEvent::SubagentReasoningDelta { id, data }
        }
        AgentEvent::SubagentToolStarted {
            id,
            sub_id,
            tool_name,
        } => marshaling_protocol::ServerEvent::SubagentToolStarted {
            id,
            sub_id,
            tool_name,
        },
        AgentEvent::SubagentToolCompleted {
            id,
            sub_id,
            result,
            changes,
            ..
        } => marshaling_protocol::ServerEvent::SubagentToolCompleted {
            id,
            sub_id,
            result,
            changes,
        },
        AgentEvent::SubagentToolFailed { id, sub_id, error } => {
            marshaling_protocol::ServerEvent::SubagentToolFailed { id, sub_id, error }
        }
        AgentEvent::SubagentDone { id, content } => {
            marshaling_protocol::ServerEvent::SubagentDone { id, content }
        }
        AgentEvent::TurnDone { text, tool_calls } => {
            marshaling_protocol::ServerEvent::TurnDone { text, tool_calls }
        }
        AgentEvent::Done {
            content,
            tokens_input,
            tokens_output,
            ..
        } => marshaling_protocol::ServerEvent::Done {
            content,
            tokens_input,
            tokens_output,
        },
        AgentEvent::Cancelled {
            content,
            tokens_input,
            tokens_output,
            ..
        } => marshaling_protocol::ServerEvent::Cancelled {
            content,
            tokens_input,
            tokens_output,
        },
        AgentEvent::NeedsContinuation {
            content,
            tokens_input,
            tokens_output,
            ..
        } => marshaling_protocol::ServerEvent::NeedsContinuation {
            content,
            tokens_input,
            tokens_output,
        },
    };
    Some(event)
}
