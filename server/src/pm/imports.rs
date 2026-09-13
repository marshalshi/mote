//! Google Sheets import service (slice 2): title-keyed diff + apply.
//!
//! Consumes the normalized types from [`crate::pm::google_sheets`] and turns
//! them into a pure, classified diff against the stored imported jobs, then
//! applies that diff atomically through the store.
//!
//! # Classification contract
//!
//! - The exact source `Title of Activity` is the stable external job key. A
//!   row is matched to a stored job by that key alone — never by row index,
//!   location, or date.
//! - Every mapped source field is compared (via a stable row hash): new keys
//!   are `create`, changed keys are `update`, identical keys are `unchanged`.
//! - A stored active job whose title is now marked Done in the source is
//!   `complete_from_source` (completed locally, never deleted). If the same
//!   title is both active and done in one read, the active row wins.
//! - A stored active job absent from the latest read is `missing_from_source`:
//!   reported only, never deleted and never auto-completed.
//! - A stored completed/ignored job that reappears as active is a
//!   `reopen_conflicts` requiring user review — no silent reopen.
//! - Normalization conflicts and invalid rows are passed through untouched by
//!   classification; on apply, conflicts and invalid rows are persisted as
//!   open `import_conflicts` rows (`InvalidRow` for the latter) so the list
//!   tool can surface them for operator review.
//!
//! # Wiring status
//!
//! Slice 3 wires the preview/apply/persist entry points into the PM import
//! tools (`google_sheets_tools`), so the core diff/apply path is exercised by
//! the binary.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};

use crate::pm::PmService;
use crate::pm::google_sheets::{InvalidRow, NormalizedRow, RowConflict};
use crate::pm::store::Store;

use marshaling_protocol::pm::{
    ImportConflictInput, ImportConflictType, ImportRunInput, ImportRunRecord,
    ImportRunStatus, ImportedJobInput, ImportedJobRecord, ImportedJobStatus,
};

// ── Stable hashing ───────────────────────────────────────────

/// Stable 64-bit FNV-1a hash rendered as lowercase hex.
///
/// Dependency-free and stable across runs and Rust releases, unlike
/// `std::collections::hash_map::DefaultHasher`. Adequate for change detection
/// over a few hundred rows.
fn fnv1a64(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Canonical serialization of a normalized row's mapped source fields.
///
/// Fields are unit-separator delimited so boundaries stay unambiguous.
fn canonical_row(row: &NormalizedRow) -> String {
    [
        row.title.clone(),
        row.location.clone(),
        row.activity_type.clone(),
        row.start_date.clone().unwrap_or_default(),
        row.end_date.clone().unwrap_or_default(),
        row.job_leader.clone(),
        row.team_member.clone(),
        row.robots.clone(),
        row.additional_info.clone(),
        row.done.to_string(),
    ]
    .join("\u{1f}")
}

/// Stable hash of a normalized row's mapped source fields.
pub fn row_hash(row: &NormalizedRow) -> String {
    fnv1a64(&canonical_row(row))
}

// ── Snapshot and diff ────────────────────────────────────────

/// One import snapshot: the normalized output of a single sheet read.
#[derive(Debug, Clone)]
pub struct ImportSnapshot {
    pub source: String,
    pub spreadsheet_id: String,
    pub sheet_name: String,
    /// Eligible active rows from the source read.
    pub rows: Vec<NormalizedRow>,
    /// Normalization conflicts passed through (blank/duplicate titles).
    pub conflicts: Vec<RowConflict>,
    /// Normalization invalid rows passed through.
    pub invalid: Vec<InvalidRow>,
    /// Exact titles of eligible rows currently marked Done in the source.
    pub done_titles: Vec<String>,
    /// Rows skipped by normalization as ineligible/blank.
    pub skipped: usize,
}

/// Classified diff between an import snapshot and the stored jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDiff {
    /// New external job keys → create.
    pub creates: Vec<NormalizedRow>,
    /// Existing active jobs with changed mapped fields → update.
    pub updates: Vec<NormalizedRow>,
    /// Existing active jobs identical to the source → no-op.
    pub unchanged: Vec<String>,
    /// Stored active jobs now marked Done → complete/ignore locally.
    pub complete_from_source: Vec<String>,
    /// Stored active jobs absent from the latest read → report only, never delete.
    pub missing_from_source: Vec<String>,
    /// Normalization conflicts passed through.
    pub conflicts: Vec<RowConflict>,
    /// Normalization invalid rows passed through.
    pub invalid: Vec<InvalidRow>,
    /// Stored completed/ignored jobs that reappeared as active → user review.
    pub reopen_conflicts: Vec<String>,
    /// Total eligible active rows classified.
    pub seen: usize,
}

