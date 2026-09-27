//! Server-side conversation transcripts.
//!
//! Each session is one append-only JSONL file,
//! `{history_dir}/{client_key}/{session_id}.jsonl`. Records are never
//! rewritten, only appended, so a crash mid-write can at worst leave a
//! truncated final line (ignored on load). The transcript holds the full
//! `ChatMessage`s — tool calls, tool results, reasoning — so every run
//! continues from exactly what the model saw, not from display text.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::llm::{ChatMessage, Role};

/// One line of a transcript file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    /// Always the first record.
    Session {
        id: String,
        created: DateTime<Utc>,
        model_provider: String,
        model_id: String,
        version: String,
    },
    Message {
        seq: u64,
        ts: DateTime<Utc>,
        message: ChatMessage,
        /// `ChatMessage::internal_role_task` is never serialized with the
        /// message (it must not reach providers), so it is kept here.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        internal_role_task: bool,
    },
    /// Appended when a run ends; token counts are per run.
    RunEnd {
        ts: DateTime<Utc>,
        tokens_input: u64,
        tokens_output: u64,
        model_provider: String,
        model_id: String,
    },
    /// Messages up to and including `upto_seq` are replaced by `summary`
    /// when building the model's history.
    Compaction {
        ts: DateTime<Utc>,
        upto_seq: u64,
        summary: String,
        model_provider: String,
        model_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub seq: u64,
    pub message: ChatMessage,
}

#[derive(Debug, Clone)]
pub struct StoredCompaction {
    pub upto_seq: u64,
    pub summary: String,
    pub model_provider: String,
    pub model_id: String,
}

/// A loaded transcript: the fold of all its records.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub id: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub model_provider: String,
    pub model_id: String,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub messages: Vec<StoredMessage>,
    pub compaction: Option<StoredCompaction>,
}

/// Approximate size of messages as sent to a model: text, reasoning, and
/// tool-call names and arguments.
pub fn history_chars(messages: &[ChatMessage]) -> u64 {
    messages
        .iter()
        .map(|m| {
            let calls: usize = m
                .tool_calls
                .iter()
                .flatten()
                .map(|c| c.function.name.len() + c.function.arguments.len())
                .sum();
            m.content.as_deref().map_or(0, str::len)
                + m.reasoning_content.as_deref().map_or(0, str::len)
                + calls
        })
        .sum::<usize>() as u64
}

pub fn transcript_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.jsonl"))
}

pub fn new_session_id() -> String {
    format!("chat-{}", Utc::now().format("%Y%m%d-%H%M%S%6f"))
}

/// A user-authored message (not a tool result, role hand-off, or summary).
fn is_user_text(message: &ChatMessage) -> bool {
    message.role == Role::User
        && message.tool_call_id.is_none()
        && !message.internal_role_task
}

impl Transcript {
    /// Sequence number the next appended message gets.
    pub fn next_seq(&self) -> u64 {
        self.messages.last().map_or(0, |m| m.seq + 1)
    }

    /// Short summary: the start of the first user message.
    pub fn summary(&self) -> Option<String> {
        self.messages
            .iter()
            .map(|m| &m.message)
            .filter(|m| is_user_text(m))
            .find_map(|m| {
                m.content
                    .as_deref()
                    .and_then(crate::session::summary_from_user_content)
            })
    }

    /// The chat as a person reads it: user text and non-empty assistant
    /// text, in order. Tool calls and results are omitted.
    pub fn display_messages(&self) -> Vec<(Role, String)> {
        display_messages(self.messages.iter().map(|m| &m.message))
    }

    /// How many display messages the current compaction covers.
    pub fn compacted_display_count(&self) -> usize {
        let Some(compaction) = &self.compaction else {
            return 0;
        };
        display_messages(
            self.messages
                .iter()
                .take_while(|m| m.seq <= compaction.upto_seq)
                .map(|m| &m.message),
        )
        .len()
    }

    /// Messages the model still sees verbatim: everything after the
    /// compacted range.
    pub fn uncompacted_messages(&self) -> Vec<ChatMessage> {
        let after = self.compaction.as_ref().map(|c| c.upto_seq);
        self.messages
            .iter()
            .filter(|m| after.is_none_or(|upto| m.seq > upto))
            .map(|m| m.message.clone())
            .collect()
    }
}

