//! Session endpoints and transcript helpers: listing (with a cache),
//! loading, converting legacy `.md` sessions, and building the history a
//! run starts from.

use super::*;

pub(crate) type SessionInfoCache =
    HashMap<PathBuf, (std::time::SystemTime, u64, marshaling_protocol::SessionInfo)>;

/// Sessions in `dir`, newest first: transcripts, plus legacy `.md` sessions
/// that have not been converted yet. A transcript is only parsed when it is
/// new or changed since the last listing (`cache`, keyed by path and checked
/// against modification time and size).
pub(crate) fn session_infos(
    dir: &std::path::Path,
    cache: &std::sync::Mutex<SessionInfoCache>,
) -> Vec<marshaling_protocol::SessionInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut transcripts = HashSet::new();
    let mut found: Vec<(std::time::SystemTime, marshaling_protocol::SessionInfo)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = entry.metadata().ok();
        let stamp = metadata
            .as_ref()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        let (modified, size) = stamp.unwrap_or((std::time::UNIX_EPOCH, 0));
        let info = match path.extension().and_then(|e| e.to_str()) {
            Some("jsonl") => {
                transcripts.insert(path.clone());
                // Without a usable stamp the file cannot be checked for
                // changes, so it is always parsed and never cached.
                let cached = stamp.and_then(|_| {
                    cache
                        .get(&path)
                        .filter(|(time, len, _)| *time == modified && *len == size)
                        .map(|(_, _, info)| info.clone())
                });
                let info = match cached {
                    Some(info) => info,
                    None => match store::load(&path) {
                        Ok(transcript) => marshaling_protocol::SessionInfo {
                            id: transcript.id.clone(),
                            created: transcript.created.to_rfc3339(),
                            model: format!("{}/{}", transcript.model_provider, transcript.model_id),
                            message_count: transcript.display_messages().len(),
                            summary: transcript.summary(),
                        },
                        Err(e) => {
                            tracing::warn!("Skipping unreadable session: {e:#}");
                            continue;
                        }
                    },
                };
                if stamp.is_some() {
                    cache.insert(path.clone(), (modified, size, info.clone()));
                }
                info
            }
            Some("md") if !path.with_extension("jsonl").exists() => {
                match history::parse_file(&path) {
                    Ok((meta, messages)) => marshaling_protocol::SessionInfo {
                        id: meta.id,
                        created: meta.created.to_rfc3339(),
                        model: format!("{}/{}", meta.model_provider, meta.model_id),
                        message_count: messages.len(),
                        summary: meta.summary,
                    },
                    Err(_) => continue,
                }
            }
            _ => continue,
        };
        found.push((modified, info));
    }
    // Forget sessions of this directory that no longer exist.
    cache.retain(|path, _| path.parent() != Some(dir) || transcripts.contains(path));
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().map(|(_, info)| info).collect()
}

/// A session for display: from its transcript, or from a legacy `.md` file.
pub(crate) fn load_session_data(
    dir: &std::path::Path,
    id: &str,
) -> Option<marshaling_protocol::SessionData> {
    let path = store::transcript_path(dir, id);
    if path.exists() {
        let transcript = store::load(&path)
            .map_err(|e| tracing::warn!("Failed to load session {id}: {e:#}"))
            .ok()?;
        let messages = transcript
            .display_messages()
            .into_iter()
            .filter_map(|(role, content)| {
                Some(marshaling_protocol::HistoryMessage {
                    role: protocol_role_for_session(role)?.into(),
                    content,
                })
            })
            .collect();
        return Some(marshaling_protocol::SessionData {
            id: transcript.id.clone(),
            created: transcript.created.to_rfc3339(),
            model: format!("{}/{}", transcript.model_provider, transcript.model_id),
            compaction: protocol_compaction(&transcript),
            context_chars: Some(store::history_chars(&model_history(&transcript, None))),
            messages,
        });
    }
    let (meta, messages) = history::parse_file(&dir.join(format!("{id}.md"))).ok()?;
    Some(marshaling_protocol::SessionData {
        id: meta.id,
        created: meta.created.to_rfc3339(),
        model: format!("{}/{}", meta.model_provider, meta.model_id),
        messages: messages
            .into_iter()
            .filter_map(|m| {
                Some(marshaling_protocol::HistoryMessage {
                    role: protocol_role_for_session(m.role)?.into(),
                    content: m.content,
                })
            })
            .collect(),
        compaction: meta.compaction,
        context_chars: None,
    })
}

/// GET /sessions
pub(crate) async fn list_sessions(
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let Some(runtime_session_key) = runtime_session_key_from_headers(&headers) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let hist_dir = history_dir_for_session(&state.config.history.dir, &runtime_session_key);
    let cache = Arc::clone(&state.session_info_cache);
    let items = tokio::task::spawn_blocking(move || session_infos(&hist_dir, &cache))
        .await
        .unwrap_or_default();
    Ok(Json(items))
}

