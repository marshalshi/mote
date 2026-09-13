//! Google Sheets import tools for the PM agent (slice 3).
//!
//! Manual, operator-triggered import workflow:
//!
//! 1. `pm_preview_google_sheet_site_jobs_import` — validates the `[google_sheets]`
//!    config, fetches the configured tab through the [`SheetFetcher`]
//!    abstraction, normalizes/filters eligible rows, classifies the diff
//!    against the PM store, and persists a **dry-run** import run. It never
//!    mutates imported jobs. Returns an `import_run_id` + `preview_hash`.
//! 2. `pm_apply_google_sheet_site_jobs_import` — requires the exact
//!    `import_run_id` + `preview_hash` from a preview. It verifies the run
//!    exists and is a preview, then **refetches and recomputes the current
//!    preview hash**. If the sheet changed since the preview, it refuses to
//!    apply; only the reviewed state is ever written.
//! 3. `pm_list_google_sheet_import_conflicts` / `pm_list_imported_google_sheet_jobs`
//!    — operator visibility into the database after syncing.
//!
//! # Testability
//!
//! All tools go through [`GoogleSheetsToolCtx::fetch_snapshot`], which uses the
//! injected [`SheetFetcher`]. Tests queue canned [`ValuesResponse`]s in a mock
//! fetcher — no live Google HTTP, and never the user's credentials.
//!
//! # Secrets
//!
//! Credentials are loaded from the configured path but never rendered into
//! tool output or errors (only the path may appear in validation errors).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::config::GoogleSheetsConfig;
use crate::llm::{Tool, ToolDef, ToolExecutionResult, ToolFunctionDef};
use crate::pm::GOOGLE_SHEETS_IMPORT_SOURCE;
use crate::pm::google_sheets::{
    HttpSheetFetcher, NormalizedRow, RowConflict, ServiceAccount, SheetFetcher,
    normalize_response,
};
use crate::pm::imports::{ImportSnapshot, preview_hash};
use crate::pm::service::PmService;

use marshaling_protocol::pm::{
    ImportConflictRecord, ImportConflictStatus, ImportedJobStatus,
};

// ── Shared context ──────────────────────────────────────────

/// Shared state for the Google Sheets import tools.
#[derive(Clone)]
pub struct GoogleSheetsToolCtx {
    service: Arc<PmService>,
    config: GoogleSheetsConfig,
    fetcher: Arc<dyn SheetFetcher>,
}

impl GoogleSheetsToolCtx {
    /// Build a context using the real HTTP fetcher.
    pub fn new(service: Arc<PmService>, config: GoogleSheetsConfig) -> Self {
        Self {
            service,
            config,
            fetcher: Arc::new(HttpSheetFetcher::new()),
        }
    }

    /// Build a context with an injected fetcher (tests only).
    #[cfg(test)]
    pub fn with_fetcher(
        service: Arc<PmService>,
        config: GoogleSheetsConfig,
        fetcher: Arc<dyn SheetFetcher>,
    ) -> Self {
        Self {
            service,
            config,
            fetcher,
        }
    }

    /// Validate the `[google_sheets]` config and resolve the spreadsheet id
    /// plus the credentials path. Never returns credential contents.
    fn resolve(&self) -> Result<(String, PathBuf)> {
        let cfg = &self.config;
        if !cfg.enabled {
            anyhow::bail!(
                "Google Sheets import is not enabled; set [google_sheets].enabled = true in config.toml"
            );
        }
        let spreadsheet_id = cfg
            .spreadsheet_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .with_context(|| {
                "Google Sheets import is enabled but [google_sheets].spreadsheet_id is not set"
            })?
            .to_string();
        let credentials_path = cfg
            .credentials_path
            .as_deref()
            .with_context(|| {
                "Google Sheets import is enabled but [google_sheets].credentials_path is not set"
            })?
            .to_path_buf();
        if !credentials_path.exists() {
            anyhow::bail!(
                "Google Sheets credentials file not found at {}",
                credentials_path.display()
            );
        }
        Ok((spreadsheet_id, credentials_path))
    }

    /// Fetch the configured tab and build an import snapshot (read-only).
    async fn fetch_snapshot(&self) -> Result<ImportSnapshot> {
        let (spreadsheet_id, credentials_path) = self.resolve()?;
        let account = ServiceAccount::from_json_file(&credentials_path)?;
        let response = self
            .fetcher
            .fetch_sheet(&account, &spreadsheet_id, &self.config.sheet_name)
            .await?;
        let normalized = normalize_response(
            &response,
            &self.config.eligible_activity_types,
        )?;
        Ok(ImportSnapshot {
            source: GOOGLE_SHEETS_IMPORT_SOURCE.to_string(),
            spreadsheet_id,
            sheet_name: self.config.sheet_name.clone(),
            rows: normalized.rows,
            conflicts: normalized.conflicts,
            invalid: normalized.invalid,
            done_titles: normalized.done_titles,
            skipped: normalized.skipped,
        })
    }
}

// ── Builders ────────────────────────────────────────────────

/// Build the Google Sheets import tool set for the PM agent.
pub fn google_sheets_tools(
    service: Arc<PmService>,
    config: GoogleSheetsConfig,
) -> Vec<Box<dyn Tool>> {
    let ctx = GoogleSheetsToolCtx::new(service, config);
    vec![
        Box::new(PmPreviewGoogleSheetSiteJobsImportTool::new(ctx.clone())),
        Box::new(PmApplyGoogleSheetSiteJobsImportTool::new(ctx.clone())),
        Box::new(PmListGoogleSheetImportConflictsTool::new(ctx.clone())),
        Box::new(PmListImportedGoogleSheetJobsTool::new(ctx)),
    ]
}

// ── Output helpers ──────────────────────────────────────────

/// Format a tool result with a readable summary plus a structured JSON block.
fn json_result(
    output: &str,
    details: &impl serde::Serialize,
) -> ToolExecutionResult {
    let detail_json =
        serde_json::to_string(details).unwrap_or_else(|_| "{}".to_string());
    ToolExecutionResult {
        output: format!("{}\n\n```json\n{}\n```", output, detail_json),
        changes: Vec::new(),
        rollback_entries: Vec::new(),
    }
}

