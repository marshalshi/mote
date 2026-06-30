use anyhow::{Context, Result};
use async_trait::async_trait;
use marshaling_protocol::{DiffLine, DiffLineKind, FileChange, FileChangeKind};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use tokio::fs;

use crate::llm::{
    RollbackEntry, RollbackKind, Tool, ToolDef, ToolExecutionResult,
    ToolFunctionDef,
};

/// Maximum bytes returned from tool output before truncation.
const MAX_OUTPUT_BYTES: usize = 51200; // 50 KiB

/// Truncate tool output if it exceeds MAX_OUTPUT_BYTES.
fn truncate_output(output: String) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output;
    }
    let truncated = crate::agent::safe_truncate(&output, MAX_OUTPUT_BYTES);
    format!(
        "{}\n\n[output truncated — {} bytes total]",
        truncated,
        output.len()
    )
}

const MAX_DIFF_LINES_PER_FILE: usize = 40;
const DIFF_CONTEXT_LINES: usize = 3;

fn result_no_changes(output: String) -> ToolExecutionResult {
    ToolExecutionResult {
        output,
        changes: Vec::new(),
        rollback_entries: Vec::new(),
    }
}

fn content_hash64(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    h.finish()
}

fn compute_modified_file_change(
    path: &std::path::Path,
    before: &str,
    after: &str,
) -> Option<FileChange> {
    if before == after {
        return None;
    }
    let mut lines: Vec<DiffLine> = Vec::new();
    let mut truncated = false;
    let diff = similar::TextDiff::from_lines(before, after);
    'groups: for group in diff.grouped_ops(DIFF_CONTEXT_LINES) {
        for op in group {
            for change in diff.iter_changes(&op) {
                if lines.len() >= MAX_DIFF_LINES_PER_FILE {
                    truncated = true;
                    break 'groups;
                }
                let kind = match change.tag() {
                    similar::ChangeTag::Delete => DiffLineKind::Removed,
                    similar::ChangeTag::Insert => DiffLineKind::Added,
                    similar::ChangeTag::Equal => DiffLineKind::Context,
                };
                let mut content = change.to_string();
                if content.ends_with('\n') {
                    content.pop();
                }
                lines.push(DiffLine { kind, content });
            }
        }
    }
    Some(FileChange {
        path: path.to_string_lossy().to_string(),
        kind: FileChangeKind::Modified,
        diff_lines: lines,
        truncated,
    })
}

// ── Context available to all built-in tools ───────────────

#[derive(Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
}

/// Validate that a resolved path is within the workspace directory.
/// Returns the canonicalized path if valid, or an error describing the violation.
fn validate_path_in_workspace(
    resolved: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<PathBuf> {
    let ws_canon = if workspace.exists() {
        workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf())
    } else {
        workspace.to_path_buf()
    };

    // For non-existent targets, canonicalize the nearest existing ancestor and
    // re-append the non-existent suffix. This prevents escapes like
    // /tmp/outside/new/file when parent doesn't exist yet.
    let canonical = if resolved.exists() {
        resolved.canonicalize().with_context(|| {
            format!("Failed to canonicalize {}", resolved.display())
        })?
    } else {
        let mut ancestor = resolved;
        while !ancestor.exists() {
            ancestor = ancestor.parent().with_context(|| {
                format!("Invalid path: {}", resolved.display())
            })?;
        }
        let canon_ancestor = ancestor.canonicalize().with_context(|| {
            format!("Failed to canonicalize {}", ancestor.display())
        })?;
        let suffix = resolved
            .strip_prefix(ancestor)
            .unwrap_or(std::path::Path::new(""));
        canon_ancestor.join(suffix)
    };

    if !canonical.starts_with(&ws_canon) {
        anyhow::bail!(
            "Path '{}' is outside the workspace '{}'",
            resolved.display(),
            workspace.display(),
        );
    }
    Ok(canonical)
}

// ── ReadTool ──────────────────────────────────────────────

pub struct ReadTool {
    ctx: ToolContext,
}

impl ReadTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            ctx: ToolContext { workspace },
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "read".into(),
                description: "Read the contents of a file. Supports optional offset and limit for partial reads.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "The absolute path to the file to read"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Line number to start reading from (1-indexed)",
                            "default": null
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to read",
                            "default": null
                        }
                    },
                    "required": ["file_path"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .context("Missing file_path")?;
        let resolved = if PathBuf::from(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.ctx.workspace.join(path)
        };
        validate_path_in_workspace(&resolved, &self.ctx.workspace)?;
        let content =
            tokio::fs::read_to_string(&resolved)
                .await
                .with_context(|| {
                    format!("Failed to read {}", resolved.display())
                })?;

        let offset =
            args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        if offset == 0 && limit.is_none() {
            return Ok(result_no_changes(content));
        }

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.saturating_sub(1).min(lines.len());
        let end = limit
            .map(|l| start + l)
            .unwrap_or(lines.len())
            .min(lines.len());
        Ok(result_no_changes(lines[start..end].join("\n")))
    }
}

// ── GlobTool ──────────────────────────────────────────────

pub struct GlobTool {
    ctx: ToolContext,
}