fn display_messages<'a>(
    messages: impl Iterator<Item = &'a ChatMessage>,
) -> Vec<(Role, String)> {
    messages
        .filter_map(|m| {
            let content = m.content.as_deref().filter(|c| !c.is_empty())?;
            match m.role {
                Role::User if is_user_text(m) => {
                    Some((Role::User, content.to_string()))
                }
                Role::Assistant => Some((Role::Assistant, content.to_string())),
                _ => None,
            }
        })
        .collect()
}

/// Load a transcript. A malformed final line (a write cut short by a crash)
/// is skipped; malformed lines elsewhere are an error.
pub fn load(path: &Path) -> Result<Transcript> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let lines: Vec<&str> =
        text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut records = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str::<Record>(line) {
            Ok(record) => records.push(record),
            Err(e) if i + 1 == lines.len() => {
                tracing::warn!(
                    "ignoring truncated last record in {}: {e}",
                    path.display()
                );
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Corrupt record {} in {}", i + 1, path.display())
                });
            }
        }
    }
    fold(records).with_context(|| format!("Invalid {}", path.display()))
}

fn fold(records: Vec<Record>) -> Result<Transcript> {
    let mut records = records.into_iter();
    let Some(Record::Session {
        id,
        created,
        model_provider,
        model_id,
        ..
    }) = records.next()
    else {
        anyhow::bail!("transcript must start with a session record");
    };
    let mut transcript = Transcript {
        id,
        created,
        updated: created,
        model_provider,
        model_id,
        tokens_input: 0,
        tokens_output: 0,
        messages: Vec::new(),
        compaction: None,
    };
    for record in records {
        match record {
            Record::Session { .. } => {
                anyhow::bail!("duplicate session record");
            }
            Record::Message {
                seq,
                ts,
                mut message,
                internal_role_task,
            } => {
                message.internal_role_task = internal_role_task;
                transcript.messages.push(StoredMessage { seq, message });
                transcript.updated = ts;
            }
            Record::RunEnd {
                ts,
                tokens_input,
                tokens_output,
                model_provider,
                model_id,
            } => {
                transcript.tokens_input += tokens_input;
                transcript.tokens_output += tokens_output;
                transcript.model_provider = model_provider;
                transcript.model_id = model_id;
                transcript.updated = ts;
            }
            Record::Compaction {
                ts,
                upto_seq,
                summary,
                model_provider,
                model_id,
            } => {
                transcript.compaction = Some(StoredCompaction {
                    upto_seq,
                    summary,
                    model_provider,
                    model_id,
                });
                transcript.updated = ts;
            }
        }
    }
    Ok(transcript)
}

/// Append records in one write.
///
/// `create` must be true only for a write that starts with the session
/// record: appending to a missing file (e.g. a session deleted meanwhile)
/// is an error rather than a headerless, unreadable transcript. A partial
/// final line left by an earlier interrupted write is dropped first, so new
/// records never merge into it.
pub fn append(path: &Path, records: &[Record], create: bool) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut buf = String::new();
    for record in records {
        buf.push_str(&serde_json::to_string(record)?);
        buf.push('\n');
    }
    if create {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(create)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    drop_partial_last_line(&mut file)
        .with_context(|| format!("Failed to repair {}", path.display()))?;
    // All or nothing: a write that fails partway (disk full, I/O error)
    // is rolled back, so a retry never duplicates records or the header.
    let len_before = file.metadata()?.len();
    if let Err(e) = file.write_all(buf.as_bytes()) {
        let _ = file.set_len(len_before);
        return Err(e)
            .with_context(|| format!("Failed to write {}", path.display()));
    }
    Ok(())
}

/// Truncate the file back to its last newline if it does not end with one.
fn drop_partial_last_line(file: &mut std::fs::File) -> Result<()> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    // Rare (only after a crash mid-write), so reading the file is fine.
    let mut content = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_end(&mut content)?;
    let keep = content
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    tracing::warn!(
        "dropping {} bytes of a partial transcript record",
        content.len() - keep
    );
    file.set_len(keep as u64)?;
    Ok(())
}

