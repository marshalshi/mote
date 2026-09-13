//! SQLite schema bootstrap / migrations.
//!
//! Uses a simple version-based migration scheme. The `schema_version` table
//! tracks the current schema version. `bootstrap()` runs any pending
//! migrations idempotently.

use anyhow::{Context, Result};

/// Current schema version. Bump when adding new migrations.
const SCHEMA_VERSION: i64 = 2;

/// Bootstrap (or migrate) the PM database schema.
///
/// Safe to call multiple times — uses `CREATE TABLE IF NOT EXISTS` and a
/// version check to avoid re-applying migrations.
pub fn bootstrap(conn: &rusqlite::Connection) -> Result<()> {
    // Ensure schema_version table exists for migration tracking
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )
    .context("creating schema_version table")?;

    // Check current version (0 = no migrations applied)
    let current_version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if current_version >= SCHEMA_VERSION {
        return Ok(()); // already up to date
    }

    // Run all migrations from current_version+1 .. SCHEMA_VERSION
    for version in (current_version + 1)..=SCHEMA_VERSION {
        run_migration(conn, version)?;
    }

    // Record the new version
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        rusqlite::params![SCHEMA_VERSION],
    )
    .context("recording schema version")?;

    Ok(())
}

/// Run a single migration by version number.
fn run_migration(conn: &rusqlite::Connection, version: i64) -> Result<()> {
    match version {
        1 => migration_001_initial(conn)?,
        2 => migration_002_google_sheets_imports(conn)?,
        _ => anyhow::bail!("Unknown schema version: {}", version),
    }
    Ok(())
}