fn required_string(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .with_context(|| format!("missing required argument: '{key}'"))
}

/// Render a serde enum as its JSON string (lowercase/snake_case per the
/// protocol serde rename rules) for human-readable output.
fn serde_str(v: &impl serde::Serialize) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|val| val.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "?".to_string())
}

fn row_json(row: &NormalizedRow) -> Value {
    json!({
        "title": row.title,
        "location": row.location,
        "activity_type": row.activity_type,
        "start_date": row.start_date,
        "end_date": row.end_date,
        "job_leader": row.job_leader,
        "team_member": row.team_member,
        "robots": row.robots,
        "additional_info": row.additional_info,
    })
}

fn conflict_json(conflict: &RowConflict) -> Value {
    match conflict {
        RowConflict::BlankTitle { row_number } => json!({
            "type": "blank_title",
            "row_number": row_number,
            "reason": "eligible row has a blank title key; not imported",
        }),
        RowConflict::DuplicateTitle { title, row_numbers } => json!({
            "type": "duplicate_title",
            "title": title,
            "row_numbers": row_numbers,
            "reason": "multiple eligible rows share the same title; none imported",
        }),
    }
}

fn conflict_line(conflict: &RowConflict) -> String {
    match conflict {
        RowConflict::BlankTitle { row_number } => {
            format!(
                "  [blank_title] row {row_number}: eligible row has a blank title key; not imported"
            )
        }
        RowConflict::DuplicateTitle { title, row_numbers } => format!(
            "  [duplicate_title] '{title}' rows {row_numbers:?}: multiple eligible rows share the same title; none imported"
        ),
    }
}

/// Readable one-line preview of a create/update row. Includes the
/// `Additional Info` cell so an additional_info-only change is reviewable.
fn preview_row_line(marker: &str, row: &NormalizedRow) -> String {
    let mut line = format!(
        "  {marker} {} | {} | {} -> {}",
        row.title,
        row.location,
        row.start_date.clone().unwrap_or_default(),
        row.end_date.clone().unwrap_or_default()
    );
    if !row.additional_info.is_empty() {
        line.push_str(&format!(" | info: {}", row.additional_info));
    }
    line
}

// ── pm_preview_google_sheet_site_jobs_import ───────────────

pub struct PmPreviewGoogleSheetSiteJobsImportTool {
    ctx: GoogleSheetsToolCtx,
}

impl PmPreviewGoogleSheetSiteJobsImportTool {
    pub fn new(ctx: GoogleSheetsToolCtx) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for PmPreviewGoogleSheetSiteJobsImportTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_preview_google_sheet_site_jobs_import".into(),
                description: "Preview a manual Google Sheets import of site jobs. Validates config, fetches the configured tab, normalizes/filters eligible rows, classifies create/update/unchanged/conflicts against the PM database, and persists a dry-run import run. Returns counts, conflicts, an import_run_id and preview_hash. Never mutates imported jobs.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "include_unchanged": {
                            "type": "boolean",
                            "description": "Include unchanged job keys in the returned listing. Default: false (shown as a count only)."
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let snapshot = self.ctx.fetch_snapshot().await?;
        let diff = self.ctx.service.preview_import(&snapshot)?;
        let run = self.ctx.service.persist_preview(&snapshot, &diff)?;

        let include_unchanged = args
            .get("include_unchanged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let creates: Vec<Value> = diff.creates.iter().map(row_json).collect();
        let updates: Vec<Value> = diff.updates.iter().map(row_json).collect();
        let conflicts: Vec<Value> =
            diff.conflicts.iter().map(conflict_json).collect();
        let invalid: Vec<Value> = diff
            .invalid
            .iter()
            .map(|i| json!({ "row_number": i.row_number, "reason": i.reason }))
            .collect();

        let mut lines = Vec::new();
        lines.push(
            "Google Sheets import preview (dry run — no jobs were written)"
                .to_string(),
        );
        lines.push(String::new());
        lines.push(format!(
            "Source: {} (tab '{}')",
            GOOGLE_SHEETS_IMPORT_SOURCE, snapshot.sheet_name
        ));
        lines.push(format!(
            "Rows seen: {} | create: {} | update: {} | unchanged: {} | conflict: {} | invalid: {} | skipped: {}",
            diff.seen,
            diff.creates.len(),
            diff.updates.len(),
            diff.unchanged.len(),
            diff.conflicts.len() + diff.reopen_conflicts.len(),
            diff.invalid.len(),
            snapshot.skipped,
        ));
        if !diff.complete_from_source.is_empty() {
            lines.push(format!(
                "Done in source (will complete locally): {}",
                diff.complete_from_source.join(", ")
            ));
        }
        if !diff.missing_from_source.is_empty() {
            lines.push(format!(
                "Missing from source (reported only, never deleted): {}",
                diff.missing_from_source.join(", ")
            ));
        }
        lines.push(String::new());
        if !diff.creates.is_empty() {
            lines.push("New jobs (create):".to_string());
            for row in &diff.creates {
                lines.push(preview_row_line("+", row));
            }
            lines.push(String::new());
        }
        if !diff.updates.is_empty() {
            lines.push("Changed jobs (update):".to_string());
            for row in &diff.updates {
                lines.push(preview_row_line("~", row));
            }
            lines.push(String::new());
        }
        if include_unchanged && !diff.unchanged.is_empty() {
            lines.push("Unchanged jobs:".to_string());
            for key in &diff.unchanged {
                lines.push(format!("  = {key}"));
            }
            lines.push(String::new());
        }
        if !diff.conflicts.is_empty() {
            lines.push("Conflicts:".to_string());
            for c in &diff.conflicts {
                lines.push(conflict_line(c));
            }
            lines.push(String::new());
        }
        if !diff.reopen_conflicts.is_empty() {
            lines.push(
                "Reopen conflicts (completed/ignored jobs reappeared as active):"
                    .to_string(),
            );
            for key in &diff.reopen_conflicts {
                lines.push(format!("  ! {key}"));
            }
            lines.push(String::new());
        }
        if !diff.invalid.is_empty() {
            lines.push("Invalid rows:".to_string());
            for i in &diff.invalid {
                lines.push(format!("  x row {}: {}", i.row_number, i.reason));
            }
            lines.push(String::new());
        }
        lines.push(format!("import_run_id: {}", run.id));
        lines.push(format!("preview_hash: {}", run.preview_hash));
        lines.push(String::new());
        lines.push("Review the diff above; if it looks right, apply it with pm_apply_google_sheet_site_jobs_import passing this import_run_id and preview_hash.".to_string());

        let detail = json!({
            "tool": "pm_preview_google_sheet_site_jobs_import",
            "source": snapshot.source,
            "spreadsheet_id": snapshot.spreadsheet_id,
            "sheet_name": snapshot.sheet_name,
            "import_run_id": run.id,
            "preview_hash": run.preview_hash,
            "dry_run": true,
            "counts": {
                "seen": diff.seen,
                "created": diff.creates.len(),
                "updated": diff.updates.len(),
                "unchanged": diff.unchanged.len(),
                "conflict": diff.conflicts.len() + diff.reopen_conflicts.len(),
                "invalid": diff.invalid.len(),
                "skipped": snapshot.skipped,
                "complete_from_source": diff.complete_from_source.len(),
                "missing_from_source": diff.missing_from_source.len(),
                "reopen_conflicts": diff.reopen_conflicts.len(),
            },
            "creates": creates,
            "updates": updates,
            "unchanged": diff.unchanged,
            "complete_from_source": diff.complete_from_source,
            "missing_from_source": diff.missing_from_source,
            "reopen_conflicts": diff.reopen_conflicts,
            "conflicts": conflicts,
            "invalid": invalid,
        });

        Ok(json_result(&lines.join("\n"), &detail))
    }
}