/// Pure classification of an import snapshot against stored jobs.
///
/// See the module docs for the exact matching/conflict rules.
pub fn classify_import(
    rows: &[NormalizedRow],
    done_titles: &[String],
    stored: &[ImportedJobRecord],
    conflicts: &[RowConflict],
    invalid: &[InvalidRow],
) -> ImportDiff {
    let stored_by_key: HashMap<&str, &ImportedJobRecord> = stored
        .iter()
        .map(|r| (r.external_job_key.as_str(), r))
        .collect();
    let done_set: HashSet<&str> =
        done_titles.iter().map(String::as_str).collect();

    let mut active_keys: HashSet<String> = HashSet::new();
    let mut creates: Vec<NormalizedRow> = Vec::new();
    let mut updates: Vec<NormalizedRow> = Vec::new();
    let mut unchanged: Vec<String> = Vec::new();
    let mut reopen_conflicts: Vec<String> = Vec::new();

    for row in rows {
        active_keys.insert(row.title.clone());
        match stored_by_key.get(row.title.as_str()) {
            None => creates.push(row.clone()),
            Some(stored_job) => match stored_job.local_status {
                ImportedJobStatus::Active => {
                    if row_hash(row) == stored_job.row_hash {
                        unchanged.push(row.title.clone());
                    } else {
                        updates.push(row.clone());
                    }
                }
                ImportedJobStatus::Completed | ImportedJobStatus::Ignored => {
                    reopen_conflicts.push(row.title.clone());
                }
            },
        }
    }

    let mut complete_from_source = Vec::new();
    let mut missing_from_source = Vec::new();
    for job in stored {
        if job.local_status != ImportedJobStatus::Active {
            continue;
        }
        // Active rows win over done rows for the same title in one read.
        if active_keys.contains(&job.external_job_key) {
            continue;
        }
        if done_set.contains(job.external_job_key.as_str()) {
            complete_from_source.push(job.external_job_key.clone());
        } else {
            missing_from_source.push(job.external_job_key.clone());
        }
    }

    ImportDiff {
        creates,
        updates,
        unchanged,
        complete_from_source,
        missing_from_source,
        conflicts: conflicts.to_vec(),
        invalid: invalid.to_vec(),
        reopen_conflicts,
        seen: rows.len(),
    }
}

/// Deterministic hash of a classified diff.
///
/// The same snapshot always produces the same hash, so a preview can later be
/// verified before applying. The exact encoding is an implementation detail.
pub fn preview_hash(diff: &ImportDiff) -> String {
    let mut parts: Vec<String> = Vec::new();
    for row in &diff.creates {
        parts.push(format!("create:{}", canonical_row(row)));
    }
    for row in &diff.updates {
        parts.push(format!("update:{}", canonical_row(row)));
    }
    for key in &diff.unchanged {
        parts.push(format!("unchanged:{key}"));
    }
    for key in &diff.complete_from_source {
        parts.push(format!("complete:{key}"));
    }
    for key in &diff.missing_from_source {
        parts.push(format!("missing:{key}"));
    }
    for key in &diff.reopen_conflicts {
        parts.push(format!("reopen:{key}"));
    }
    for conflict in &diff.conflicts {
        match conflict {
            RowConflict::BlankTitle { row_number } => {
                parts.push(format!("conflict:blank:{row_number}"));
            }
            RowConflict::DuplicateTitle { title, row_numbers } => {
                parts.push(format!(
                    "conflict:duplicate:{title}:{row_numbers:?}"
                ));
            }
        }
    }
    for invalid in &diff.invalid {
        parts
            .push(format!("invalid:{}:{}", invalid.row_number, invalid.reason));
    }
    parts.sort();
    fnv1a64(&parts.join("\u{1e}"))
}

// ── Mapping helpers ──────────────────────────────────────────