/// Initial schema: all tables for the PM MVP entities.
fn migration_001_initial(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "
        -- Sites: deployment locations
        CREATE TABLE IF NOT EXISTS sites (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id       TEXT NOT NULL UNIQUE,
            name            TEXT NOT NULL,
            location        TEXT NOT NULL DEFAULT '',
            deploy_window_start TEXT,
            deploy_window_end   TEXT,
            onsite_duration_days INTEGER NOT NULL DEFAULT 0,
            files_confirmed INTEGER NOT NULL DEFAULT 0,
            notes           TEXT NOT NULL DEFAULT '',
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Batches: deployment batches with target site and dates
        CREATE TABLE IF NOT EXISTS batches (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id           TEXT NOT NULL UNIQUE,
            name                TEXT NOT NULL,
            robot_serials       TEXT NOT NULL DEFAULT '[]',
            target_site_id      INTEGER REFERENCES sites(id),
            target_deploy_date  TEXT,
            onsite_duration_days INTEGER NOT NULL DEFAULT 0,
            status              TEXT NOT NULL DEFAULT 'planned',
            notes               TEXT NOT NULL DEFAULT '',
            created_at          TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- People: team members with skills and availability
        CREATE TABLE IF NOT EXISTS people (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id         TEXT NOT NULL UNIQUE,
            name              TEXT NOT NULL,
            is_senior         INTEGER NOT NULL DEFAULT 0,
            skills            TEXT NOT NULL DEFAULT '[]',
            special_sites     TEXT NOT NULL DEFAULT '[]',
            availability_start TEXT,
            availability_end   TEXT,
            notes             TEXT NOT NULL DEFAULT '',
            created_at        TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Assignments: person-to-site/batch assignments with handover chain
        CREATE TABLE IF NOT EXISTS assignments (
            id                    INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id             TEXT NOT NULL UNIQUE,
            person_id             INTEGER NOT NULL REFERENCES people(id),
            site_id               INTEGER REFERENCES sites(id),
            batch_id              INTEGER REFERENCES batches(id),
            role                  TEXT NOT NULL DEFAULT '',
            start_date            TEXT,
            end_date              TEXT,
            handover_to_person_id INTEGER REFERENCES people(id),
            created_at            TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at            TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Inventory items: tracked supplies and equipment
        CREATE TABLE IF NOT EXISTS inventory_items (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id        TEXT NOT NULL UNIQUE,
            sku              TEXT NOT NULL DEFAULT '',
            name             TEXT NOT NULL,
            category         TEXT NOT NULL DEFAULT '',
            qty_on_hand      INTEGER NOT NULL DEFAULT 0,
            qty_reserved     INTEGER NOT NULL DEFAULT 0,
            location_area    TEXT NOT NULL DEFAULT '',
            location_rack    TEXT NOT NULL DEFAULT '',
            supplier         TEXT NOT NULL DEFAULT '',
            lead_time_days   INTEGER NOT NULL DEFAULT 0,
            reorder_threshold INTEGER NOT NULL DEFAULT 0,
            status           TEXT NOT NULL DEFAULT 'available',
            linked_batch_id  INTEGER REFERENCES batches(id),
            created_at       TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Procurement orders: purchase orders for inventory items
        CREATE TABLE IF NOT EXISTS procurement_orders (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id        TEXT NOT NULL UNIQUE,
            order_ref        TEXT NOT NULL DEFAULT '',
            item_sku         TEXT NOT NULL DEFAULT '',
            item_name        TEXT NOT NULL DEFAULT '',
            supplier         TEXT NOT NULL DEFAULT '',
            qty              INTEGER NOT NULL DEFAULT 0,
            lead_time_days   INTEGER NOT NULL DEFAULT 0,
            order_date       TEXT,
            eta              TEXT,
            status           TEXT NOT NULL DEFAULT 'pending',
            linked_batch_id  INTEGER REFERENCES batches(id),
            notes            TEXT NOT NULL DEFAULT '',
            created_at       TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Alerts: actionable notifications for various conditions
        CREATE TABLE IF NOT EXISTS alerts (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id           TEXT NOT NULL UNIQUE,
            alert_type          TEXT NOT NULL DEFAULT '',
            severity            TEXT NOT NULL DEFAULT 'ok',
            related_entity_type TEXT NOT NULL DEFAULT '',
            related_entity_id   TEXT NOT NULL DEFAULT '',
            due_date            TEXT,
            reason              TEXT NOT NULL DEFAULT '',
            suggested_action    TEXT NOT NULL DEFAULT '',
            resolved            INTEGER NOT NULL DEFAULT 0,
            created_at          TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Event logs: durable audit trail for all write operations
        CREATE TABLE IF NOT EXISTS event_logs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp   TEXT NOT NULL,
            source      TEXT NOT NULL,
            action_type TEXT NOT NULL,
            entity_type TEXT NOT NULL,
            entity_id   TEXT NOT NULL,
            summary     TEXT NOT NULL DEFAULT '',
            created_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Indexes for common queries
        CREATE INDEX IF NOT EXISTS idx_batches_site ON batches(target_site_id);
        CREATE INDEX IF NOT EXISTS idx_batches_status ON batches(status);
        CREATE INDEX IF NOT EXISTS idx_inventory_status ON inventory_items(status);
        CREATE INDEX IF NOT EXISTS idx_inventory_reorder ON inventory_items(qty_on_hand, reorder_threshold);
        CREATE INDEX IF NOT EXISTS idx_procurement_status ON procurement_orders(status);
        CREATE INDEX IF NOT EXISTS idx_alerts_severity ON alerts(severity);
        CREATE INDEX IF NOT EXISTS idx_alerts_resolved ON alerts(resolved);
        CREATE INDEX IF NOT EXISTS idx_event_logs_timestamp ON event_logs(timestamp);
        CREATE INDEX IF NOT EXISTS idx_assignments_person ON assignments(person_id);
        CREATE INDEX IF NOT EXISTS idx_assignments_site ON assignments(site_id);
        ",
    )
    .context("running initial schema migration")?;

    Ok(())
}

/// Migration v2: title-keyed Google Sheets job imports.
///
/// Adds the manual preview/apply store for the Google Sheets sync:
/// - `imported_jobs` — one row per exact `Title of Activity` (keyed per
///   `source`), with all mapped source fields, done/local lifecycle state,
///   a stable row hash for change detection, and source provenance.
/// - `import_runs` — one row per preview or apply, with preview hash, status,
///   and per-class counts.
/// - `import_conflicts` — open/resolved conflicts surfaced for operator
///   review (blank/duplicate titles, completed jobs reopening, ...).
fn migration_002_google_sheets_imports(
    conn: &rusqlite::Connection,
) -> Result<()> {
    conn.execute_batch(
        "
        -- Imported Google Sheets jobs, keyed by exact title per source.
        CREATE TABLE IF NOT EXISTS imported_jobs (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id           TEXT NOT NULL UNIQUE,
            source              TEXT NOT NULL,
            spreadsheet_id      TEXT NOT NULL DEFAULT '',
            sheet_name          TEXT NOT NULL DEFAULT '',
            external_job_key    TEXT NOT NULL,
            title               TEXT NOT NULL,
            location            TEXT NOT NULL DEFAULT '',
            activity_type       TEXT NOT NULL DEFAULT '',
            start_date          TEXT,
            end_date            TEXT,
            job_leader          TEXT NOT NULL DEFAULT '',
            team_member         TEXT NOT NULL DEFAULT '',
            robots              TEXT NOT NULL DEFAULT '',
            additional_info     TEXT NOT NULL DEFAULT '',
            source_done         INTEGER NOT NULL DEFAULT 0,
            local_status        TEXT NOT NULL DEFAULT 'active',
            row_hash            TEXT NOT NULL DEFAULT '',
            source_payload      TEXT NOT NULL DEFAULT '{}',
            first_seen_at       TEXT NOT NULL,
            last_seen_at        TEXT NOT NULL,
            last_import_run_id  TEXT,
            created_at          TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at          TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE (source, external_job_key)
        );
        CREATE INDEX IF NOT EXISTS idx_imported_jobs_source_status ON imported_jobs(source, local_status);
        CREATE INDEX IF NOT EXISTS idx_imported_jobs_source_key ON imported_jobs(source, external_job_key);

        -- Google Sheets import runs (preview or apply bookkeeping).
        CREATE TABLE IF NOT EXISTS import_runs (
            id              TEXT PRIMARY KEY,
            source          TEXT NOT NULL,
            spreadsheet_id  TEXT NOT NULL DEFAULT '',
            sheet_name      TEXT NOT NULL DEFAULT '',
            triggered_by    TEXT NOT NULL DEFAULT 'manual',
            dry_run         INTEGER NOT NULL DEFAULT 0,
            status          TEXT NOT NULL DEFAULT 'previewed',
            preview_hash    TEXT NOT NULL DEFAULT '',
            seen_count      INTEGER NOT NULL DEFAULT 0,
            created_count   INTEGER NOT NULL DEFAULT 0,
            updated_count   INTEGER NOT NULL DEFAULT 0,
            unchanged_count INTEGER NOT NULL DEFAULT 0,
            conflict_count  INTEGER NOT NULL DEFAULT 0,
            invalid_count   INTEGER NOT NULL DEFAULT 0,
            skipped_count   INTEGER NOT NULL DEFAULT 0,
            summary         TEXT NOT NULL DEFAULT '{}',
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            completed_at    TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_import_runs_source ON import_runs(source);

        -- Google Sheets import conflicts (operator review).
        CREATE TABLE IF NOT EXISTS import_conflicts (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            import_run_id   TEXT,
            source          TEXT NOT NULL,
            external_job_key TEXT NOT NULL DEFAULT '',
            conflict_type   TEXT NOT NULL DEFAULT '',
            reason          TEXT NOT NULL DEFAULT '',
            details         TEXT NOT NULL DEFAULT '{}',
            status          TEXT NOT NULL DEFAULT 'open',
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            resolved_at     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_import_conflicts_status ON import_conflicts(status);
        CREATE INDEX IF NOT EXISTS idx_import_conflicts_source ON import_conflicts(source);
        ",
    )
    .context("running google sheets imports migration")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_temp_db() -> rusqlite::Result<rusqlite::Connection> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(conn)
    }

    #[test]
    fn test_bootstrap_creates_tables() {
        let conn = create_temp_db().expect("temp db");
        bootstrap(&conn).expect("bootstrap should succeed");

        // Verify all tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        for table in &[
            "schema_version",
            "sites",
            "batches",
            "people",
            "assignments",
            "inventory_items",
            "procurement_orders",
            "alerts",
            "event_logs",
            "imported_jobs",
            "import_runs",
            "import_conflicts",
        ] {
            assert!(
                tables.contains(&table.to_string()),
                "missing table: {}",
                table
            );
        }
    }

    #[test]
    fn test_bootstrap_is_idempotent() {
        let conn = create_temp_db().expect("temp db");
        bootstrap(&conn).expect("first bootstrap");
        bootstrap(&conn).expect("second bootstrap should succeed");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1, "schema_version should have exactly one row");
    }

    #[test]
    fn test_schema_version_tracked() {
        let conn = create_temp_db().expect("temp db");
        bootstrap(&conn).expect("bootstrap");

        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn test_bootstrap_migrates_v1_to_v2() {
        let conn = create_temp_db().expect("temp db");
        // Simulate a v1 database: create the version table, apply only the
        // initial migration, and record version 1 (as the old bootstrap
        // would have done).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .expect("create schema_version");
        run_migration(&conn, 1).expect("migration 1");
        conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])
            .expect("record v1");

        let before: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(before, 1);

        // A later bootstrap must run only the pending migration (v2).
        bootstrap(&conn).expect("bootstrap upgrades v1 -> v2");

        let after: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(after, SCHEMA_VERSION);

        // v2 tables must exist with the expected unique constraint.
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('imported_jobs', 'import_runs', 'import_conflicts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 3);

        // The title key uniqueness must be enforceable.
        conn.execute(
            "INSERT INTO imported_jobs (entity_id, source, external_job_key, title, first_seen_at, last_seen_at)
             VALUES ('e1', 'google_sheets', 'Same title', 'Same title', '2026-01-01', '2026-01-01')",
            [],
        )
        .expect("insert job");
        let dup = conn.execute(
            "INSERT INTO imported_jobs (entity_id, source, external_job_key, title, first_seen_at, last_seen_at)
             VALUES ('e2', 'google_sheets', 'Same title', 'Same title', '2026-01-01', '2026-01-01')",
            [],
        );
        assert!(
            dup.is_err(),
            "duplicate (source, external_job_key) must fail"
        );
    }
}