// ── pm_apply_google_sheet_site_jobs_import ─────────────────

pub struct PmApplyGoogleSheetSiteJobsImportTool {
    ctx: GoogleSheetsToolCtx,
}

impl PmApplyGoogleSheetSiteJobsImportTool {
    pub fn new(ctx: GoogleSheetsToolCtx) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for PmApplyGoogleSheetSiteJobsImportTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_apply_google_sheet_site_jobs_import".into(),
                description: "Apply a previously previewed Google Sheets import. Requires the import_run_id and preview_hash returned by pm_preview_google_sheet_site_jobs_import. Re-fetches the source and refuses to apply if the sheet changed since the preview. On match, commits the reviewed diff to the PM database.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "import_run_id": {
                            "type": "string",
                            "description": "import_run_id returned by pm_preview_google_sheet_site_jobs_import"
                        },
                        "preview_hash": {
                            "type": "string",
                            "description": "preview_hash returned by pm_preview_google_sheet_site_jobs_import"
                        }
                    },
                    "required": ["import_run_id", "preview_hash"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let import_run_id = required_string(&args, "import_run_id")?;
        let requested_hash = required_string(&args, "preview_hash")?;

        // 1. Verify the caller references a real, still-valid preview run.
        let stored_run = self
            .ctx
            .service
            .store()
            .get_import_run(&import_run_id)?
            .with_context(|| {
                format!("import run not found: {import_run_id}")
            })?;
        if !stored_run.dry_run {
            anyhow::bail!(
                "import_run_id {import_run_id} is not a preview run (dry_run=false); refusing to apply"
            );
        }
        if stored_run.preview_hash != requested_hash {
            anyhow::bail!(
                "preview_hash does not match the stored preview; use the import_run_id and preview_hash returned together by pm_preview_google_sheet_site_jobs_import"
            );
        }

        // 2. Refetch and recompute so only the reviewed state applies. If the
        //    source changed after the preview, refuse rather than silently
        //    applying a different diff.
        let snapshot = self.ctx.fetch_snapshot().await?;
        let fresh_diff = self.ctx.service.preview_import(&snapshot)?;
        let fresh_hash = preview_hash(&fresh_diff);
        if fresh_hash != requested_hash {
            anyhow::bail!(
                "source sheet changed since the preview was created; re-run pm_preview_google_sheet_site_jobs_import to review the new state before applying"
            );
        }

        // 3. Apply the reviewed state atomically.
        let applied = self.ctx.service.apply_import(&snapshot)?;

        let mut lines = Vec::new();
        lines.push("Google Sheets import applied".to_string());
        lines.push(format!("applied_run_id: {}", applied.id));
        lines.push(format!(
            "create: {} | update: {} | unchanged: {} | conflict: {} | invalid: {} | skipped: {}",
            applied.created_count,
            applied.updated_count,
            applied.unchanged_count,
            applied.conflict_count,
            applied.invalid_count,
            applied.skipped_count,
        ));
        if applied.summary["complete_from_source"]
            .as_i64()
            .unwrap_or(0)
            > 0
        {
            lines.push(format!(
                "completed from source: {}",
                applied.summary["complete_from_source"]
            ));
        }
        if applied.summary["missing_from_source"].as_i64().unwrap_or(0) > 0 {
            lines.push(format!(
                "missing from source (reported only, never deleted): {}",
                applied.summary["missing_from_source"]
            ));
        }
        lines.push(String::new());
        lines.push("Inspect the database with pm_list_imported_google_sheet_jobs and review any conflicts with pm_list_google_sheet_import_conflicts.".to_string());

        let detail = json!({
            "tool": "pm_apply_google_sheet_site_jobs_import",
            "preview_run_id": stored_run.id,
            "applied_run_id": applied.id,
            "preview_hash": requested_hash,
            "source": snapshot.source,
            "counts": {
                "seen": applied.seen_count,
                "created": applied.created_count,
                "updated": applied.updated_count,
                "unchanged": applied.unchanged_count,
                "conflict": applied.conflict_count,
                "invalid": applied.invalid_count,
                "skipped": applied.skipped_count,
                "complete_from_source": applied.summary["complete_from_source"],
                "missing_from_source": applied.summary["missing_from_source"],
            },
        });

        Ok(json_result(&lines.join("\n"), &detail))
    }
}