/// Records for a fresh transcript converted from a legacy text-only `.md`
/// session.
pub fn legacy_records(
    id: &str,
    meta: &crate::session::SessionMeta,
    messages: &[crate::session::Message],
) -> Vec<Record> {
    let mut records = vec![Record::Session {
        id: id.to_string(),
        created: meta.created,
        model_provider: meta.model_provider.clone(),
        model_id: meta.model_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }];
    let mut seq = 0;
    for message in messages {
        let chat = match message.role {
            Role::User => ChatMessage::user(&message.content),
            Role::Assistant => ChatMessage::assistant_text(&message.content),
            _ => continue,
        };
        records.push(Record::Message {
            seq,
            ts: message.timestamp,
            message: chat,
            internal_role_task: false,
        });
        seq += 1;
    }
    // Old files kept every message and marked the compacted prefix by count;
    // messages map 1:1 onto seqs here, so the count becomes `upto_seq`.
    if let Some(compaction) = &meta.compaction {
        let covered = compaction.compacted_message_count.min(seq as usize);
        if covered > 0 {
            records.push(Record::Compaction {
                ts: meta.updated,
                upto_seq: covered as u64 - 1,
                summary: compaction.summary.clone(),
                model_provider: compaction.model_provider.clone(),
                model_id: compaction.model_id.clone(),
            });
        }
    }
    records.push(Record::RunEnd {
        ts: meta.updated,
        tokens_input: meta.tokens_input,
        tokens_output: meta.tokens_output,
        model_provider: meta.model_provider.clone(),
        model_id: meta.model_id.clone(),
    });
    records
}

/// Appends a run's messages to its session transcript as they are
/// committed. The session header is written with the first append, so a
/// run that fails before producing anything leaves no empty file behind.
pub struct TranscriptWriter {
    path: PathBuf,
    pending_header: Option<Record>,
    next_seq: u64,
    /// Messages whose write failed; retried before the next write so a
    /// transient I/O error does not drop part of the conversation.
    unsaved: Vec<ChatMessage>,
}

impl TranscriptWriter {
    /// Continue an existing transcript.
    pub fn resume(path: PathBuf, transcript: &Transcript) -> Self {
        Self {
            path,
            pending_header: None,
            next_seq: transcript.next_seq(),
            unsaved: Vec::new(),
        }
    }