impl GlobTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            ctx: ToolContext { workspace },
        }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "glob".into(),
                description: "Search for files matching a glob pattern.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match (e.g. \"src/**/*.rs\")"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .context("Missing pattern")?;
        let pattern_path = if PathBuf::from(pattern).is_absolute() {
            PathBuf::from(pattern)
        } else {
            self.ctx.workspace.join(pattern)
        };
        // Validate the non-glob static prefix to enforce workspace boundaries.
        let static_prefix = static_glob_prefix(&pattern_path);
        validate_path_in_workspace(&static_prefix, &self.ctx.workspace)?;
        let pattern = pattern_path.to_string_lossy().to_string();
        let ws_canon = self
            .ctx
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| self.ctx.workspace.clone());
        // Glob traverses the filesystem — run in spawn_blocking to avoid blocking the async runtime
        let results = tokio::task::spawn_blocking(
            move || -> anyhow::Result<Vec<String>> {
                let entries = glob::glob(&pattern)
                    .context("Invalid glob pattern")?
                    .filter_map(|e| e.ok())
                    .filter_map(|p| p.canonicalize().ok())
                    .filter(|p| p.starts_with(&ws_canon))
                    .map(|p| p.to_string_lossy().to_string())
                    .collect::<Vec<String>>();
                Ok(entries)
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("Glob task panicked: {:#}", e))??;
        if results.is_empty() {
            return Ok(result_no_changes("No matches found.".into()));
        }
        Ok(result_no_changes(results.join("\n")))
    }
}

// ── GrepTool ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrepBackend {
    Ripgrep,
    Grep,
}

pub struct GrepTool {
    ctx: ToolContext,
}

fn command_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn preferred_grep_backend_with<F>(mut is_available: F) -> GrepBackend
where
    F: FnMut(&str) -> bool,
{
    if is_available("rg") {
        GrepBackend::Ripgrep
    } else {
        GrepBackend::Grep
    }
}

fn preferred_grep_backend() -> GrepBackend {
    preferred_grep_backend_with(command_available)
}

impl GrepTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            ctx: ToolContext { workspace },
        }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "grep".into(),
                description: "Search file contents for a regex pattern. Returns matching lines with file paths.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for"
                        },
                        "include": {
                            "type": "string",
                            "description": "File pattern to include (e.g. \"*.rs\")",
                            "default": null
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory to search in",
                            "default": null
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .context("Missing pattern")?;
        let dir = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| {
                if PathBuf::from(p).is_absolute() {
                    PathBuf::from(p)
                } else {
                    self.ctx.workspace.join(p)
                }
            })
            .unwrap_or_else(|| self.ctx.workspace.clone());
        let dir = validate_path_in_workspace(&dir, &self.ctx.workspace)?;

        let include = args.get("include").and_then(|v| v.as_str());

        let output = match preferred_grep_backend() {
            GrepBackend::Ripgrep => {
                // Use ripgrep (preferred)
                let mut cmd = tokio::process::Command::new("rg");
                cmd.arg("--line-number")
                    .arg("--with-filename")
                    .arg("-i")
                    .arg(pattern)
                    .arg(&dir);
                if let Some(inc) = include {
                    cmd.arg("--glob").arg(inc);
                }
                cmd.output().await.context("Failed to run ripgrep")?
            }
            GrepBackend::Grep => {
                // Fallback to grep -rn
                let mut cmd = tokio::process::Command::new("grep");
                cmd.arg("-rn").arg("-i").arg("-E").arg(pattern);
                if let Some(inc) = include {
                    cmd.arg("--include").arg(inc);
                }
                cmd.arg(&dir);
                cmd.output().await.context("Failed to run grep")?
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if stdout.is_empty() {
            return Ok(result_no_changes("No matches found.".into()));
        }
        Ok(result_no_changes(truncate_output(stdout)))
    }
}

fn static_glob_prefix(path: &std::path::Path) -> PathBuf {
    use std::path::{Component, PathBuf};
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => {
                out.push(std::path::MAIN_SEPARATOR.to_string())
            }
            Component::CurDir => out.push("."),
            Component::ParentDir => out.push(".."),
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.contains('*') || s.contains('?') || s.contains('[') {
                    break;
                }
                out.push(seg);
            }
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

// ── WriteTool ─────────────────────────────────────────────

pub struct WriteTool {
    ctx: ToolContext,
}

impl WriteTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            ctx: ToolContext { workspace },
        }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "write".into(),
                description: "Write content to a file. Creates the file if it doesn't exist. Overwrites existing content.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "The path to write to"
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to write"
                        }
                    },
                    "required": ["file_path", "content"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .context("Missing file_path")?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .context("Missing content")?;
        let resolved = if PathBuf::from(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.ctx.workspace.join(path)
        };
        validate_path_in_workspace(&resolved, &self.ctx.workspace)?;
        let existed_before = resolved.exists();
        let before_content = if existed_before {
            Some(tokio::fs::read_to_string(&resolved).await.with_context(
                || format!("Failed to read {}", resolved.display()),
            )?)
        } else {
            None
        };
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&resolved, content)
            .await
            .with_context(|| {
                format!("Failed to write {}", resolved.display())
            })?;
        let mut changes = Vec::new();
        let mut rollback_entries = Vec::new();
        if let Some(before) = before_content {
            if let Some(fc) =
                compute_modified_file_change(&resolved, &before, content)
            {
                changes.push(fc);
            }
            rollback_entries.push(RollbackEntry {
                path: resolved.clone(),
                kind: RollbackKind::Modified,
                before_content: Some(before),
                expected_after_hash: Some(content_hash64(content)),
            });
        } else {
            changes.push(FileChange {
                path: resolved.to_string_lossy().to_string(),
                kind: FileChangeKind::Added,
                diff_lines: Vec::new(),
                truncated: false,
            });
            rollback_entries.push(RollbackEntry {
                path: resolved.clone(),
                kind: RollbackKind::Added,
                before_content: None,
                expected_after_hash: Some(content_hash64(content)),
            });
        }
        Ok(ToolExecutionResult {
            output: format!(
                "Written {} bytes to {}",
                content.len(),
                resolved.display()
            ),
            changes,
            rollback_entries,
        })
    }
}