// ── pm_list_google_sheet_import_conflicts ──────────────────

pub struct PmListGoogleSheetImportConflictsTool {
    ctx: GoogleSheetsToolCtx,
}

impl PmListGoogleSheetImportConflictsTool {
    pub fn new(ctx: GoogleSheetsToolCtx) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for PmListGoogleSheetImportConflictsTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_list_google_sheet_import_conflicts".into(),
                description: "List Google Sheets import conflicts (blank/duplicate titles, invalid rows, reopen-completed) from the PM database. Optionally filter by review status and exact job key.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "description": "Which conflicts to list: open (default), resolved, or all.",
                            "enum": ["open", "resolved", "all"]
                        },
                        "job_key": {
                            "type": "string",
                            "description": "Optional exact Title of Activity to filter by."
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let status = args
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("open");
        if !matches!(status, "open" | "resolved" | "all") {
            anyhow::bail!(
                "invalid status '{status}'; expected one of: open, resolved, all"
            );
        }
        let job_key = args
            .get("job_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let store = self.ctx.service.store();
        let mut conflicts: Vec<ImportConflictRecord> = Vec::new();
        if matches!(status, "open" | "all") {
            conflicts.extend(store.list_import_conflicts(
                GOOGLE_SHEETS_IMPORT_SOURCE,
                ImportConflictStatus::Open,
            )?);
        }
        if matches!(status, "resolved" | "all") {
            conflicts.extend(store.list_import_conflicts(
                GOOGLE_SHEETS_IMPORT_SOURCE,
                ImportConflictStatus::Resolved,
            )?);
        }
        if let Some(jk) = &job_key {
            conflicts.retain(|c| &c.external_job_key == jk);
        }
        if status == "all" {
            conflicts.sort_by_key(|c| c.id);
        }

        let conflict_details: Vec<Value> = conflicts
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "import_run_id": c.import_run_id,
                    "external_job_key": c.external_job_key,
                    "conflict_type": serde_json::to_value(c.conflict_type).unwrap_or(Value::Null),
                    "reason": c.reason,
                    "details": c.details,
                    "status": serde_json::to_value(c.status).unwrap_or(Value::Null),
                    "created_at": c.created_at,
                    "resolved_at": c.resolved_at,
                })
            })
            .collect();

        let mut lines = Vec::new();
        lines.push(format!(
            "Google Sheets import conflicts ({status}) — {}",
            conflicts.len()
        ));
        if conflicts.is_empty() {
            lines.push(String::new());
            lines.push("  (none)".to_string());
        } else {
            for c in &conflicts {
                lines.push(String::new());
                lines.push(format!(
                    "  #{} [{}] '{}' — {}",
                    c.id,
                    serde_str(&c.conflict_type),
                    if c.external_job_key.is_empty() {
                        "(no key)".to_string()
                    } else {
                        c.external_job_key.clone()
                    },
                    c.reason
                ));
                if let Some(run) = &c.import_run_id {
                    lines.push(format!(
                        "    run: {run} | created: {}",
                        c.created_at
                    ));
                }
            }
        }

        let detail = json!({
            "tool": "pm_list_google_sheet_import_conflicts",
            "source": GOOGLE_SHEETS_IMPORT_SOURCE,
            "status": status,
            "job_key": job_key,
            "count": conflicts.len(),
            "conflicts": conflict_details,
        });

        Ok(json_result(&lines.join("\n"), &detail))
    }
}

// ── pm_list_imported_google_sheet_jobs ─────────────────────

pub struct PmListImportedGoogleSheetJobsTool {
    ctx: GoogleSheetsToolCtx,
}

