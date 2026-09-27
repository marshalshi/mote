//! Undoing the last tracked file change-set, and telling the model.

use super::*;

#[derive(Debug, Clone)]
pub(crate) struct RollbackChangeSet {
    pub(crate) id: String,
    pub(crate) tool_name: String,
    pub(crate) entries: Vec<llm::RollbackEntry>,
    pub(crate) display_changes: Vec<marshaling_protocol::FileChange>,
    /// Transcript of the session that made the change; a rollback is noted
    /// there so the model knows the edit was undone.
    pub(crate) transcript: Option<PathBuf>,
}

/// POST /rollback/last — rollback latest tracked file change-set.
pub(crate) async fn rollback_last_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(payload): Json<marshaling_protocol::RollbackLastRequest>,
) -> impl IntoResponse {
    Json(apply_rollback_last(&state, &payload.runtime_session_key).await)
}

pub(crate) fn hash64(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    h.finish()
}

pub(crate) async fn apply_rollback_last(
    state: &Arc<AppState>,
    runtime_session_key: &str,
) -> marshaling_protocol::RollbackResultPayload {
    let cs = {
        let mut sessions = state.runtime_states.lock().await;
        let Some(session) = sessions.get_mut(runtime_session_key) else {
            return marshaling_protocol::RollbackResultPayload {
                success: false,
                message: "No reversible changes available in this session."
                    .into(),
                changes: Vec::new(),
            };
        };
        match session.rollback_journal.last() {
            Some(v) => v.clone(),
            None => {
                return marshaling_protocol::RollbackResultPayload {
                    success: false,
                    message: "No reversible changes available in this session."
                        .into(),
                    changes: Vec::new(),
                };
            }
        }
    };

    // Preflight conflict checks
    for entry in &cs.entries {
        match entry.kind {
            llm::RollbackKind::Modified => {
                let content = match tokio::fs::read_to_string(&entry.path).await
                {
                    Ok(c) => c,
                    Err(e) => {
                        return marshaling_protocol::RollbackResultPayload {
                            success: false,
                            message: format!(
                                "Rollback blocked: failed to read {}: {e}",
                                entry.path.display()
                            ),
                            changes: Vec::new(),
                        };
                    }
                };
                if Some(hash64(&content)) != entry.expected_after_hash {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback blocked: {} changed since the original edit.",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
            }
            llm::RollbackKind::Added => {
                if !entry.path.exists() {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback blocked: {} is already missing.",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
                let content = match tokio::fs::read_to_string(&entry.path).await
                {
                    Ok(c) => c,
                    Err(e) => {
                        return marshaling_protocol::RollbackResultPayload {
                            success: false,
                            message: format!(
                                "Rollback blocked: failed to read {}: {e}",
                                entry.path.display()
                            ),
                            changes: Vec::new(),
                        };
                    }
                };
                if Some(hash64(&content)) != entry.expected_after_hash {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback blocked: {} changed since it was added.",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
            }
            llm::RollbackKind::Removed => {
                if entry.path.exists() {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback blocked: {} already exists.",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
            }
        }
    }

    // Apply rollback
    for entry in &cs.entries {
        match entry.kind {
            llm::RollbackKind::Modified => {
                if let Some(before) = &entry.before_content
                    && let Err(e) = tokio::fs::write(&entry.path, before).await
                {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback failed writing {}: {e}",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
            }
            llm::RollbackKind::Added => {
                if let Err(e) = tokio::fs::remove_file(&entry.path).await {
                    return marshaling_protocol::RollbackResultPayload {
                        success: false,
                        message: format!(
                            "Rollback failed removing {}: {e}",
                            entry.path.display()
                        ),
                        changes: Vec::new(),
                    };
                }
            }
            llm::RollbackKind::Removed => {
                if let Some(before) = &entry.before_content {
                    if let Some(parent) = entry.path.parent()
                        && let Err(e) = tokio::fs::create_dir_all(parent).await
                    {
                        return marshaling_protocol::RollbackResultPayload {
                            success: false,
                            message: format!(
                                "Rollback failed creating {}: {e}",
                                parent.display()
                            ),
                            changes: Vec::new(),
                        };
                    }
                    if let Err(e) = tokio::fs::write(&entry.path, before).await
                    {
                        return marshaling_protocol::RollbackResultPayload {
                            success: false,
                            message: format!(
                                "Rollback failed restoring {}: {e}",
                                entry.path.display()
                            ),
                            changes: Vec::new(),
                        };
                    }
                }
            }
        }
    }

    {
        let mut sessions = state.runtime_states.lock().await;
        if let Some(session) = sessions.get_mut(runtime_session_key) {
            session.rollback_journal.pop();
        }
    }
    note_rollback_in_transcript(state, &cs).await;

    marshaling_protocol::RollbackResultPayload {
        success: true,
        message: format!("Rolled back {} ({})", cs.id, cs.tool_name),
        changes: cs.display_changes,
    }
}

/// Tell the model that the user undid an edit: otherwise the stored tool
/// results still claim it happened. If a run is active on the session the
/// note goes into that run (before its next step); otherwise it is appended
/// to the transcript. Deciding under the session lock closes the race with
/// a run that is just starting: a run claims its slot before it takes the
/// lock to open the transcript, so it is either seen here as running or
/// opens the transcript after the note was written.
pub(crate) async fn note_rollback_in_transcript(
    state: &Arc<AppState>,
    cs: &RollbackChangeSet,
) {
    let Some(path) = cs.transcript.clone() else {
        return;
    };
    let files: Vec<String> = cs
        .entries
        .iter()
        .map(|entry| entry.path.display().to_string())
        .collect();
    let note = format!(
        "[mote] The user rolled back the changes made by the '{}' tool call ({}) to: {}. Those edits are no longer in the workspace; re-read the files before relying on them.",
        cs.tool_name,
        cs.id,
        files.join(", ")
    );
    let lock = state.session_lock(&path).await;
    let _guard = lock.lock().await;
    let delivered = {
        let mut runs = state.runs.lock().await;
        match runs.values_mut().find(|run| {
            !run.finished
                && run.accepting_notes
                && run.session_path.as_deref() == Some(path.as_path())
        }) {
            Some(run) => {
                // The loop may already have stopped taking notes; then the
                // forwarder writes this one when the run ends.
                if let Err(unsent) = run.notes_tx.send(note.clone()) {
                    run.pending_notes.push(unsent.0);
                }
                true
            }
            None => false,
        }
    };
    if delivered {
        return;
    }
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let transcript = store::load(&path)?;
        store::TranscriptWriter::resume(path, &transcript)
            .append_messages(&[llm::ChatMessage::internal_note(note)])
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!("Failed to note rollback in transcript: {e:#}")
        }
        Err(e) => tracing::warn!("Rollback note task failed: {e}"),
    }
}