/// Map a normalized row to a store input, carrying the provenance + row hash.
fn job_input_from_row(
    row: &NormalizedRow,
    source: &str,
    spreadsheet_id: &str,
    sheet_name: &str,
    run_id: Option<&str>,
) -> Result<ImportedJobInput> {
    Ok(ImportedJobInput {
        source: source.to_string(),
        spreadsheet_id: spreadsheet_id.to_string(),
        sheet_name: sheet_name.to_string(),
        external_job_key: row.title.clone(),
        title: row.title.clone(),
        location: row.location.clone(),
        activity_type: row.activity_type.clone(),
        start_date: row.start_date.clone(),
        end_date: row.end_date.clone(),
        job_leader: row.job_leader.clone(),
        team_member: row.team_member.clone(),
        robots: row.robots.clone(),
        additional_info: row.additional_info.clone(),
        source_done: row.done,
        local_status: ImportedJobStatus::Active,
        row_hash: row_hash(row),
        source_payload: serde_json::to_value(row)
            .context("serializing normalized row payload")?,
        import_run_id: run_id.map(String::from),
    })
}

/// Map a normalization [`RowConflict`] to a persisted conflict input.
fn conflict_input_from_row_conflict(
    conflict: &RowConflict,
    source: &str,
    run_id: &str,
) -> ImportConflictInput {
    match conflict {
        RowConflict::BlankTitle { row_number } => ImportConflictInput {
            import_run_id: Some(run_id.to_string()),
            source: source.to_string(),
            external_job_key: String::new(),
            conflict_type: ImportConflictType::BlankTitle,
            reason: "eligible row has a blank title key; not imported".into(),
            details: serde_json::json!({ "row_number": row_number }),
        },
        RowConflict::DuplicateTitle { title, row_numbers } => {
            ImportConflictInput {
                import_run_id: Some(run_id.to_string()),
                source: source.to_string(),
                external_job_key: title.clone(),
                conflict_type: ImportConflictType::DuplicateTitle,
                reason:
                    "multiple eligible rows share the same title; none imported"
                        .into(),
                details: serde_json::json!({ "row_numbers": row_numbers }),
            }
        }
    }
}

// ── PmService entry points ───────────────────────────────────

/// Build the `import_runs` record metadata for a classified diff.
///
/// Shared by preview (dry-run) and apply so their counters, summary, and
/// `preview_hash` stay consistent.
fn import_run_input(
    snapshot: &ImportSnapshot,
    diff: &ImportDiff,
    run_id: &str,
    dry_run: bool,
    status: ImportRunStatus,
    triggered_by: &str,
) -> ImportRunInput {
    ImportRunInput {
        id: run_id.to_string(),
        source: snapshot.source.clone(),
        spreadsheet_id: snapshot.spreadsheet_id.clone(),
        sheet_name: snapshot.sheet_name.clone(),
        triggered_by: triggered_by.to_string(),
        dry_run,
        status,
        preview_hash: preview_hash(diff),
        seen_count: diff.seen as i64,
        created_count: diff.creates.len() as i64,
        updated_count: diff.updates.len() as i64,
        unchanged_count: diff.unchanged.len() as i64,
        conflict_count: (diff.conflicts.len() + diff.reopen_conflicts.len())
            as i64,
        invalid_count: diff.invalid.len() as i64,
        skipped_count: snapshot.skipped as i64,
        summary: serde_json::json!({
            "complete_from_source": diff.complete_from_source.len(),
            "missing_from_source": diff.missing_from_source.len(),
            "reopen_conflicts": diff.reopen_conflicts.len(),
            "done_titles": snapshot.done_titles.len(),
        }),
    }
}

impl PmService {
    /// Classify a snapshot against the currently stored imported jobs.
    ///
    /// Read-only: nothing is persisted (no jobs, no run records).
    pub fn preview_import(
        &self,
        snapshot: &ImportSnapshot,
    ) -> Result<ImportDiff> {
        let stored = self.store().list_imported_jobs(&snapshot.source)?;
        Ok(classify_import(
            &snapshot.rows,
            &snapshot.done_titles,
            &stored,
            &snapshot.conflicts,
            &snapshot.invalid,
        ))
    }