impl PmListImportedGoogleSheetJobsTool {
    pub fn new(ctx: GoogleSheetsToolCtx) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for PmListImportedGoogleSheetJobsTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_list_imported_google_sheet_jobs".into(),
                description: "List jobs imported from Google Sheets into the PM database. Optionally filter by local status (active/completed/ignored) and exact job key. Use this to inspect what the database holds after a sync.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "description": "Which imported jobs to list: all (default), active, completed, or ignored.",
                            "enum": ["active", "completed", "ignored", "all"]
                        },
                        "job_key": {
                            "type": "string",
                            "description": "Optional exact Title of Activity to filter by."
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let status =
            args.get("status").and_then(|v| v.as_str()).unwrap_or("all");
        let wanted = match status {
            "all" => None,
            "active" => Some(ImportedJobStatus::Active),
            "completed" => Some(ImportedJobStatus::Completed),
            "ignored" => Some(ImportedJobStatus::Ignored),
            other => {
                anyhow::bail!(
                    "invalid status '{other}'; expected one of: active, completed, ignored, all"
                );
            }
        };
        let job_key = args
            .get("job_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let store = self.ctx.service.store();
        let mut jobs = store.list_imported_jobs(GOOGLE_SHEETS_IMPORT_SOURCE)?;
        if let Some(w) = wanted {
            jobs.retain(|j| j.local_status == w);
        }
        if let Some(jk) = &job_key {
            jobs.retain(|j| &j.external_job_key == jk);
        }

        let job_details: Vec<Value> = jobs
            .iter()
            .map(|j| {
                json!({
                    "entity_id": j.entity_id,
                    "external_job_key": j.external_job_key,
                    "title": j.title,
                    "location": j.location,
                    "activity_type": j.activity_type,
                    "start_date": j.start_date,
                    "end_date": j.end_date,
                    "job_leader": j.job_leader,
                    "team_member": j.team_member,
                    "robots": j.robots,
                    "additional_info": j.additional_info,
                    "local_status": serde_json::to_value(j.local_status).unwrap_or(Value::Null),
                    "source_done": j.source_done,
                    "first_seen_at": j.first_seen_at,
                    "last_seen_at": j.last_seen_at,
                    "last_import_run_id": j.last_import_run_id,
                })
            })
            .collect();

        let mut lines = Vec::new();
        lines.push(format!(
            "Imported Google Sheets jobs ({status}) — {}",
            jobs.len()
        ));
        if jobs.is_empty() {
            lines.push(String::new());
            lines.push("  (none)".to_string());
        } else {
            for j in &jobs {
                let dates = match (&j.start_date, &j.end_date) {
                    (Some(s), Some(e)) => format!("{s} -> {e}"),
                    (Some(s), None) => s.clone(),
                    (None, Some(e)) => format!("-> {e}"),
                    (None, None) => "-".to_string(),
                };
                lines.push(format!(
                    "  [{}] {} | {} | {} | leader: {}",
                    serde_str(&j.local_status),
                    j.title,
                    j.location,
                    dates,
                    j.job_leader
                ));
            }
        }

        let detail = json!({
            "tool": "pm_list_imported_google_sheet_jobs",
            "source": GOOGLE_SHEETS_IMPORT_SOURCE,
            "status": status,
            "job_key": job_key,
            "count": jobs.len(),
            "jobs": job_details,
        });

        Ok(json_result(&lines.join("\n"), &detail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm::google_sheets::ValuesResponse;
    use crate::pm::imports::row_hash;
    use crate::pm::schema;
    use crate::pm::store::Store;
    use marshaling_protocol::pm::{
        ImportConflictInput, ImportConflictType, ImportRunStatus,
        ImportedJobInput,
    };
    use rusqlite::Connection;
    use std::sync::Mutex as StdMutex;

    // ── Mock fetcher ─────────────────────────────────────────

    /// Queued canned responses. The last response repeats for every extra
    /// fetch; earlier responses are consumed once each (so a test can model
    /// the source changing between preview and apply).
    #[derive(Clone, Default)]
    struct MockSheetFetcher {
        responses: Arc<StdMutex<Vec<ValuesResponse>>>,
    }

    impl MockSheetFetcher {
        fn queue(&self, response: ValuesResponse) {
            self.responses.lock().unwrap().push(response);
        }
    }

    #[async_trait]
    impl SheetFetcher for MockSheetFetcher {
        async fn fetch_sheet(
            &self,
            _account: &ServiceAccount,
            _spreadsheet_id: &str,
            _sheet_name: &str,
        ) -> Result<ValuesResponse> {
            let mut guard = self.responses.lock().unwrap();
            match guard.len() {
                0 => anyhow::bail!("mock fetcher has no responses queued"),
                1 => Ok(guard[0].clone()),
                _ => Ok(guard.remove(0)),
            }
        }
    }

    // ── Test fixtures ────────────────────────────────────────

    fn test_service() -> Arc<PmService> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        schema::bootstrap(&conn).unwrap();
        Arc::new(PmService::new(Store::new(conn)))
    }

    /// A context with the given config and a mock fetcher (no HTTP, no real
    /// credentials).
    fn ctx_with_config(config: GoogleSheetsConfig) -> GoogleSheetsToolCtx {
        GoogleSheetsToolCtx::with_fetcher(
            test_service(),
            config,
            Arc::new(MockSheetFetcher::default()),
        )
    }

    /// A fully-enabled context with a temp credentials file; returns the
    /// context plus the tempdir so the credential file stays alive.
    fn test_ctx_with(
        fetcher: MockSheetFetcher,
    ) -> (GoogleSheetsToolCtx, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("sa.json");
        std::fs::write(
            &creds_path,
            json!({
                "type": "service_account",
                "private_key": "test-key",
                "client_email": "sa@example.com",
            })
            .to_string(),
        )
        .unwrap();

        let config = GoogleSheetsConfig {
            enabled: true,
            spreadsheet_id: Some("spreadsheet-1".into()),
            sheet_name: "US".into(),
            credentials_path: Some(creds_path),
            eligible_activity_types: vec!["Boris Job".into(), "Demo".into()],
        };
        let ctx = GoogleSheetsToolCtx::with_fetcher(
            test_service(),
            config,
            Arc::new(fetcher),
        );
        (ctx, dir)
    }

    fn header_row() -> Vec<Value> {
        vec![
            Value::from(crate::pm::google_sheets::COL_OFFICE),
            Value::from(crate::pm::google_sheets::COL_ACTIVITY_TYPE),
            Value::from(crate::pm::google_sheets::COL_TITLE),
            Value::from(crate::pm::google_sheets::COL_SITE),
            Value::from(crate::pm::google_sheets::COL_START_DATE),
            Value::from(crate::pm::google_sheets::COL_END_DATE),
            Value::from(crate::pm::google_sheets::COL_JOB_LEADER),
            Value::from(crate::pm::google_sheets::COL_TEAM_MEMBER),
            Value::from(crate::pm::google_sheets::COL_ROBOTS),
            Value::from(crate::pm::google_sheets::COL_ADDITIONAL_INFO),
            Value::from(crate::pm::google_sheets::COL_DONE),
        ]
    }

    fn response_with_rows(rows: Vec<Vec<Value>>) -> ValuesResponse {
        let mut all: Vec<Vec<Value>> = vec![header_row()];
        all.extend(rows);
        ValuesResponse {
            range: "US".into(),
            major_dimension: "ROWS".into(),
            values: all,
        }
    }

    fn job_row(title: &str, location: &str, leader: &str) -> Vec<Value> {
        serde_json::from_value(json!([
            "Shenzhen",
            "Boris Job",
            title,
            location,
            "5-Aug-2026",
            "12-Aug-2026",
            leader,
            "",
            "R1",
            "",
            ""
        ]))
        .unwrap()
    }

    fn job_row_with_info(
        title: &str,
        location: &str,
        leader: &str,
        info: &str,
    ) -> Vec<Value> {
        serde_json::from_value(json!([
            "Shenzhen",
            "Boris Job",
            title,
            location,
            "5-Aug-2026",
            "12-Aug-2026",
            leader,
            "",
            "R1",
            info,
            ""
        ]))
        .unwrap()
    }

    /// Run a preview against `ctx` and return the persisted run id + hash.
    async fn do_preview(ctx: &GoogleSheetsToolCtx) -> (String, String) {
        let tool = PmPreviewGoogleSheetSiteJobsImportTool::new(ctx.clone());
        tool.execute(json!({})).await.unwrap();
        let run = ctx
            .service
            .store()
            .list_import_runs(GOOGLE_SHEETS_IMPORT_SOURCE, 1)
            .unwrap()
            .remove(0);
        (run.id, run.preview_hash)
    }

    /// Extract the ```json``` detail block from a tool result.
    fn output_json(output: &str) -> Value {
        let start = output
            .rfind("```json")
            .map(|i| i + "```json\n".len())
            .unwrap_or(0);
        let end = output[start..]
            .find("```")
            .map(|i| start + i)
            .unwrap_or(output.len());
        serde_json::from_str(output[start..end].trim()).unwrap()
    }

    fn err_text(result: Result<ToolExecutionResult>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    // ── Preview: config validation ───────────────────────────

    #[tokio::test]
    async fn test_preview_rejects_disabled_config() {
        let tool = PmPreviewGoogleSheetSiteJobsImportTool::new(
            ctx_with_config(GoogleSheetsConfig::default()),
        );
        let err = err_text(tool.execute(json!({})).await);
        assert!(err.contains("not enabled"), "err: {err}");
        assert!(
            !err.contains("private_key") && !err.contains("BEGIN"),
            "must never leak credentials: {err}"
        );
    }

    #[tokio::test]
    async fn test_preview_rejects_missing_spreadsheet_id() {
        let config = GoogleSheetsConfig {
            enabled: true,
            spreadsheet_id: None,
            ..GoogleSheetsConfig::default()
        };
        let tool = PmPreviewGoogleSheetSiteJobsImportTool::new(
            ctx_with_config(config),
        );
        let err = err_text(tool.execute(json!({})).await);
        assert!(err.contains("spreadsheet_id"), "err: {err}");
    }

    #[tokio::test]
    async fn test_preview_rejects_missing_credentials_path() {
        let config = GoogleSheetsConfig {
            enabled: true,
            spreadsheet_id: Some("s1".into()),
            credentials_path: None,
            ..GoogleSheetsConfig::default()
        };
        let tool = PmPreviewGoogleSheetSiteJobsImportTool::new(
            ctx_with_config(config),
        );
        let err = err_text(tool.execute(json!({})).await);
        assert!(err.contains("credentials_path"), "err: {err}");
    }

    #[tokio::test]
    async fn test_preview_rejects_missing_credentials_file() {
        let config = GoogleSheetsConfig {
            enabled: true,
            spreadsheet_id: Some("s1".into()),
            credentials_path: Some("/nonexistent/sa.json".into()),
            ..GoogleSheetsConfig::default()
        };
        let tool = PmPreviewGoogleSheetSiteJobsImportTool::new(
            ctx_with_config(config),
        );
        let err = err_text(tool.execute(json!({})).await);
        assert!(err.contains("credentials file not found"), "err: {err}");
    }

    // ── Preview: success ─────────────────────────────────────

    #[tokio::test]
    async fn test_preview_persists_dry_run_and_mutates_nothing() {
        let fetcher = MockSheetFetcher::default();
        fetcher.queue(response_with_rows(vec![job_row(
            "Site A install",
            "100 Bay St",
            "Alice",
        )]));
        let (ctx, _dir) = test_ctx_with(fetcher);
        let tool = PmPreviewGoogleSheetSiteJobsImportTool::new(ctx.clone());

        let result = tool.execute(json!({})).await.unwrap();
        assert!(
            result.output.contains("import_run_id"),
            "output: {}",
            result.output
        );
        assert!(
            result.output.contains("preview_hash"),
            "output: {}",
            result.output
        );
        assert!(
            result.output.contains("create: 1"),
            "output: {}",
            result.output
        );

        // No imported jobs written by a preview.
        assert!(
            ctx.service
                .store()
                .list_imported_jobs(GOOGLE_SHEETS_IMPORT_SOURCE)
                .unwrap()
                .is_empty()
        );

        // A dry-run preview run was persisted with the right bookkeeping.
        let runs = ctx
            .service
            .store()
            .list_import_runs(GOOGLE_SHEETS_IMPORT_SOURCE, 10)
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].dry_run);
        assert_eq!(runs[0].status, ImportRunStatus::Previewed);
        assert_eq!(runs[0].created_count, 1);
        assert_eq!(runs[0].seen_count, 1);
    }

    #[tokio::test]
    async fn test_preview_output_is_structured_json() {
        let fetcher = MockSheetFetcher::default();
        fetcher.queue(response_with_rows(vec![
            job_row("Site A install", "100 Bay St", "Alice"),
            job_row("Customer demo", "200 Main Rd", "Bob"),
        ]));
        let (ctx, _dir) = test_ctx_with(fetcher);
        let tool = PmPreviewGoogleSheetSiteJobsImportTool::new(ctx);

        let result = tool.execute(json!({})).await.unwrap();
        let detail = output_json(&result.output);
        assert_eq!(detail["tool"], "pm_preview_google_sheet_site_jobs_import");
        assert_eq!(detail["dry_run"], true);
        assert_eq!(detail["counts"]["created"], 2);
        assert_eq!(detail["counts"]["skipped"], 0);
        assert_eq!(detail["creates"][0]["title"], "Site A install");
    }

    #[tokio::test]
    async fn test_preview_includes_additional_info_for_update() {
        let fetcher = MockSheetFetcher::default();
        fetcher.queue(response_with_rows(vec![job_row_with_info(
            "Site A install",
            "100 Bay St",
            "Alice",
            "bring drill",
        )]));
        let (ctx, _dir) = test_ctx_with(fetcher);

        // Stored job matches the previewed row except Additional Info, so the
        // only change is an additional_info-only update.
        let stored_row = NormalizedRow {
            row_number: 1,
            title: "Site A install".into(),
            location: "100 Bay St".into(),
            activity_type: "Boris Job".into(),
            start_date: Some("2026-08-05".into()),
            end_date: Some("2026-08-12".into()),
            job_leader: "Alice".into(),
            team_member: String::new(),
            robots: "R1".into(),
            additional_info: String::new(),
            done: false,
        };
        ctx.service
            .store()
            .upsert_imported_job(&ImportedJobInput {
                source: GOOGLE_SHEETS_IMPORT_SOURCE.into(),
                spreadsheet_id: "spreadsheet-1".into(),
                sheet_name: "US".into(),
                external_job_key: "Site A install".into(),
                title: "Site A install".into(),
                location: "100 Bay St".into(),
                activity_type: "Boris Job".into(),
                start_date: Some("2026-08-05".into()),
                end_date: Some("2026-08-12".into()),
                job_leader: "Alice".into(),
                team_member: String::new(),
                robots: "R1".into(),
                additional_info: String::new(),
                source_done: false,
                local_status: ImportedJobStatus::Active,
                row_hash: row_hash(&stored_row),
                source_payload: json!({}),
                import_run_id: None,
            })
            .unwrap();

        let tool = PmPreviewGoogleSheetSiteJobsImportTool::new(ctx);
        let result = tool.execute(json!({})).await.unwrap();
        let detail = output_json(&result.output);

        // The change is classified as an update, and the new Additional Info
        // is visible in both the structured row JSON and the readable text.
        assert_eq!(detail["counts"]["created"], 0);
        assert_eq!(detail["counts"]["updated"], 1);
        assert_eq!(detail["updates"][0]["additional_info"], "bring drill");
        assert!(
            result.output.contains("info: bring drill"),
            "output: {}",
            result.output
        );
    }

    // ── Apply: guards ────────────────────────────────────────

    #[tokio::test]
    async fn test_apply_requires_arguments() {
        let (ctx, _dir) = test_ctx_with(MockSheetFetcher::default());
        let tool = PmApplyGoogleSheetSiteJobsImportTool::new(ctx);
        let err = err_text(tool.execute(json!({})).await);
        assert!(err.contains("import_run_id"), "err: {err}");
    }

    #[tokio::test]
    async fn test_apply_rejects_unknown_run() {
        let (ctx, _dir) = test_ctx_with(MockSheetFetcher::default());
        let tool = PmApplyGoogleSheetSiteJobsImportTool::new(ctx);
        let err = err_text(
            tool.execute(json!({
                "import_run_id": "nope",
                "preview_hash": "abc"
            }))
            .await,
        );
        assert!(err.contains("import run not found"), "err: {err}");
    }

    #[tokio::test]
    async fn test_apply_rejects_mismatched_preview_hash() {
        let fetcher = MockSheetFetcher::default();
        fetcher.queue(response_with_rows(vec![job_row(
            "Site A install",
            "100 Bay St",
            "Alice",
        )]));
        let (ctx, _dir) = test_ctx_with(fetcher);
        let (run_id, _hash) = do_preview(&ctx).await;
        let tool = PmApplyGoogleSheetSiteJobsImportTool::new(ctx);
        let err = err_text(
            tool.execute(json!({
                "import_run_id": run_id,
                "preview_hash": "wrong-hash"
            }))
            .await,
        );
        assert!(err.contains("does not match"), "err: {err}");
    }

    #[tokio::test]
    async fn test_apply_rejects_source_changed_after_preview() {
        let fetcher = MockSheetFetcher::default();
        // First fetch (preview) sees the original row; second fetch (apply)
        // sees a changed location — the source moved after the preview.
        fetcher.queue(response_with_rows(vec![job_row(
            "Site A install",
            "100 Bay St",
            "Alice",
        )]));
        fetcher.queue(response_with_rows(vec![job_row(
            "Site A install",
            "999 Changed St",
            "Alice",
        )]));
        let (ctx, _dir) = test_ctx_with(fetcher);
        let (run_id, hash) = do_preview(&ctx).await;
        let tool = PmApplyGoogleSheetSiteJobsImportTool::new(ctx.clone());
        let err = err_text(
            tool.execute(json!({
                "import_run_id": run_id,
                "preview_hash": hash
            }))
            .await,
        );
        assert!(err.contains("changed since the preview"), "err: {err}");
        // Nothing was applied.
        assert!(
            ctx.service
                .store()
                .list_imported_jobs(GOOGLE_SHEETS_IMPORT_SOURCE)
                .unwrap()
                .is_empty()
        );
    }

    // ── Apply: success ───────────────────────────────────────

    #[tokio::test]
    async fn test_apply_success_applies_reviewed_state() {
        let fetcher = MockSheetFetcher::default();
        fetcher.queue(response_with_rows(vec![job_row(
            "Site A install",
            "100 Bay St",
            "Alice",
        )]));
        let (ctx, _dir) = test_ctx_with(fetcher);
        let (run_id, hash) = do_preview(&ctx).await;
        let tool = PmApplyGoogleSheetSiteJobsImportTool::new(ctx.clone());

        let result = tool
            .execute(json!({
                "import_run_id": run_id,
                "preview_hash": hash
            }))
            .await
            .unwrap();
        assert!(
            result.output.contains("applied"),
            "output: {}",
            result.output
        );

        // The reviewed row is now in the database.
        let jobs = ctx
            .service
            .store()
            .list_imported_jobs(GOOGLE_SHEETS_IMPORT_SOURCE)
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].title, "Site A install");
        assert_eq!(jobs[0].location, "100 Bay St");
        assert_eq!(jobs[0].job_leader, "Alice");

        // Two runs: the applied run (newest first) and the preview (dry run).
        let runs = ctx
            .service
            .store()
            .list_import_runs(GOOGLE_SHEETS_IMPORT_SOURCE, 10)
            .unwrap();
        assert_eq!(runs.len(), 2);
        assert!(!runs[0].dry_run);
        assert_eq!(runs[0].status, ImportRunStatus::Applied);
        assert_eq!(runs[0].created_count, 1);
        assert!(runs[1].dry_run);
        assert_eq!(runs[1].status, ImportRunStatus::Previewed);
    }

    // ── List conflicts ───────────────────────────────────────

    fn insert_conflict(
        store: &Store,
        key: &str,
        conflict_type: ImportConflictType,
    ) -> i64 {
        store
            .insert_import_conflict(&ImportConflictInput {
                import_run_id: Some("run-1".into()),
                source: GOOGLE_SHEETS_IMPORT_SOURCE.into(),
                external_job_key: key.into(),
                conflict_type,
                reason: format!("test conflict for {key}"),
                details: json!({}),
            })
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn test_list_conflicts_defaults_to_open_and_filters() {
        let (ctx, _dir) = test_ctx_with(MockSheetFetcher::default());
        let store = ctx.service.store();
        let id_a =
            insert_conflict(store, "Job A", ImportConflictType::DuplicateTitle);
        insert_conflict(store, "Job B", ImportConflictType::BlankTitle);
        store.resolve_import_conflict(id_a).unwrap();

        let tool = PmListGoogleSheetImportConflictsTool::new(ctx.clone());

        // Default: open only.
        let res = tool.execute(json!({})).await.unwrap();
        let detail = output_json(&res.output);
        assert_eq!(detail["status"], "open");
        assert_eq!(detail["count"], 1);
        assert_eq!(detail["conflicts"][0]["external_job_key"], "Job B");

        // All statuses.
        let res = tool.execute(json!({"status": "all"})).await.unwrap();
        let detail = output_json(&res.output);
        assert_eq!(detail["count"], 2);

        // Filter by exact job key.
        let res = tool
            .execute(json!({"status": "all", "job_key": "Job A"}))
            .await
            .unwrap();
        let detail = output_json(&res.output);
        assert_eq!(detail["count"], 1);
        assert_eq!(detail["conflicts"][0]["external_job_key"], "Job A");

        // Invalid status.
        let err = err_text(tool.execute(json!({"status": "bogus"})).await);
        assert!(err.contains("invalid status"), "err: {err}");
    }

    // ── List imported jobs ───────────────────────────────────

    fn job_input(key: &str) -> ImportedJobInput {
        ImportedJobInput {
            source: GOOGLE_SHEETS_IMPORT_SOURCE.into(),
            spreadsheet_id: "s1".into(),
            sheet_name: "US".into(),
            external_job_key: key.into(),
            title: key.into(),
            location: "loc".into(),
            activity_type: "Boris Job".into(),
            start_date: Some("2026-08-05".into()),
            end_date: None,
            job_leader: "Alice".into(),
            team_member: String::new(),
            robots: "R1".into(),
            additional_info: String::new(),
            source_done: false,
            local_status: ImportedJobStatus::Active,
            row_hash: format!("hash-{key}"),
            source_payload: json!({}),
            import_run_id: None,
        }
    }

    #[tokio::test]
    async fn test_list_imported_jobs_filters() {
        let (ctx, _dir) = test_ctx_with(MockSheetFetcher::default());
        let store = ctx.service.store();
        store.upsert_imported_job(&job_input("Job A")).unwrap();
        store.upsert_imported_job(&job_input("Job B")).unwrap();
        store
            .mark_imported_job_completed(
                GOOGLE_SHEETS_IMPORT_SOURCE,
                "Job B",
                None,
            )
            .unwrap();

        let tool = PmListImportedGoogleSheetJobsTool::new(ctx.clone());

        // Default: all, ordered by title.
        let res = tool.execute(json!({})).await.unwrap();
        let detail = output_json(&res.output);
        assert_eq!(detail["count"], 2);
        assert_eq!(detail["jobs"][0]["external_job_key"], "Job A");
        assert_eq!(detail["jobs"][1]["local_status"], "completed");

        // Filter by status.
        let res = tool.execute(json!({"status": "completed"})).await.unwrap();
        let detail = output_json(&res.output);
        assert_eq!(detail["count"], 1);
        assert_eq!(detail["jobs"][0]["external_job_key"], "Job B");

        // Filter by exact job key.
        let res = tool.execute(json!({"job_key": "Job A"})).await.unwrap();
        let detail = output_json(&res.output);
        assert_eq!(detail["count"], 1);
        assert_eq!(detail["jobs"][0]["external_job_key"], "Job A");

        // Invalid status.
        let err = err_text(tool.execute(json!({"status": "bogus"})).await);
        assert!(err.contains("invalid status"), "err: {err}");
    }

    #[tokio::test]
    async fn test_tool_defs_are_stable() {
        let (ctx, _dir) = test_ctx_with(MockSheetFetcher::default());
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(PmPreviewGoogleSheetSiteJobsImportTool::new(ctx.clone())),
            Box::new(PmApplyGoogleSheetSiteJobsImportTool::new(ctx.clone())),
            Box::new(PmListGoogleSheetImportConflictsTool::new(ctx.clone())),
            Box::new(PmListImportedGoogleSheetJobsTool::new(ctx)),
        ];
        let expected = [
            "pm_preview_google_sheet_site_jobs_import",
            "pm_apply_google_sheet_site_jobs_import",
            "pm_list_google_sheet_import_conflicts",
            "pm_list_imported_google_sheet_jobs",
        ];
        for (tool, name) in tools.iter().zip(expected.iter()) {
            let def = tool.def();
            assert_eq!(def.function.name, *name);
            let params = def
                .function
                .parameters
                .as_object()
                .expect("param schema must be object");
            assert!(params.contains_key("type"));
            assert!(params.contains_key("properties"));
        }
        // Apply requires both arguments.
        let apply = &tools[1].def().function.parameters;
        let required = apply["required"].as_array().unwrap();
        assert!(required.contains(&Value::from("import_run_id")));
        assert!(required.contains(&Value::from("preview_hash")));
    }
}