    /// Start a new transcript for session `id`.
    pub fn create(
        path: PathBuf,
        id: String,
        model_provider: String,
        model_id: String,
    ) -> Self {
        Self {
            path,
            pending_header: Some(Record::Session {
                id,
                created: Utc::now(),
                model_provider,
                model_id,
                version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            next_seq: 0,
            unsaved: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append committed messages (after any earlier ones whose write
    /// failed). On failure they are kept and retried with the next write.
    pub fn append_messages(&mut self, messages: &[ChatMessage]) -> Result<()> {
        self.unsaved.extend_from_slice(messages);
        self.flush_unsaved()
    }

    fn flush_unsaved(&mut self) -> Result<()> {
        let ts = Utc::now();
        let records: Vec<Record> = self
            .unsaved
            .iter()
            .enumerate()
            .map(|(i, message)| Record::Message {
                seq: self.next_seq + i as u64,
                ts,
                message: message.clone(),
                internal_role_task: message.internal_role_task,
            })
            .collect();
        self.write(records)?;
        self.next_seq += self.unsaved.len() as u64;
        self.unsaved.clear();
        Ok(())
    }

    /// Append a record other than a message (run end, compaction), after
    /// any unsaved messages so records stay in order.
    pub fn append_record(&mut self, record: Record) -> Result<()> {
        self.flush_unsaved()?;
        self.write(vec![record])
    }

    fn write(&mut self, records: Vec<Record>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut all = Vec::with_capacity(records.len() + 1);
        if let Some(header) = self.pending_header.clone() {
            all.push(header);
        }
        all.extend(records);
        append(&self.path, &all, self.pending_header.is_some())?;
        self.pending_header = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ToolCall, ToolFunction};

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: ToolFunction {
                name: "read".into(),
                arguments: r#"{"file_path":"a"}"#.into(),
            },
        }
    }

    fn sample_run() -> Vec<ChatMessage> {
        let mut with_reasoning = ChatMessage::assistant_tool_calls_with_content(
            vec![tool_call("c1")],
            Some("Let me look.".into()),
        );
        with_reasoning.reasoning_content = Some("thinking".into());
        vec![
            ChatMessage::user("fix the bug"),
            with_reasoning,
            ChatMessage::tool_result("c1", "file contents"),
            ChatMessage::role_task("hand off"),
            ChatMessage::assistant_text("Done."),
        ]
    }

    #[test]
    fn writer_roundtrips_full_fidelity_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_path(dir.path(), "chat-1");
        let mut writer = TranscriptWriter::create(
            path.clone(),
            "chat-1".into(),
            "deepseek".into(),
            "deepseek-chat".into(),
        );
        // Nothing is written until there is something to write.
        writer.append_messages(&[]).unwrap();
        assert!(!path.exists());

        let run = sample_run();
        writer.append_messages(&run[..2]).unwrap();
        writer.append_messages(&run[2..]).unwrap();
        writer
            .append_record(Record::RunEnd {
                ts: Utc::now(),
                tokens_input: 10,
                tokens_output: 5,
                model_provider: "deepseek".into(),
                model_id: "deepseek-chat".into(),
            })
            .unwrap();

        let transcript = load(&path).unwrap();
        assert_eq!(transcript.id, "chat-1");
        assert_eq!(transcript.tokens_input, 10);
        let seqs: Vec<u64> =
            transcript.messages.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, [0, 1, 2, 3, 4]);
        let loaded: Vec<ChatMessage> = transcript.uncompacted_messages();
        assert_eq!(loaded[1].tool_calls.as_ref().unwrap()[0].id, "c1");
        assert_eq!(loaded[1].reasoning_content.as_deref(), Some("thinking"));
        assert_eq!(loaded[2].tool_call_id.as_deref(), Some("c1"));
        assert!(loaded[3].internal_role_task);
        assert_eq!(transcript.summary().as_deref(), Some("fix the bug"));

        // Display view: user text and assistant text only.
        let display = transcript.display_messages();
        assert_eq!(
            display,
            [
                (Role::User, "fix the bug".to_string()),
                (Role::Assistant, "Let me look.".to_string()),
                (Role::Assistant, "Done.".to_string()),
            ]
        );

        // Resuming continues the sequence.
        let mut writer = TranscriptWriter::resume(path.clone(), &transcript);
        writer
            .append_messages(&[ChatMessage::user("again")])
            .unwrap();
        assert_eq!(load(&path).unwrap().next_seq(), 6);
    }

    #[test]
    fn compaction_hides_covered_messages_from_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_path(dir.path(), "chat-1");
        let mut writer = TranscriptWriter::create(
            path.clone(),
            "chat-1".into(),
            "p".into(),
            "m".into(),
        );
        writer.append_messages(&sample_run()).unwrap();
        writer
            .append_record(Record::Compaction {
                ts: Utc::now(),
                upto_seq: 2,
                summary: "earlier work".into(),
                model_provider: "p".into(),
                model_id: "m".into(),
            })
            .unwrap();
        let transcript = load(&path).unwrap();
        let visible = transcript.uncompacted_messages();
        assert_eq!(visible.len(), 2);
        assert!(visible[0].internal_role_task);
        assert_eq!(transcript.compacted_display_count(), 2);
        assert_eq!(
            transcript.compaction.as_ref().map(|c| c.summary.as_str()),
            Some("earlier work")
        );
    }