// ── EditTool ──────────────────────────────────────────────

pub struct EditTool {
    ctx: ToolContext,
}

impl EditTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            ctx: ToolContext { workspace },
        }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "edit".into(),
                description: "Perform a search-and-replace edit on a file. Replaces the first occurrence of old_string with new_string.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "The path to the file to edit"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "The text to search for (must match exactly)"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "The replacement text"
                        }
                    },
                    "required": ["file_path", "old_string", "new_string"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .context("Missing file_path")?;
        let old = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .context("Missing old_string")?;
        let new = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .context("Missing new_string")?;
        let resolved = if PathBuf::from(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.ctx.workspace.join(path)
        };
        validate_path_in_workspace(&resolved, &self.ctx.workspace)?;
        let content =
            tokio::fs::read_to_string(&resolved)
                .await
                .with_context(|| {
                    format!("Failed to read {}", resolved.display())
                })?;
        if !content.contains(old) {
            return Err(anyhow::anyhow!(
                "old_string not found in {}",
                resolved.display()
            ));
        }
        let new_content = content.replacen(old, new, 1);
        tokio::fs::write(&resolved, &new_content)
            .await
            .with_context(|| {
                format!("Failed to write {}", resolved.display())
            })?;
        let mut changes = Vec::new();
        if let Some(fc) =
            compute_modified_file_change(&resolved, &content, &new_content)
        {
            changes.push(fc);
        }
        Ok(ToolExecutionResult {
            output: format!("Edited {}", resolved.display()),
            changes,
            rollback_entries: vec![RollbackEntry {
                path: resolved,
                kind: RollbackKind::Modified,
                before_content: Some(content),
                expected_after_hash: Some(content_hash64(&new_content)),
            }],
        })
    }
}

// ── DeleteTool ────────────────────────────────────────────

pub struct DeleteTool {
    ctx: ToolContext,
}

impl DeleteTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            ctx: ToolContext { workspace },
        }
    }
}

#[async_trait]
impl Tool for DeleteTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "delete".into(),
                description: "Delete an existing file. File-only in v1 (directories are rejected).".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "The file path to delete"
                        }
                    },
                    "required": ["file_path"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .context("Missing file_path")?;
        let resolved = if PathBuf::from(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.ctx.workspace.join(path)
        };
        validate_path_in_workspace(&resolved, &self.ctx.workspace)?;

        let metadata =
            tokio::fs::metadata(&resolved).await.with_context(|| {
                format!("Failed to access {}", resolved.display())
            })?;
        if metadata.is_dir() {
            anyhow::bail!(
                "Refusing to delete directory {} (file-only delete)",
                resolved.display()
            );
        }

        let before_content = tokio::fs::read_to_string(&resolved)
            .await
            .with_context(|| {
                format!("Failed to read {}", resolved.display())
            })?;

        tokio::fs::remove_file(&resolved).await.with_context(|| {
            format!("Failed to delete {}", resolved.display())
        })?;

        Ok(ToolExecutionResult {
            output: format!("Deleted {}", resolved.display()),
            changes: vec![FileChange {
                path: resolved.to_string_lossy().to_string(),
                kind: FileChangeKind::Removed,
                diff_lines: Vec::new(),
                truncated: false,
            }],
            rollback_entries: vec![RollbackEntry {
                path: resolved,
                kind: RollbackKind::Removed,
                before_content: Some(before_content),
                expected_after_hash: None,
            }],
        })
    }
}

// ── BashTool ──────────────────────────────────────────────

pub struct BashTool {
    ctx: ToolContext,
}

impl BashTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            ctx: ToolContext { workspace },
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "bash".into(),
                description: "Execute a shell command. Use with caution."
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute"
                        },
                        "description": {
                            "type": "string",
                            "description": "A brief description of what the command does",
                            "default": null
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Timeout in seconds (default: 120)",
                            "default": 120
                        }
                    },
                    "required": ["command"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let cmd = args
            .get("command")
            .and_then(|v| v.as_str())
            .context("Missing command")?;
        let timeout_secs =
            args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(120);

        // Use sh -c for portable shell execution, with a timeout guard.
        // `kill_on_drop(true)` ensures a timed-out command does not keep
        // running in the background after the future is dropped.
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.ctx.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Failed to execute command")?;

        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await
        {
            Ok(result) => result.context("Failed to execute command")?,
            Err(_) => {
                return Ok(result_no_changes(format!(
                    "[command timed out after {}s]",
                    timeout_secs
                )));
            }
        };

        let mut result = String::new();
        if !output.stdout.is_empty() {
            result.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if !output.status.success() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&format!(
                "[exit code: {}]",
                output.status.code().unwrap_or(-1)
            ));
        }
        if result.is_empty() {
            result = "(no output)".into();
        }
        Ok(result_no_changes(truncate_output(result)))
    }
}

// ── UseSkillTool — load full skill content on demand ────

/// Tool that loads the full content of a skill when the LLM requests it.
pub struct UseSkillTool;

