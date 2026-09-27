//! Reattaching to an in-flight run after its websocket drops: retries
//! with backoff in the background, resuming from the client's position in
//! the run log.

use super::*;

/// Delay before each reattach attempt after the run's websocket drops. The
/// server keeps the run alive, so these only need to cover short outages.
pub(super) const REATTACH_DELAYS_MS: [u64; 6] = [0, 500, 1_000, 2_000, 4_000, 8_000];
/// Upper bound for one reattach attempt, so a server that accepts TCP but
/// never finishes the handshake cannot stall reconnection.
pub(super) const REATTACH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const REATTACH_STATUS: &str = "reconnecting to run… (Ctrl+C twice to stop waiting)";

/// Try to reattach to `run_id` after the delay for `attempt`, reporting the
/// outcome as a `BackgroundEvent::Reattached`. Runs in the background so the
/// UI stays responsive during the outage.
pub(super) fn schedule_reattach(
    client: &MoteClient,
    app: &App,
    run_id: String,
    attempt: usize,
    generation: u64,
    background_tx: &tokio::sync::mpsc::UnboundedSender<BackgroundEvent>,
) {
    let delay = Duration::from_millis(
        REATTACH_DELAYS_MS
            .get(attempt)
            .copied()
            .unwrap_or(REATTACH_DELAYS_MS[REATTACH_DELAYS_MS.len() - 1]),
    );
    let request = build_attach_request(app, run_id.clone());
    let client = client.clone();
    let tx = background_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let result = tokio::time::timeout(REATTACH_CONNECT_TIMEOUT, client.chat_stream(request))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("reattach timed out")));
        let _ = tx.send(BackgroundEvent::Reattached {
            run_id,
            generation,
            result,
        });
    });
}

#[allow(clippy::too_many_arguments)] // event-loop state it updates in place
pub(super) fn handle_reattach_result(
    client: &MoteClient,
    app: &mut App,
    run_id: String,
    generation: u64,
    result: anyhow::Result<ChatStream>,
    chat_stream: &mut Option<ChatStream>,
    reattach_attempt: &mut Option<usize>,
    background_tx: &tokio::sync::mpsc::UnboundedSender<BackgroundEvent>,
) {
    // Ignore stale results: the user may have started over (e.g. /new) or
    // the run may already be settled while this attempt was in flight.
    let Some(attempt) = *reattach_attempt else {
        return;
    };
    match app.active_run_id.as_deref() {
        // The run was abandoned (e.g. /new) or a stream already exists:
        // there is nothing left to reattach to.
        _ if chat_stream.is_some() => {
            *reattach_attempt = None;
            return;
        }
        None => {
            *reattach_attempt = None;
            return;
        }
        // A late result for some other run; the current run's own attempt
        // is still in flight.
        Some(current) if current != run_id => return,
        Some(_) => {}
    }
    match result {
        Ok(stream) => {
            *reattach_attempt = None;
            app.server_health = ServerHealth::Connected;
            *chat_stream = Some(stream);
        }
        Err(e) if attempt + 1 < REATTACH_DELAYS_MS.len() => {
            tracing::warn!(attempt, "reattach to run {run_id} failed: {e:#}");
            *reattach_attempt = Some(attempt + 1);
            schedule_reattach(client, app, run_id, attempt + 1, generation, background_tx);
        }
        Err(e) => {
            *reattach_attempt = None;
            app.server_health = ServerHealth::Disconnected(format!("{e:#}"));
            app.connection_lost(&format!(
                "Lost connection to the running agent ({e:#}). It may still be running on the server."
            ));
        }
    }
}

pub(super) fn build_attach_request(app: &App, run_id: String) -> marshaling_protocol::ChatRequest {
    let (model_override, provider_override) = app.current_model_override_parts();
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
        replay_from: Some(app.run_event_count),
        client_instance_id: Some(app.client_instance_id.clone()),
        compaction: app.compaction_state.clone(),
    }
}