/// GET /sessions/:id — load a specific session.
pub(crate) async fn load_session(
    Path(id): Path<String>,
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<Json<marshaling_protocol::SessionData>, StatusCode> {
    if !validate_session_id(&id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let runtime_session_key =
        runtime_session_key_from_headers(&headers).ok_or(StatusCode::BAD_REQUEST)?;
    let dir = history_dir_for_session(&state.config.history.dir, &runtime_session_key);
    tokio::task::spawn_blocking(move || load_session_data(&dir, &id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// DELETE /sessions/:id — delete a saved session.
pub(crate) async fn delete_session(
    Path(id): Path<String>,
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> StatusCode {
    if !validate_session_id(&id) {
        return StatusCode::BAD_REQUEST;
    }
    let Some(runtime_session_key) = runtime_session_key_from_headers(&headers) else {
        return StatusCode::BAD_REQUEST;
    };
    let dir = history_dir_for_session(&state.config.history.dir, &runtime_session_key);
    // Deleting under a running run would let its next append recreate a
    // headerless (unreadable) transcript.
    let transcript = store::transcript_path(&dir, &id);
    // Hold the session lock so no run or compaction writes to (and no
    // legacy conversion recreates) the files while they are removed.
    let session_lock = state.session_lock(&transcript).await;
    let _guard = session_lock.lock().await;
    let busy = state
        .runs
        .lock()
        .await
        .values()
        .any(|run| !run.finished && run.session_path.as_deref() == Some(transcript.as_path()));
    if busy {
        return StatusCode::CONFLICT;
    }
    // Remove the transcript and any legacy `.md` copy of the session.
    let paths = [
        store::transcript_path(&dir, &id),
        dir.join(format!("{id}.md")),
    ];
    let result = tokio::task::spawn_blocking(move || {
        let mut found = false;
        for path in paths.iter().filter(|p| p.exists()) {
            found = true;
            if std::fs::remove_file(path).is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        }
        if found {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        }
    })
    .await
    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if result == StatusCode::OK {
        tracing::info!("Deleted session: {id}");
    }
    result
}

/// A run's view of its session when it starts.
pub(crate) struct OpenedSession {
    pub(crate) writer: store::TranscriptWriter,
    /// The history the model starts from.
    pub(crate) history: Vec<llm::ChatMessage>,
}

/// Open the transcript a run appends to. Blocking; run under the session
/// lock.
pub(crate) fn open_run_session(
    dir: PathBuf,
    session_id: String,
    path: PathBuf,
    is_new_session: bool,
    model_provider: String,
    model_id: String,
) -> Result<OpenedSession> {
    if is_new_session {
        let writer = store::TranscriptWriter::create(path, session_id, model_provider, model_id);
        return Ok(OpenedSession {
            writer,
            history: Vec::new(),
        });
    }
    let transcript = load_or_convert_session(&dir, &session_id)?;
    Ok(OpenedSession {
        history: model_history(&transcript, Some(&model_provider)),
        writer: store::TranscriptWriter::resume(path, &transcript),
    })
}

/// Load session `id`, converting a legacy text-only `.md` session into a
/// transcript on first use (the `.md` file is left untouched). Blocking;
/// run under the session lock.
///
/// Every loader that continues or compacts a session goes through here, so
/// they all see the repaired transcript (see `repair_unanswered_calls`).
pub(crate) fn load_or_convert_session(
    dir: &std::path::Path,
    id: &str,
) -> Result<store::Transcript> {
    let path = store::transcript_path(dir, id);
    if !path.exists() {
        let legacy = dir.join(format!("{id}.md"));
        if !legacy.exists() {
            anyhow::bail!("Unknown session: {id}");
        }
        let (meta, messages) = history::parse_file(&legacy)?;
        store::append(&path, &store::legacy_records(id, &meta, &messages), true)?;
        tracing::info!("Converted legacy session {id} to {}", path.display());
    }
    repair_unanswered_calls(&path, store::load(&path)?)
}

/// A crash can cut a step short, leaving tool calls without results. Answer
/// them on disk, right after that step, before anything else is appended or
/// compacted: an in-memory fix would stop applying once later messages
/// follow it, and a compaction covering the step would orphan a result
/// added afterwards. Blocking; run under the session lock.
pub(crate) fn repair_unanswered_calls(
    path: &std::path::Path,
    transcript: store::Transcript,
) -> Result<store::Transcript> {
    let mut messages: Vec<llm::ChatMessage> = transcript
        .messages
        .iter()
        .map(|m| m.message.clone())
        .collect();
    let stored = messages.len();
    agent::close_unanswered_tool_calls(&mut messages, 0);
    if messages.len() == stored {
        return Ok(transcript);
    }
    tracing::warn!(
        "Answering {} tool call(s) left unanswered in {}",
        messages.len() - stored,
        path.display()
    );
    store::TranscriptWriter::resume(path.to_path_buf(), &transcript)
        .append_messages(&messages[stored..])?;
    store::load(path)
}

/// The history the model sees: the compaction summary (if any), then every
/// message after the compacted range.
///
/// `provider` is the provider about to receive it: when it differs from
/// the one the session last ran with, earlier reasoning is dropped, as it
/// is provider-specific (and some APIs reject foreign reasoning fields).
pub(crate) fn model_history(
    transcript: &store::Transcript,
    provider: Option<&str>,
) -> Vec<llm::ChatMessage> {
    let mut history = Vec::new();
    if let Some(compaction) = protocol_compaction(transcript) {
        history.push(compaction_context_message(&compaction));
    }
    let switched = provider.is_some_and(|p| p != transcript.model_provider);
    history.extend(
        transcript
            .uncompacted_messages()
            .into_iter()
            .map(|mut message| {
                if switched {
                    message.reasoning_content = None;
                }
                message
            }),
    );
    // Steps are written whole, but a crash mid-write can still lose the tail
    // of one: answer any trailing unanswered tool calls so the history is
    // valid for providers.
    agent::close_unanswered_tool_calls(&mut history, 0);
    history
}

/// The transcript's compaction as clients see it; the message count is in
/// display messages.
pub(crate) fn protocol_compaction(
    transcript: &store::Transcript,
) -> Option<marshaling_protocol::CompactionState> {
    transcript
        .compaction
        .as_ref()
        .map(|compaction| marshaling_protocol::CompactionState {
            summary: compaction.summary.clone(),
            compacted_message_count: transcript.compacted_display_count(),
            model_provider: compaction.model_provider.clone(),
            model_id: compaction.model_id.clone(),
        })
}