#[async_trait]
impl Tool for UseSkillTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "use_skill".into(),
                description: "Load the full content of a named skill. Call this when a skill listed in 'Skills available' is relevant to the current task.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "skill_name": {
                            "type": "string",
                            "description": "Name of the skill to load"
                        }
                    },
                    "required": ["skill_name"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let skill_name = args
            .get("skill_name")
            .and_then(|v| v.as_str())
            .context("Missing 'skill_name'")?;

        // Validate skill name to prevent path traversal
        if !skill_name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!(
                "Invalid skill name '{}': only letters, numbers, hyphens, and underscores allowed",
                skill_name
            );
        }

        let home = dirs::home_dir().context("Cannot find home directory")?;
        let skill_path = home
            .join(".config")
            .join("mote")
            .join("skills")
            .join(skill_name)
            .join("SKILL.md");

        if !skill_path.exists() {
            anyhow::bail!(
                "Skill '{}' not found at {}",
                skill_name,
                skill_path.display()
            );
        }

        let content =
            fs::read_to_string(&skill_path).await.with_context(|| {
                format!("Failed to read skill file: {}", skill_path.display())
            })?;

        // Strip YAML frontmatter if present, return only the body
        let body = if let Some(rest) = content
            .strip_prefix("---\n")
            .or_else(|| content.strip_prefix("---\r\n"))
        {
            if let Some((_yaml, rest_body)) = rest
                .split_once("\n---")
                .or_else(|| rest.split_once("\r\n---"))
            {
                rest_body.trim_start_matches('\n').trim().to_string()
            } else {
                content.trim().to_string()
            }
        } else {
            content.trim().to_string()
        };

        if body.is_empty() {
            Ok(result_no_changes(format!(
                "[Skill '{}' has no content]",
                skill_name
            )))
        } else {
            Ok(result_no_changes(body))
        }
    }
}

/// Internal completion marker. The agent loop handles this tool specially and
/// does not execute it as a normal external tool.
pub struct FinishTaskTool;

#[async_trait]
impl Tool for FinishTaskTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "finish_task".into(),
                description: "Call exactly once when the user's request is fully complete. Provide the final answer to show the user.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "final_answer": {
                            "type": "string",
                            "description": "Final response to the user, including concise summary and any caveats."
                        }
                    },
                    "required": ["final_answer"]
                }),
            },
        }
    }

    async fn execute(&self, _args: Value) -> Result<ToolExecutionResult> {
        Ok(result_no_changes("[task finished]".into()))
    }
}

// ── SubagentTool — delegate to another agent ────────────

/// Tool that delegates a task to a sub-agent by name.
pub struct SubagentTool {
    ctx: ToolContext,
    /// How to look up a subagent, run it, and get results.
    runner: Box<dyn SubagentRunner + Send + Sync>,
}

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn run(
        &self,
        agent_name: &str,
        task: &str,
        workspace: &PathBuf,
    ) -> Result<String>;
}

impl SubagentTool {
    pub fn new(
        ctx: ToolContext,
        runner: Box<dyn SubagentRunner + Send + Sync>,
    ) -> Self {
        Self { ctx, runner }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "subagent".into(),
                description: "Delegate a task to a named sub-agent. Available agents are listed in the system prompt.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent": {
                            "type": "string",
                            "description": "Name of the agent to delegate to"
                        },
                        "task": {
                            "type": "string",
                            "description": "The task or question for the sub-agent"
                        }
                    },
                    "required": ["agent", "task"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let agent_name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .context("Missing 'agent'")?;
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .context("Missing 'task'")?;
        // Strip leading '@' if present (user may have typed @name in the TUI)
        let agent_name = agent_name.strip_prefix('@').unwrap_or(agent_name);
        let output = self
            .runner
            .run(agent_name, task, &self.ctx.workspace)
            .await?;
        Ok(result_no_changes(output))
    }
}

// ── Concrete runner using the agent loop ────────────────

use std::sync::Arc;

pub struct AgentSubagentRunner {
    pub provider: Arc<dyn crate::llm::LlmProvider>,
    pub tools: Arc<Vec<Box<dyn crate::llm::Tool>>>,
    pub config: crate::config::Config,
    pub merged_agents:
        std::collections::HashMap<String, crate::config::AgentConfig>,
    pub repo_agents_md: Option<String>,
    /// Parent cancellation channel — subagent is cancelled when parent is.
    pub cancel_rx: tokio::sync::watch::Receiver<bool>,
    /// Current call depth (0 = top-level, 1 = subagent, 2 = sub-subagent, etc.)
    pub depth: u8,
    /// Max allowed recursion depth
    pub max_depth: u8,
    /// Channel to forward subagent events to the parent's TUI.
    pub parent_events_tx: tokio::sync::mpsc::UnboundedSender<
        anyhow::Result<crate::agent::AgentEvent>,
    >,
}