    #[test]
    fn load_skips_truncated_last_line_but_rejects_corruption_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_path(dir.path(), "chat-1");
        let mut writer = TranscriptWriter::create(
            path.clone(),
            "chat-1".into(),
            "p".into(),
            "m".into(),
        );
        writer
            .append_messages(&[ChatMessage::user("hello")])
            .unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(r#"{"type":"message","seq":1,"ts":"20"#);
        std::fs::write(&path, &text).unwrap();
        assert_eq!(load(&path).unwrap().messages.len(), 1);

        text.push_str("\n{\"type\":\"run_end\"}\n");
        std::fs::write(&path, &text).unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn legacy_markdown_converts_to_text_transcript() {
        let (meta, messages) = crate::history::parse(
            "---\nid: chat-old\ncreated: 2026-05-25T23:36:58Z\nupdated: 2026-05-25T23:40:00Z\nmodel_provider: ollama\nmodel_id: qwen\ntokens_input: 10\ntokens_output: 5\nversion: 0.1.0\n---\n\n## User — 23:36:58\nWhat is 99-1?\n\n## Assistant — 23:36:59\n98\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_path(dir.path(), "chat-old");
        append(&path, &legacy_records("chat-old", &meta, &messages), true)
            .unwrap();
        let transcript = load(&path).unwrap();
        assert_eq!(transcript.id, "chat-old");
        assert_eq!(transcript.tokens_input, 10);
        assert_eq!(
            transcript.display_messages(),
            [
                (Role::User, "What is 99-1?".to_string()),
                (Role::Assistant, "98".to_string()),
            ]
        );
        assert_eq!(transcript.next_seq(), 2);
    }

    #[test]
    fn append_after_crash_drops_partial_line_and_stays_loadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_path(dir.path(), "chat-1");
        let mut writer = TranscriptWriter::create(
            path.clone(),
            "chat-1".into(),
            "p".into(),
            "m".into(),
        );
        writer
            .append_messages(&[ChatMessage::user("hello")])
            .unwrap();
        // Simulate a crash in the middle of writing the next record.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(r#"{"type":"message","seq":1,"ts":"20"#);
        std::fs::write(&path, &text).unwrap();

        let transcript = load(&path).unwrap();
        let mut writer = TranscriptWriter::resume(path.clone(), &transcript);
        writer
            .append_messages(&[ChatMessage::assistant_text("hi")])
            .unwrap();
        writer
            .append_messages(&[ChatMessage::user("again")])
            .unwrap();
        let transcript = load(&path).unwrap();
        let seqs: Vec<u64> =
            transcript.messages.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, [0, 1, 2]);
        assert_eq!(
            transcript.messages[1].message.content.as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn appends_never_recreate_a_deleted_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_path(dir.path(), "chat-1");
        let mut writer = TranscriptWriter::create(
            path.clone(),
            "chat-1".into(),
            "p".into(),
            "m".into(),
        );
        writer
            .append_messages(&[ChatMessage::user("hello")])
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(
            writer
                .append_messages(&[ChatMessage::assistant_text("hi")])
                .is_err()
        );
        assert!(!path.exists(), "no headerless file is created");
        assert!(append(&path, &[], false).is_ok());
    }

    #[test]
    fn failed_writes_are_retried_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_path(dir.path(), "chat-1");
        let mut writer = TranscriptWriter::create(
            path.clone(),
            "chat-1".into(),
            "p".into(),
            "m".into(),
        );
        writer.append_messages(&[ChatMessage::user("one")]).unwrap();
        // Make the next write fail by moving the file away...
        let aside = dir.path().join("aside");
        std::fs::rename(&path, &aside).unwrap();
        assert!(
            writer
                .append_messages(&[ChatMessage::assistant_text("two")])
                .is_err()
        );
        // ...then restore it: the failed message is written first.
        std::fs::rename(&aside, &path).unwrap();
        writer
            .append_messages(&[ChatMessage::user("three")])
            .unwrap();
        let contents: Vec<String> = load(&path)
            .unwrap()
            .messages
            .iter()
            .filter_map(|m| m.message.content.clone())
            .collect();
        assert_eq!(contents, ["one", "two", "three"]);
    }

    #[test]
    fn legacy_compaction_is_preserved() {
        let (meta, messages) = crate::history::parse(
            "---\nid: chat-old\ncreated: 2026-05-25T23:36:58Z\nupdated: 2026-05-25T23:40:00Z\nmodel_provider: ollama\nmodel_id: qwen\ntokens_input: 1\ntokens_output: 1\nversion: 0.1.0\ncompaction:\n  summary: early stuff\n  compacted_message_count: 2\n  model_provider: ollama\n  model_id: qwen\n---\n\n## User — 23:36:58\na\n\n## Assistant — 23:36:59\nb\n\n## User — 23:37:00\nc\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_path(dir.path(), "chat-old");
        append(&path, &legacy_records("chat-old", &meta, &messages), true)
            .unwrap();
        let transcript = load(&path).unwrap();
        let compaction = transcript.compaction.as_ref().unwrap();
        assert_eq!(compaction.upto_seq, 1);
        assert_eq!(compaction.summary, "early stuff");
        assert_eq!(transcript.uncompacted_messages().len(), 1);
        assert_eq!(transcript.compacted_display_count(), 2);
    }
}