    /// Persist a dry-run preview run record for an already-classified diff.
    ///
    /// Only writes the `import_runs` bookkeeping row — imported jobs are never
    /// mutated by a preview. The returned run carries the deterministic
    /// `preview_hash` used by the apply tool to gate staleness.
    pub fn persist_preview(
        &self,
        snapshot: &ImportSnapshot,
        diff: &ImportDiff,
    ) -> Result<ImportRunRecord> {
        let run_id = Store::generate_id();
        let run = import_run_input(
            snapshot,
            diff,
            &run_id,
            true,
            ImportRunStatus::Previewed,
            "manual",
        );
        self.store().create_import_run(&run)
    }

    /// Classify and atomically apply an import snapshot.
    ///
    /// Creates/updates title-keyed jobs, marks Done rows completed locally,
    /// records normalization + reopen conflicts, and persists the import run —
    /// all in one SQLite transaction. Jobs missing from the source are never
    /// deleted or auto-completed.
    pub fn apply_import(
        &self,
        snapshot: &ImportSnapshot,
    ) -> Result<ImportRunRecord> {
        let stored = self.store().list_imported_jobs(&snapshot.source)?;
        let diff = classify_import(
            &snapshot.rows,
            &snapshot.done_titles,
            &stored,
            &snapshot.conflicts,
            &snapshot.invalid,
        );

        let run_id = Store::generate_id();
        let creates = diff
            .creates
            .iter()
            .map(|r| {
                job_input_from_row(
                    r,
                    &snapshot.source,
                    &snapshot.spreadsheet_id,
                    &snapshot.sheet_name,
                    Some(&run_id),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let updates = diff
            .updates
            .iter()
            .map(|r| {
                job_input_from_row(
                    r,
                    &snapshot.source,
                    &snapshot.spreadsheet_id,
                    &snapshot.sheet_name,
                    Some(&run_id),
                )
            })
            .collect::<Result<Vec<_>>>()?;

        let mut conflict_inputs: Vec<ImportConflictInput> = Vec::new();
        conflict_inputs.extend(diff.conflicts.iter().map(|c| {
            conflict_input_from_row_conflict(c, &snapshot.source, &run_id)
        }));
        conflict_inputs.extend(diff.reopen_conflicts.iter().map(|key| {
            ImportConflictInput {
                import_run_id: Some(run_id.clone()),
                source: snapshot.source.clone(),
                external_job_key: key.clone(),
                conflict_type: ImportConflictType::ReopenCompleted,
                reason:
                    "completed/ignored job reappeared as active; requires user review"
                        .into(),
                details: serde_json::json!({}),
            }
        }));
        // Invalid rows (unparseable dates etc.) are persisted as open
        // `InvalidRow` conflicts so operators can review them with
        // `pm_list_google_sheet_import_conflicts`.
        conflict_inputs.extend(diff.invalid.iter().map(|i| {
            ImportConflictInput {
                import_run_id: Some(run_id.clone()),
                source: snapshot.source.clone(),
                external_job_key: String::new(),
                conflict_type: ImportConflictType::InvalidRow,
                reason: format!(
                    "row {} could not be normalized: {}",
                    i.row_number, i.reason
                ),
                details: serde_json::json!({
                    "row_number": i.row_number,
                    "reason": i.reason,
                }),
            }
        }));

        let run = import_run_input(
            snapshot,
            &diff,
            &run_id,
            false,
            ImportRunStatus::Applied,
            "manual",
        );

        self.store().apply_import(
            &run,
            &creates,
            &updates,
            &diff.complete_from_source,
            &conflict_inputs,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm::schema;
    use marshaling_protocol::pm::ImportConflictStatus;
    use rusqlite::Connection;

    fn create_service() -> PmService {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        schema::bootstrap(&conn).expect("bootstrap");
        let store = Store::new(conn);
        PmService::new(store)
    }

    fn norm_row(title: &str, location: &str, leader: &str) -> NormalizedRow {
        NormalizedRow {
            row_number: 1,
            title: title.into(),
            location: location.into(),
            activity_type: "Boris Job".into(),
            start_date: Some("2026-08-05".into()),
            end_date: None,
            job_leader: leader.into(),
            team_member: String::new(),
            robots: "R1".into(),
            additional_info: String::new(),
            done: false,
        }
    }

    fn stored_job(
        key: &str,
        hash: &str,
        status: ImportedJobStatus,
    ) -> ImportedJobRecord {
        ImportedJobRecord {
            id: 1,
            entity_id: format!("entity-{key}"),
            source: "google_sheets".into(),
            spreadsheet_id: "spreadsheet-1".into(),
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
            source_done: status == ImportedJobStatus::Completed,
            local_status: status,
            row_hash: hash.into(),
            source_payload: serde_json::json!({}),
            first_seen_at: "2026-08-01T00:00:00Z".into(),
            last_seen_at: "2026-08-01T00:00:00Z".into(),
            last_import_run_id: None,
            created_at: "2026-08-01T00:00:00Z".into(),
            updated_at: "2026-08-01T00:00:00Z".into(),
        }
    }

    fn snapshot(rows: Vec<NormalizedRow>) -> ImportSnapshot {
        ImportSnapshot {
            source: "google_sheets".into(),
            spreadsheet_id: "spreadsheet-1".into(),
            sheet_name: "US".into(),
            rows,
            conflicts: vec![],
            invalid: vec![],
            done_titles: vec![],
            skipped: 0,
        }
    }

    fn job_input(
        key: &str,
        title: &str,
        location: &str,
        row_hash: &str,
    ) -> ImportedJobInput {
        ImportedJobInput {
            source: "google_sheets".into(),
            spreadsheet_id: "spreadsheet-1".into(),
            sheet_name: "US".into(),
            external_job_key: key.into(),
            title: title.into(),
            location: location.into(),
            activity_type: "Boris Job".into(),
            start_date: Some("2026-08-05".into()),
            end_date: None,
            job_leader: "Alice".into(),
            team_member: String::new(),
            robots: "R1".into(),
            additional_info: String::new(),
            source_done: false,
            local_status: ImportedJobStatus::Active,
            row_hash: row_hash.into(),
            source_payload: serde_json::json!({}),
            import_run_id: None,
        }
    }

    // ── Hashing ──

    #[test]
    fn test_row_hash_stable_and_sensitive() {
        let a = norm_row("Alpha", "loc", "Alice");
        let same = norm_row("Alpha", "loc", "Alice");
        let different = norm_row("Alpha", "loc", "Bob");
        assert_eq!(row_hash(&a), row_hash(&same));
        assert_ne!(row_hash(&a), row_hash(&different));
        assert_eq!(row_hash(&a).len(), 16);
    }

    #[test]
    fn test_preview_hash_stable_for_same_diff() {
        let rows = vec![norm_row("Alpha", "loc", "Alice")];
        let stored = vec![stored_job("Bravo", "h", ImportedJobStatus::Active)];
        let d1 = classify_import(&rows, &[], &stored, &[], &[]);
        let d2 = classify_import(&rows, &[], &stored, &[], &[]);
        assert_eq!(preview_hash(&d1), preview_hash(&d2));
        assert_eq!(preview_hash(&d1).len(), 16);
    }

    // ── Pure classification ──

    #[test]
    fn test_classify_create_update_unchanged() {
        let row_a = norm_row("Alpha", "loc1", "Alice"); // stored, unchanged
        let row_b = norm_row("Bravo", "loc1", "Alice"); // stored, changed leader
        let row_c = norm_row("Charlie", "loc1", "Alice"); // new
        let stored = vec![
            stored_job("Alpha", &row_hash(&row_a), ImportedJobStatus::Active),
            stored_job(
                "Bravo",
                &row_hash(&norm_row("Bravo", "loc1", "Bob")),
                ImportedJobStatus::Active,
            ),
        ];

        let diff = classify_import(
            &[row_a.clone(), row_b.clone(), row_c.clone()],
            &[],
            &stored,
            &[],
            &[],
        );

        assert_eq!(diff.creates, vec![row_c]);
        assert_eq!(diff.updates, vec![row_b]);
        assert_eq!(diff.unchanged, vec!["Alpha"]);
        assert!(diff.complete_from_source.is_empty());
        assert!(diff.missing_from_source.is_empty());
        assert!(diff.reopen_conflicts.is_empty());
        assert_eq!(diff.seen, 3);
    }

    #[test]
    fn test_classify_done_transition_marks_completed() {
        let active_row = norm_row("StillActive", "loc", "Alice");
        let stored = vec![
            stored_job(
                "Finished",
                &row_hash(&norm_row("Finished", "loc", "Alice")),
                ImportedJobStatus::Active,
            ),
            stored_job(
                "StillActive",
                &row_hash(&active_row),
                ImportedJobStatus::Active,
            ),
        ];

        let diff = classify_import(
            &[active_row],
            &["Finished".to_string()],
            &stored,
            &[],
            &[],
        );

        assert_eq!(diff.complete_from_source, vec!["Finished"]);
        assert!(diff.missing_from_source.is_empty());
        assert_eq!(diff.unchanged, vec!["StillActive"]);
    }

    #[test]
    fn test_classify_missing_reported_never_deleted() {
        let stored = vec![stored_job("Gone", "h", ImportedJobStatus::Active)];
        let diff = classify_import(&[], &[], &stored, &[], &[]);
        assert_eq!(diff.missing_from_source, vec!["Gone"]);
        assert!(diff.complete_from_source.is_empty());
        assert!(diff.creates.is_empty());
    }

    #[test]
    fn test_classify_reopen_completed_is_conflict_not_reopen() {
        let stored = vec![stored_job("Old", "h", ImportedJobStatus::Completed)];
        let diff = classify_import(
            &[norm_row("Old", "loc", "Alice")],
            &[],
            &stored,
            &[],
            &[],
        );
        assert_eq!(diff.reopen_conflicts, vec!["Old"]);
        assert!(diff.creates.is_empty());
        assert!(diff.updates.is_empty());
        assert!(diff.complete_from_source.is_empty());
    }

    #[test]
    fn test_classify_active_row_wins_over_done_same_read() {
        let row = norm_row("Both", "loc", "Alice");
        let stored = vec![stored_job(
            "Both",
            &row_hash(&row),
            ImportedJobStatus::Active,
        )];
        // The title is both active and done in the same read.
        let diff =
            classify_import(&[row], &["Both".to_string()], &stored, &[], &[]);
        assert_eq!(diff.unchanged, vec!["Both"]);
        assert!(diff.complete_from_source.is_empty());
    }

    #[test]
    fn test_classify_passes_through_conflicts_and_invalid() {
        let conflicts = vec![RowConflict::DuplicateTitle {
            title: "Shared".into(),
            row_numbers: vec![1, 2],
        }];
        let invalid = vec![InvalidRow {
            row_number: 3,
            reason: "Start Date: cannot parse".into(),
        }];
        let diff = classify_import(&[], &[], &[], &conflicts, &invalid);
        assert_eq!(diff.conflicts, conflicts);
        assert_eq!(diff.invalid, invalid);
    }

    // ── Preview / apply integration ──

    #[test]
    fn test_preview_import_is_read_only() {
        let svc = create_service();
        let snap = snapshot(vec![norm_row("Alpha", "loc", "Alice")]);
        let diff = svc.preview_import(&snap).unwrap();
        assert_eq!(diff.creates.len(), 1);

        // Nothing persisted.
        assert!(
            svc.store()
                .list_imported_jobs("google_sheets")
                .unwrap()
                .is_empty()
        );
        assert!(
            svc.store()
                .list_import_runs("google_sheets", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_apply_import_end_to_end() {
        let svc = create_service();
        let mut snap = snapshot(vec![
            norm_row("Alpha", "loc1", "Alice"),  // create
            norm_row("Bravo", "loc2", "Alice"),  // update (stored differs)
            norm_row("Charlie", "loc", "Alice"), // unchanged (stored matches)
        ]);
        snap.done_titles = vec!["DoneNow".into()];

        svc.store()
            .upsert_imported_job(&job_input(
                "Bravo",
                "Bravo",
                "old",
                &row_hash(&norm_row("Bravo", "old", "Alice")),
            ))
            .unwrap();
        svc.store()
            .upsert_imported_job(&job_input(
                "Charlie",
                "Charlie",
                "loc",
                &row_hash(&norm_row("Charlie", "loc", "Alice")),
            ))
            .unwrap();
        svc.store()
            .upsert_imported_job(&job_input("DoneNow", "DoneNow", "loc", "h"))
            .unwrap();
        svc.store()
            .upsert_imported_job(&job_input(
                "MissingJob",
                "MissingJob",
                "loc",
                "h",
            ))
            .unwrap();

        let run = svc.apply_import(&snap).unwrap();
        assert_eq!(run.status, ImportRunStatus::Applied);
        assert_eq!(run.seen_count, 3);
        assert_eq!(run.created_count, 1);
        assert_eq!(run.updated_count, 1);
        assert_eq!(run.unchanged_count, 1);
        assert_eq!(run.conflict_count, 0);
        assert_eq!(run.summary["complete_from_source"], 1);
        assert_eq!(run.summary["missing_from_source"], 1);

        // Created.
        let alpha = svc
            .store()
            .get_imported_job("google_sheets", "Alpha")
            .unwrap()
            .unwrap();
        assert_eq!(alpha.location, "loc1");
        assert_eq!(alpha.last_import_run_id.as_deref(), Some(run.id.as_str()));

        // Updated, stable identity preserved.
        let bravo = svc
            .store()
            .get_imported_job("google_sheets", "Bravo")
            .unwrap()
            .unwrap();
        assert_eq!(bravo.location, "loc2");

        // Completed from source.
        let done = svc
            .store()
            .get_imported_job("google_sheets", "DoneNow")
            .unwrap()
            .unwrap();
        assert!(done.source_done);
        assert_eq!(done.local_status, ImportedJobStatus::Completed);

        // Missing job not deleted and not completed.
        let missing = svc
            .store()
            .get_imported_job("google_sheets", "MissingJob")
            .unwrap()
            .unwrap();
        assert_eq!(missing.local_status, ImportedJobStatus::Active);

        // Run persisted.
        let fetched = svc.store().get_import_run(&run.id).unwrap().unwrap();
        assert_eq!(fetched.created_count, 1);
        assert_eq!(fetched.updated_count, 1);
    }

    #[test]
    fn test_apply_import_no_delete_when_missing() {
        let svc = create_service();
        svc.store()
            .upsert_imported_job(&job_input("Gone", "Gone", "loc", "h"))
            .unwrap();

        let snap = snapshot(vec![]);
        let run = svc.apply_import(&snap).unwrap();
        assert_eq!(run.created_count, 0);

        let gone = svc
            .store()
            .get_imported_job("google_sheets", "Gone")
            .unwrap()
            .unwrap();
        assert_eq!(gone.local_status, ImportedJobStatus::Active);
        assert!(!gone.source_done);
    }

    #[test]
    fn test_apply_import_records_duplicate_title_conflict() {
        let svc = create_service();
        let mut snap = snapshot(vec![norm_row("Alpha", "loc", "Alice")]);
        snap.conflicts = vec![RowConflict::DuplicateTitle {
            title: "Shared".into(),
            row_numbers: vec![1, 2],
        }];

        let run = svc.apply_import(&snap).unwrap();
        assert_eq!(run.conflict_count, 1);

        let open = svc
            .store()
            .list_import_conflicts("google_sheets", ImportConflictStatus::Open)
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].conflict_type, ImportConflictType::DuplicateTitle);
        assert_eq!(open[0].external_job_key, "Shared");
    }

    #[test]
    fn test_apply_import_records_invalid_row_conflict() {
        let svc = create_service();
        let mut snap = snapshot(vec![]);
        snap.invalid = vec![InvalidRow {
            row_number: 3,
            reason: "Start Date: cannot parse 'bad'".into(),
        }];

        let run = svc.apply_import(&snap).unwrap();
        assert_eq!(run.status, ImportRunStatus::Applied);
        assert_eq!(run.invalid_count, 1);

        // The invalid row is persisted as an open InvalidRow conflict so
        // pm_list_google_sheet_import_conflicts can surface it.
        let open = svc
            .store()
            .list_import_conflicts("google_sheets", ImportConflictStatus::Open)
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].conflict_type, ImportConflictType::InvalidRow);
        assert_eq!(open[0].external_job_key, "");
        assert_eq!(open[0].details["row_number"], 3);
        assert_eq!(open[0].details["reason"], "Start Date: cannot parse 'bad'");
        assert!(
            open[0].reason.contains("row 3"),
            "reason: {}",
            open[0].reason
        );
        assert_eq!(open[0].import_run_id.as_deref(), Some(run.id.as_str()));
    }
}