#[async_trait]
impl SubagentRunner for AgentSubagentRunner {
    async fn run(
        &self,
        agent_name: &str,
        task: &str,
        workspace: &PathBuf,
    ) -> Result<String> {
        if self.depth >= self.max_depth {
            anyhow::bail!(
                "Sub-agent recursion limit reached (max {} levels)",
                self.max_depth
            );
        }
        let agent_cfg = self.merged_agents.get(agent_name);
        if let Some(agent) = agent_cfg {
            if !agent.is_subagent_callable() {
                anyhow::bail!(
                    "Agent '{}' is not available as a subagent",
                    agent_name
                );
            }
        } else {
            anyhow::bail!("Unknown sub-agent: '{}'", agent_name);
        }
        let agent_model = agent_cfg.and_then(|a| a.model.as_deref());

        let eff_provider_name = if let Some(model_str) = agent_model {
            if let Some((provider, _)) = model_str.split_once('/') {
                provider.to_string()
            } else {
                self.config.effective_provider(agent_model)
            }
        } else {
            self.config.effective_provider(agent_model)
        };

        // Use the same provider for subagents (they share the LLM backend)
        let provider = Arc::clone(&self.provider);

        // Build system prompt for the subagent (blocking filesystem I/O)
        let eff_model_id = self.config.effective_model_id(agent_model);
        let prompt =
            crate::prompt::PromptAssembler::for_agent(&self.config, agent_cfg)
                .with_workspace_context(
                    Some(workspace.clone()),
                    self.repo_agents_md.clone(),
                );
        let provider_for_prompt = eff_provider_name.clone();
        let model_for_prompt = eff_model_id.clone();
        let system_layers = tokio::task::spawn_blocking(move || {
            prompt.assemble(&provider_for_prompt, &model_for_prompt)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Prompt assembly panicked: {:#}", e))??;

        let eff_temperature = self
            .config
            .effective_temperature(agent_cfg.and_then(|a| a.temperature));
        let eff_max_tokens = self.config.effective_max_tokens(
            agent_cfg.and_then(|a| a.max_tokens),
            &eff_provider_name,
        );

        // Build permission map using the shared helper.
        // Subagent remaps "ask" → "allow" (no TUI for subagent permission prompts).
        let tool_names: Vec<String> = self
            .tools
            .iter()
            .map(|t| t.def().function.name.clone())
            .collect();
        let mut perms =
            crate::build_permission_map(&self.config, agent_cfg, &tool_names);
        // Remap Ask → Allow for subagents (no interactive TUI)
        for perm in perms.values_mut() {
            if *perm == crate::config::Permission::Ask {
                *perm = crate::config::Permission::Allow;
            }
        }

        let opts = crate::llm::ChatOptions {
            model_id: eff_model_id,
            temperature: eff_temperature,
            max_tokens: eff_max_tokens,
            // Only advertise tools that are allowed — denied tools are invisible to the subagent LLM
            tools: self
                .tools
                .iter()
                .filter(|t| {
                    perms.get(&t.def().function.name).copied()
                        != Some(crate::config::Permission::Deny)
                })
                .map(|t| t.def())
                .collect(),
        };

        // Create channels for the subagent
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::unbounded_channel();
        let (sub_cancel_tx, sub_cancel_rx) = tokio::sync::watch::channel(false);
        let timeout_cancel_tx = sub_cancel_tx.clone();
        // Link subagent cancellation to parent: when parent cancels, subagent cancels too
        let mut parent_cancel = self.cancel_rx.clone();
        tokio::spawn(async move {
            let _ = parent_cancel.changed().await;
            let _ = sub_cancel_tx.send(true);
        });
        // _perm_tx is intentionally dropped immediately: subagent permissions are
        // remapped to "allow" or "deny" only (never "ask"), so the permission
        // channel is never used. If this changes, store _perm_tx and wire it to a
        // permission forwarding mechanism.
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();

        let user_msg = task.to_string();
        let history: Vec<crate::llm::ChatMessage> = Vec::new();
        let t2 = Arc::clone(&self.tools);
        let p2 = Arc::clone(&provider);
        let workspace_display = workspace.display().to_string();

        tokio::spawn(async move {
            crate::agent::run_loop(
                p2,
                t2,
                system_layers,
                user_msg,
                history,
                opts,
                agent_tx,
                sub_cancel_rx,
                perm_rx,
                perms,
                crate::agent::DEFAULT_MAX_STEPS,
                workspace_display,
            )
            .await;
        });

        // Generate a unique subagent session ID
        let sub_id =
            format!("sub_{}", chrono::Local::now().format("%Y%m%d%H%M%S%6f"));
        let sub_name = agent_name.to_string();

        // Signal that a subagent started
        let _ = self.parent_events_tx.send(Ok(
            crate::agent::AgentEvent::SubagentStarted {
                id: sub_id.clone(),
                name: sub_name.clone(),
            },
        ));

        // Collect the result while forwarding events to the parent.
        // Subagent has a 5-minute timeout to prevent blocking the parent indefinitely.
        let mut content = String::new();
        let mut tool_log = String::new();
        let subagent_timeout = std::time::Duration::from_secs(300);

        let collect_result = tokio::time::timeout(subagent_timeout, async {
            while let Some(event) = agent_rx.recv().await {
                match event {
                    Ok(crate::agent::AgentEvent::Done {
                        content: c, ..
                    })
                    | Ok(crate::agent::AgentEvent::Cancelled {
                        content: c,
                        ..
                    })
                    | Ok(crate::agent::AgentEvent::NeedsContinuation {
                        content: c,
                        ..
                    }) => {
                        content = c;
                        break;
                    }
                    Ok(crate::agent::AgentEvent::TextDelta(text)) => {
                        content.push_str(&text);
                        let _ = self.parent_events_tx.send(Ok(
                            crate::agent::AgentEvent::SubagentTextDelta {
                                id: sub_id.clone(),
                                data: text,
                            },
                        ));
                    }
                    Ok(crate::agent::AgentEvent::ReasoningDelta(text)) => {
                        let _ = self.parent_events_tx.send(Ok(
                            crate::agent::AgentEvent::SubagentReasoningDelta {
                                id: sub_id.clone(),
                                data: text,
                            },
                        ));
                    }
                    Ok(crate::agent::AgentEvent::ToolStarted {
                        id: tool_call_id,
                        name,
                    }) => {
                        tool_log.push_str(&format!("\n  [Tool: {}]", name));
                        let _ = self.parent_events_tx.send(Ok(
                            crate::agent::AgentEvent::SubagentToolStarted {
                                id: sub_id.clone(),
                                sub_id: tool_call_id,
                                tool_name: name,
                            },
                        ));
                    }
                    Ok(crate::agent::AgentEvent::ToolCompleted {
                        id: tool_call_id,
                        result,
                        changes,
                        ..
                    }) => {
                        let summary = if result.len() > 100 {
                            format!(
                                "{}...",
                                crate::agent::safe_truncate(&result, 97)
                            )
                        } else {
                            result.clone()
                        };
                        tool_log.push_str(&format!(" → {}", summary));
                        let _ = self.parent_events_tx.send(Ok(
                            crate::agent::AgentEvent::SubagentToolCompleted {
                                id: sub_id.clone(),
                                sub_id: tool_call_id,
                                result,
                                changes,
                            },
                        ));
                    }
                    Ok(crate::agent::AgentEvent::ToolFailed {
                        id: tool_call_id,
                        error,
                    }) => {
                        tool_log.push_str(&format!(" → FAILED: {}", error));
                        let _ = self.parent_events_tx.send(Ok(
                            crate::agent::AgentEvent::SubagentToolFailed {
                                id: sub_id.clone(),
                                sub_id: tool_call_id,
                                error,
                            },
                        ));
                    }
                    Err(e) => {
                        content = format!("[Sub-agent error: {:#}]", e);
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;

        // Handle timeout — cancel the subagent if it didn't finish in time
        if collect_result.is_err() {
            tracing::warn!(
                "Sub-agent '{}' timed out after {}s",
                agent_name,
                subagent_timeout.as_secs()
            );
            let _ = timeout_cancel_tx.send(true);
            anyhow::bail!(
                "Sub-agent '{}' timed out after {}s",
                agent_name,
                subagent_timeout.as_secs()
            );
        }

        let mut result = String::new();
        if !tool_log.is_empty() {
            result.push_str(&format!("[Tools used:{}]\n", tool_log));
        }
        if !content.is_empty() {
            result.push_str(&content);
        }
        if result.is_empty() {
            result = format!("[Sub-agent '{}' completed]", agent_name);
        }

        // Signal that the subagent finished
        let _ = self.parent_events_tx.send(Ok(
            crate::agent::AgentEvent::SubagentDone {
                id: sub_id,
                content: content.clone(),
            },
        ));

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio;

    fn tmp_workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[tokio::test]
    async fn test_read_file() {
        let (_d, ws) = tmp_workspace();
        let test_file = ws.join("test.txt");
        std::fs::write(&test_file, "hello\nworld\n").unwrap();

        let tool = ReadTool::new(ws);
        let args = serde_json::json!({"file_path": "test.txt"});
        let result = tool.execute(args).await.unwrap();
        assert_eq!(result.output, "hello\nworld\n");
    }

    #[tokio::test]
    async fn test_read_with_offset() {
        let (_d, ws) = tmp_workspace();
        let test_file = ws.join("lines.txt");
        std::fs::write(&test_file, "a\nb\nc\nd\ne\n").unwrap();

        let tool = ReadTool::new(ws);
        let args = serde_json::json!({"file_path": "lines.txt", "offset": 2, "limit": 3});
        let result = tool.execute(args).await.unwrap();
        assert_eq!(result.output, "b\nc\nd");
    }

    #[tokio::test]
    async fn test_read_missing_file() {
        let (_d, ws) = tmp_workspace();
        let tool = ReadTool::new(ws);
        let args = serde_json::json!({"file_path": "nonexistent.txt"});
        assert!(tool.execute(args).await.is_err());
    }

    #[tokio::test]
    async fn test_write_file() {
        let (_d, ws) = tmp_workspace();
        let ws_path = ws.clone();
        let tool = WriteTool::new(ws);
        let args = serde_json::json!({"file_path": "new.txt", "content": "test content"});
        let result = tool.execute(args).await.unwrap();
        assert!(result.output.contains("Written"));
        assert_eq!(result.changes.len(), 1);
        assert!(matches!(result.changes[0].kind, FileChangeKind::Added));

        let content = std::fs::read_to_string(ws_path.join("new.txt")).unwrap();
        assert_eq!(content, "test content");
    }

    #[tokio::test]
    async fn test_write_creates_dirs() {
        let (_d, ws) = tmp_workspace();
        let ws_path = ws.clone();
        let tool = WriteTool::new(ws);
        let args = serde_json::json!({"file_path": "sub/dir/file.txt", "content": "nested"});
        tool.execute(args).await.unwrap();
        assert!(ws_path.join("sub/dir/file.txt").exists());
    }

    #[tokio::test]
    async fn test_write_rejects_outside_workspace_abs_missing_parent() {
        let (_d, ws) = tmp_workspace();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_target = outside_dir.path().join("x/y/new.txt");
        let tool = WriteTool::new(ws);
        let args = serde_json::json!({
            "file_path": outside_target.to_string_lossy().to_string(),
            "content": "blocked"
        });
        let result = tool.execute(args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_edit_file() {
        let (_d, ws) = tmp_workspace();
        let ws_path = ws.clone();
        let f = ws.join("edit.txt");
        std::fs::write(&f, "hello world\nfoo bar\n").unwrap();

        let tool = EditTool::new(ws);
        let args = serde_json::json!({"file_path": "edit.txt", "old_string": "foo bar", "new_string": "baz qux"});
        tool.execute(args).await.unwrap();

        let content = std::fs::read_to_string(&f).unwrap();
        assert_eq!(content, "hello world\nbaz qux\n");
        let result = EditTool::new(ws_path).execute(serde_json::json!({"file_path": "edit.txt", "old_string": "baz qux", "new_string": "z"})).await.unwrap();
        assert_eq!(result.changes.len(), 1);
        assert!(matches!(result.changes[0].kind, FileChangeKind::Modified));
    }

    #[tokio::test]
    async fn test_edit_diff_focuses_changed_hunk_after_long_context() {
        let (_d, ws) = tmp_workspace();
        let f = ws.join("long.txt");
        let mut content = String::new();
        for i in 0..80 {
            content.push_str(&format!("context {i}\n"));
        }
        content.push_str("target line\n");
        std::fs::write(&f, content).unwrap();

        let tool = EditTool::new(ws);
        let result = tool
            .execute(serde_json::json!({
                "file_path": "long.txt",
                "old_string": "target line",
                "new_string": "replacement line"
            }))
            .await
            .unwrap();

        assert_eq!(result.changes.len(), 1);
        let diff_lines = &result.changes[0].diff_lines;
        assert!(diff_lines.iter().any(|line| {
            matches!(line.kind, DiffLineKind::Removed)
                && line.content == "target line"
        }));
        assert!(diff_lines.iter().any(|line| {
            matches!(line.kind, DiffLineKind::Added)
                && line.content == "replacement line"
        }));
    }

    #[tokio::test]
    async fn test_edit_not_found() {
        let (_d, ws) = tmp_workspace();
        let f = ws.join("edit.txt");
        std::fs::write(&f, "hello").unwrap();

        let tool = EditTool::new(ws);
        let args = serde_json::json!({"file_path": "edit.txt", "old_string": "nonexistent", "new_string": "x"});
        assert!(tool.execute(args).await.is_err());
    }

    #[tokio::test]
    async fn test_glob_matches() {
        let (_d, ws) = tmp_workspace();
        std::fs::write(ws.join("a.rs"), "").unwrap();
        std::fs::write(ws.join("b.rs"), "").unwrap();
        std::fs::write(ws.join("c.txt"), "").unwrap();

        let tool = GlobTool::new(ws);
        let args = serde_json::json!({"pattern": "*.rs"});
        let result = tool.execute(args).await.unwrap();
        assert!(result.output.contains("a.rs"));
        assert!(result.output.contains("b.rs"));
        assert!(!result.output.contains("c.txt"));
    }

    #[tokio::test]
    async fn test_glob_no_matches() {
        let (_d, ws) = tmp_workspace();
        let tool = GlobTool::new(ws);
        let args = serde_json::json!({"pattern": "*.zzz"});
        let result = tool.execute(args).await.unwrap();
        assert_eq!(result.output, "No matches found.");
    }

    #[tokio::test]
    async fn test_glob_rejects_outside_workspace() {
        let (_d, ws) = tmp_workspace();
        let outside = tempfile::tempdir().unwrap();
        let pattern = outside.path().join("*.rs").to_string_lossy().to_string();
        let tool = GlobTool::new(ws);
        let result =
            tool.execute(serde_json::json!({"pattern": pattern})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_grep_matches() {
        let (_d, ws) = tmp_workspace();
        std::fs::write(
            ws.join("search.txt"),
            "hello world\nfoo bar\nhello again\n",
        )
        .unwrap();

        let tool = GrepTool::new(ws);
        let args = serde_json::json!({"pattern": "hello"});
        let result = tool.execute(args).await.unwrap();
        assert!(result.output.contains("hello world"));
        assert!(result.output.contains("hello again"));
    }

    #[tokio::test]
    async fn test_grep_rejects_outside_workspace_path() {
        let (_d, ws) = tmp_workspace();
        let outside = tempfile::tempdir().unwrap();
        let tool = GrepTool::new(ws);
        let args = serde_json::json!({
            "pattern": "hello",
            "path": outside.path().to_string_lossy().to_string()
        });
        let result = tool.execute(args).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_preferred_grep_backend_prefers_rg_when_available() {
        let backend = preferred_grep_backend_with(|name| name == "rg");
        assert_eq!(backend, GrepBackend::Ripgrep);
    }

    #[test]
    fn test_preferred_grep_backend_falls_back_to_grep_when_rg_missing() {
        let backend = preferred_grep_backend_with(|_| false);
        assert_eq!(backend, GrepBackend::Grep);
    }

    #[tokio::test]
    async fn test_bash_echo() {
        let (_d, ws) = tmp_workspace();
        let tool = BashTool::new(ws);
        let args = serde_json::json!({"command": "echo hello"});
        let result = tool.execute(args).await.unwrap();
        assert_eq!(result.output.trim(), "hello");
    }

    #[tokio::test]
    async fn test_bash_exit_code() {
        let (_d, ws) = tmp_workspace();
        let tool = BashTool::new(ws);
        let args = serde_json::json!({"command": "exit 42"});
        let result = tool.execute(args).await.unwrap();
        assert!(result.output.contains("exit code: 42"));
    }

    #[tokio::test]
    async fn test_delete_file() {
        let (_d, ws) = tmp_workspace();
        let f = ws.join("dead.txt");
        std::fs::write(&f, "bye").unwrap();
        let tool = DeleteTool::new(ws.clone());
        let result = tool
            .execute(serde_json::json!({"file_path": "dead.txt"}))
            .await
            .unwrap();
        assert!(!f.exists());
        assert_eq!(result.changes.len(), 1);
        assert!(matches!(result.changes[0].kind, FileChangeKind::Removed));
    }

    #[tokio::test]
    async fn test_delete_directory_rejected() {
        let (_d, ws) = tmp_workspace();
        let dir = ws.join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        let tool = DeleteTool::new(ws.clone());
        let result =
            tool.execute(serde_json::json!({"file_path": "dir"})).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_tool_def_read() {
        let (_d, ws) = tmp_workspace();
        let tool = ReadTool::new(ws);
        let def = tool.def();
        assert_eq!(def.function.name, "read");
        assert!(def.function.description.contains("file"));
        assert!(def.function.parameters["required"].is_array());
    }

    #[test]
    fn test_tool_def_write() {
        let (_d, ws) = tmp_workspace();
        let tool = WriteTool::new(ws);
        let def = tool.def();
        assert_eq!(def.function.name, "write");
        assert!(
            def.function.parameters["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("content"))
        );
    }

    #[tokio::test]
    async fn test_subagent_tool_def() {
        let (_d, ws) = tmp_workspace();
        let runner = MockSubagentRunner;
        let tool =
            SubagentTool::new(ToolContext { workspace: ws }, Box::new(runner));
        let def = tool.def();
        assert_eq!(def.function.name, "subagent");
        assert!(def.function.description.contains("agent"));
        let args = def.function.parameters;
        assert!(
            args["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("agent"))
        );
        assert!(
            args["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("task"))
        );
    }

    #[tokio::test]
    async fn test_subagent_tool_executes_runner() {
        let (_d, ws) = tmp_workspace();
        let runner = MockSubagentRunner;
        let tool =
            SubagentTool::new(ToolContext { workspace: ws }, Box::new(runner));
        let result = tool
            .execute(serde_json::json!({"agent": "review", "task": "check"}))
            .await
            .unwrap();
        assert_eq!(result.output, "mock result for agent=review, task=check");
    }

    // ── UseSkillTool tests ────────────────────────────────

    #[tokio::test]
    async fn test_use_skill_tool_def() {
        let tool = UseSkillTool;
        let def = tool.def();
        assert_eq!(def.function.name, "use_skill");
        assert!(
            def.function.parameters["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("skill_name"))
        );
    }

    #[tokio::test]
    async fn test_use_skill_rejects_bad_name() {
        let tool = UseSkillTool;
        let result = tool
            .execute(serde_json::json!({"skill_name": "../../etc/passwd"}))
            .await;
        assert!(result.is_err(), "Path traversal should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Invalid skill name"),
            "Error should mention invalid name: {err}"
        );
    }

    #[tokio::test]
    async fn test_use_skill_returns_body_without_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: test-skill
description: A test skill
---
Actual skill content here."#,
        )
        .unwrap();

        // Override home dir for the tool
        // We can't easily mock home_dir(), so test the file reading directly
        let skill_path = skill_dir.join("SKILL.md");
        let content = tokio::fs::read_to_string(&skill_path).await.unwrap();

        // Test frontmatter stripping logic (same as UseSkillTool)
        let body = if let Some(rest) = content.strip_prefix("---\n") {
            if let Some((_yaml, rest_body)) = rest.split_once("\n---") {
                rest_body.trim_start_matches('\n').trim().to_string()
            } else {
                content.trim().to_string()
            }
        } else {
            content.trim().to_string()
        };
        assert_eq!(body, "Actual skill content here.");
    }

    #[tokio::test]
    async fn test_use_skill_not_found_error() {
        // Test that missing skill returns a clear error
        let tool = UseSkillTool;
        // This skill should not exist
        let result = tool
            .execute(serde_json::json!({"skill_name": "nonexistent-skill-xyz"}))
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_truncate_output_short() {
        let short = "hello world".to_string();
        assert_eq!(truncate_output(short.clone()), short);
    }

    #[test]
    fn test_truncate_output_long() {
        let long = "x".repeat(MAX_OUTPUT_BYTES + 1000);
        let result = truncate_output(long.clone());
        assert!(result.len() < long.len());
        assert!(result.contains("[output truncated"));
        assert!(result.contains(&format!("{} bytes total", long.len())));
    }

    #[test]
    fn test_truncate_output_exact_boundary() {
        let exact = "y".repeat(MAX_OUTPUT_BYTES);
        assert_eq!(truncate_output(exact.clone()), exact);
    }
}

// Mock runner for SubagentTool tests
#[cfg(test)]
struct MockSubagentRunner;

#[cfg(test)]
#[async_trait]
impl SubagentRunner for MockSubagentRunner {
    async fn run(
        &self,
        agent_name: &str,
        task: &str,
        _workspace: &PathBuf,
    ) -> Result<String> {
        Ok(format!(
            "mock result for agent={}, task={}",
            agent_name, task
        ))
    }
}
