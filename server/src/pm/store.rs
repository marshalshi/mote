//! SQLite persistence layer for PM entities.
//!
//! `Store` wraps an `Arc<Mutex<rusqlite::Connection>>` for thread-safe access
//! in an async context. All CRUD operations acquire the mutex, run the query,
//! and return typed records.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use uuid::Uuid;

use marshaling_protocol::pm::{
    AlertRecord, AssignmentRecord, BatchRecord, EventLogRecord,
    ImportConflictInput, ImportConflictRecord, ImportConflictStatus,
    ImportConflictType, ImportRunInput, ImportRunRecord, ImportRunStatus,
    ImportedJobInput, ImportedJobRecord, ImportedJobStatus,
    InventoryItemRecord, PersonRecord, ProcurementOrderRecord, SiteRecord,
};

/// Thread-safe SQLite store for PM entities.
#[derive(Debug, Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Create a new Store wrapping an existing SQLite connection.
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Return a new v4 UUID string for use as a stable entity ID.
    pub fn generate_id() -> String {
        Uuid::new_v4().to_string()
    }

    /// Return the current UTC timestamp as an ISO-8601 string.
    fn now() -> String {
        Utc::now().to_rfc3339()
    }
}

// ── Helpers ─────────────────────────────────────────────────

/// Serialize a Vec<String> to a JSON string for SQLite storage.
fn vec_to_json(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

/// Deserialize a JSON string from SQLite into a Vec<String>.
fn vec_from_json(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

/// Convert a bool to an i32 for SQLite storage (0 or 1).
fn bool_to_int(b: bool) -> i32 {
    if b { 1 } else { 0 }
}

/// Convert an i32 from SQLite to a bool.
fn int_to_bool(i: i32) -> bool {
    i != 0
}

// ── Site CRUD ───────────────────────────────────────────────

impl Store {
    /// Insert a new site, generating a UUID entity_id.
    pub fn insert_site(
        &self,
        name: &str,
        location: &str,
        deploy_window_start: Option<&str>,
        deploy_window_end: Option<&str>,
        onsite_duration_days: i32,
        files_confirmed: bool,
        notes: &str,
    ) -> Result<SiteRecord> {
        let entity_id = Self::generate_id();
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sites (entity_id, name, location, deploy_window_start, deploy_window_end, onsite_duration_days, files_confirmed, notes, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![entity_id, name, location, deploy_window_start, deploy_window_end, onsite_duration_days, bool_to_int(files_confirmed), notes, now, now],
        ).context("inserting site")?;
        let id = conn.last_insert_rowid();
        Ok(SiteRecord {
            id,
            entity_id,
            name: name.to_string(),
            location: location.to_string(),
            deploy_window_start: deploy_window_start.map(String::from),
            deploy_window_end: deploy_window_end.map(String::from),
            onsite_duration_days,
            files_confirmed,
            notes: notes.to_string(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Update an existing site by entity_id. Only non-None fields are applied.
    pub fn update_site(
        &self,
        entity_id: &str,
        name: Option<&str>,
        location: Option<&str>,
        deploy_window_start: Option<&str>,
        deploy_window_end: Option<&str>,
        onsite_duration_days: Option<i32>,
        files_confirmed: Option<bool>,
        notes: Option<&str>,
    ) -> Result<Option<SiteRecord>> {
        let conn = self.conn.lock().unwrap();
        // Check existence
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sites WHERE entity_id = ?1",
                params![entity_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)?;
        if !exists {
            return Ok(None);
        }

        let now = Self::now();
        if let Some(v) = name {
            conn.execute("UPDATE sites SET name = ?1, updated_at = ?2 WHERE entity_id = ?3",
                params![v, now, entity_id])?;
        }
        if let Some(v) = location {
            conn.execute("UPDATE sites SET location = ?1, updated_at = ?2 WHERE entity_id = ?3",
                params![v, now, entity_id])?;
        }
        if let Some(v) = deploy_window_start {
            conn.execute("UPDATE sites SET deploy_window_start = ?1, updated_at = ?2 WHERE entity_id = ?3",
                params![v, now, entity_id])?;
        }
        if let Some(v) = deploy_window_end {
            conn.execute("UPDATE sites SET deploy_window_end = ?1, updated_at = ?2 WHERE entity_id = ?3",
                params![v, now, entity_id])?;
        }
        if let Some(v) = onsite_duration_days {
            conn.execute("UPDATE sites SET onsite_duration_days = ?1, updated_at = ?2 WHERE entity_id = ?3",
                params![v, now, entity_id])?;
        }
        if let Some(v) = files_confirmed {
            conn.execute("UPDATE sites SET files_confirmed = ?1, updated_at = ?2 WHERE entity_id = ?3",
                params![bool_to_int(v), now, entity_id])?;
        }
        if let Some(v) = notes {
            conn.execute("UPDATE sites SET notes = ?1, updated_at = ?2 WHERE entity_id = ?3",
                params![v, now, entity_id])?;
        }

        // Fetch updated record
        let rec = conn.query_row(
            "SELECT id, entity_id, name, location, deploy_window_start, deploy_window_end, onsite_duration_days, files_confirmed, notes, created_at, updated_at FROM sites WHERE entity_id = ?1",
            params![entity_id],
            |row| {
                Ok(SiteRecord {
                    id: row.get(0)?,
                    entity_id: row.get(1)?,
                    name: row.get(2)?,
                    location: row.get(3)?,
                    deploy_window_start: row.get(4)?,
                    deploy_window_end: row.get(5)?,
                    onsite_duration_days: row.get(6)?,
                    files_confirmed: int_to_bool(row.get::<_, i32>(7)?),
                    notes: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            },
        )?;
        Ok(Some(rec))
    }

    /// Get a site by its entity_id (UUID).
    pub fn get_site(&self, entity_id: &str) -> Result<Option<SiteRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, location, deploy_window_start, deploy_window_end, onsite_duration_days, files_confirmed, notes, created_at, updated_at FROM sites WHERE entity_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![entity_id], |row| {
            Ok(SiteRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                location: row.get(3)?,
                deploy_window_start: row.get(4)?,
                deploy_window_end: row.get(5)?,
                onsite_duration_days: row.get(6)?,
                files_confirmed: int_to_bool(row.get::<_, i32>(7)?),
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Get a site by name (exact match).
    #[allow(dead_code)]
    pub fn get_site_by_name(&self, name: &str) -> Result<Option<SiteRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, location, deploy_window_start, deploy_window_end, onsite_duration_days, files_confirmed, notes, created_at, updated_at FROM sites WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], |row| {
            Ok(SiteRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                location: row.get(3)?,
                deploy_window_start: row.get(4)?,
                deploy_window_end: row.get(5)?,
                onsite_duration_days: row.get(6)?,
                files_confirmed: int_to_bool(row.get::<_, i32>(7)?),
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Get a site by its internal integer ID.
    #[allow(dead_code)]
    pub fn get_site_by_id(&self, id: i64) -> Result<Option<SiteRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, location, deploy_window_start, deploy_window_end, onsite_duration_days, files_confirmed, notes, created_at, updated_at FROM sites WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(SiteRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                location: row.get(3)?,
                deploy_window_start: row.get(4)?,
                deploy_window_end: row.get(5)?,
                onsite_duration_days: row.get(6)?,
                files_confirmed: int_to_bool(row.get::<_, i32>(7)?),
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// List all sites.
    pub fn list_sites(&self) -> Result<Vec<SiteRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, location, deploy_window_start, deploy_window_end, onsite_duration_days, files_confirmed, notes, created_at, updated_at FROM sites ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SiteRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                location: row.get(3)?,
                deploy_window_start: row.get(4)?,
                deploy_window_end: row.get(5)?,
                onsite_duration_days: row.get(6)?,
                files_confirmed: int_to_bool(row.get::<_, i32>(7)?),
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get a site by name, or create one if it does not exist.
    /// This is useful for tools that reference sites by name.
    #[allow(dead_code)]
    pub fn get_or_create_site(&self, name: &str) -> Result<SiteRecord> {
        let conn = self.conn.lock().unwrap();
        // Try to find by name
        let existing = conn.query_row(
            "SELECT id, entity_id, name, location, deploy_window_start, deploy_window_end, onsite_duration_days, files_confirmed, notes, created_at, updated_at FROM sites WHERE name = ?1",
            params![name],
            |row| {
                Ok(SiteRecord {
                    id: row.get(0)?,
                    entity_id: row.get(1)?,
                    name: row.get(2)?,
                    location: row.get(3)?,
                    deploy_window_start: row.get(4)?,
                    deploy_window_end: row.get(5)?,
                    onsite_duration_days: row.get(6)?,
                    files_confirmed: int_to_bool(row.get::<_, i32>(7)?),
                    notes: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            },
        );
        match existing {
            Ok(rec) => Ok(rec),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                drop(existing); // release borrow on conn
                drop(conn);
                // Insert new site
                self.insert_site(name, "", None, None, 0, false, "")
            }
            Err(e) => Err(e.into()),
        }
    }
}

// ── Batch CRUD ──────────────────────────────────────────────

impl Store {
    /// Insert a new batch, generating a UUID entity_id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_batch(
        &self,
        name: &str,
        robot_serials: &[String],
        target_site_id: Option<i64>,
        target_deploy_date: Option<&str>,
        onsite_duration_days: i32,
        status: &str,
        notes: &str,
    ) -> Result<BatchRecord> {
        let entity_id = Self::generate_id();
        let now = Self::now();
        let serials_json = vec_to_json(robot_serials);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO batches (entity_id, name, robot_serials, target_site_id, target_deploy_date, onsite_duration_days, status, notes, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![entity_id, name, serials_json, target_site_id, target_deploy_date, onsite_duration_days, status, notes, now, now],
        ).context("inserting batch")?;
        let id = conn.last_insert_rowid();
        Ok(BatchRecord {
            id,
            entity_id,
            name: name.to_string(),
            robot_serials: robot_serials.to_vec(),
            target_site_id,
            target_deploy_date: target_deploy_date.map(String::from),
            onsite_duration_days,
            status: status.to_string(),
            notes: notes.to_string(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Update an existing batch by entity_id.
    pub fn update_batch(
        &self,
        entity_id: &str,
        name: Option<&str>,
        robot_serials: Option<&[String]>,
        target_site_id: Option<Option<i64>>,
        target_deploy_date: Option<&str>,
        onsite_duration_days: Option<i32>,
        status: Option<&str>,
        notes: Option<&str>,
    ) -> Result<Option<BatchRecord>> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM batches WHERE entity_id = ?1",
                params![entity_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)?;
        if !exists {
            return Ok(None);
        }

        let now = Self::now();
        if let Some(v) = name {
            conn.execute("UPDATE batches SET name = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = robot_serials {
            conn.execute("UPDATE batches SET robot_serials = ?1, updated_at = ?2 WHERE entity_id = ?3", params![vec_to_json(v), now, entity_id])?;
        }
        if let Some(v) = target_site_id {
            conn.execute("UPDATE batches SET target_site_id = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = target_deploy_date {
            conn.execute("UPDATE batches SET target_deploy_date = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = onsite_duration_days {
            conn.execute("UPDATE batches SET onsite_duration_days = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = status {
            conn.execute("UPDATE batches SET status = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = notes {
            conn.execute("UPDATE batches SET notes = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }

        let rec = conn.query_row(
            "SELECT id, entity_id, name, robot_serials, target_site_id, target_deploy_date, onsite_duration_days, status, notes, created_at, updated_at FROM batches WHERE entity_id = ?1",
            params![entity_id],
            |row| {
                let serials_str: String = row.get(3)?;
                Ok(BatchRecord {
                    id: row.get(0)?,
                    entity_id: row.get(1)?,
                    name: row.get(2)?,
                    robot_serials: vec_from_json(&serials_str),
                    target_site_id: row.get(4)?,
                    target_deploy_date: row.get(5)?,
                    onsite_duration_days: row.get(6)?,
                    status: row.get(7)?,
                    notes: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            },
        )?;
        Ok(Some(rec))
    }

    /// Get a batch by entity_id.
    pub fn get_batch(&self, entity_id: &str) -> Result<Option<BatchRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, robot_serials, target_site_id, target_deploy_date, onsite_duration_days, status, notes, created_at, updated_at FROM batches WHERE entity_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![entity_id], |row| {
            let serials_str: String = row.get(3)?;
            Ok(BatchRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                robot_serials: vec_from_json(&serials_str),
                target_site_id: row.get(4)?,
                target_deploy_date: row.get(5)?,
                onsite_duration_days: row.get(6)?,
                status: row.get(7)?,
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Get a batch by internal ID.
    #[allow(dead_code)]
    pub fn get_batch_by_id(&self, id: i64) -> Result<Option<BatchRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, robot_serials, target_site_id, target_deploy_date, onsite_duration_days, status, notes, created_at, updated_at FROM batches WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            let serials_str: String = row.get(3)?;
            Ok(BatchRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                robot_serials: vec_from_json(&serials_str),
                target_site_id: row.get(4)?,
                target_deploy_date: row.get(5)?,
                onsite_duration_days: row.get(6)?,
                status: row.get(7)?,
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// List all batches.
    pub fn list_batches(&self) -> Result<Vec<BatchRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, robot_serials, target_site_id, target_deploy_date, onsite_duration_days, status, notes, created_at, updated_at FROM batches ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            let serials_str: String = row.get(3)?;
            Ok(BatchRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                robot_serials: vec_from_json(&serials_str),
                target_site_id: row.get(4)?,
                target_deploy_date: row.get(5)?,
                onsite_duration_days: row.get(6)?,
                status: row.get(7)?,
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }
}

// ── Person CRUD ─────────────────────────────────────────────

#[allow(dead_code)]
impl Store {
    /// Insert a new person, generating a UUID entity_id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_person(
        &self,
        name: &str,
        is_senior: bool,
        skills: &[String],
        special_sites: &[String],
        availability_start: Option<&str>,
        availability_end: Option<&str>,
        notes: &str,
    ) -> Result<PersonRecord> {
        let entity_id = Self::generate_id();
        let now = Self::now();
        let skills_json = vec_to_json(skills);
        let special_sites_json = vec_to_json(special_sites);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO people (entity_id, name, is_senior, skills, special_sites, availability_start, availability_end, notes, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![entity_id, name, bool_to_int(is_senior), skills_json, special_sites_json, availability_start, availability_end, notes, now, now],
        ).context("inserting person")?;
        let id = conn.last_insert_rowid();
        Ok(PersonRecord {
            id,
            entity_id,
            name: name.to_string(),
            is_senior,
            skills: skills.to_vec(),
            special_sites: special_sites.to_vec(),
            availability_start: availability_start.map(String::from),
            availability_end: availability_end.map(String::from),
            notes: notes.to_string(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Update an existing person by entity_id.
    pub fn update_person(
        &self,
        entity_id: &str,
        name: Option<&str>,
        is_senior: Option<bool>,
        skills: Option<&[String]>,
        special_sites: Option<&[String]>,
        availability_start: Option<&str>,
        availability_end: Option<&str>,
        notes: Option<&str>,
    ) -> Result<Option<PersonRecord>> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM people WHERE entity_id = ?1",
                params![entity_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)?;
        if !exists {
            return Ok(None);
        }

        let now = Self::now();
        if let Some(v) = name {
            conn.execute("UPDATE people SET name = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = is_senior {
            conn.execute("UPDATE people SET is_senior = ?1, updated_at = ?2 WHERE entity_id = ?3", params![bool_to_int(v), now, entity_id])?;
        }
        if let Some(v) = skills {
            conn.execute("UPDATE people SET skills = ?1, updated_at = ?2 WHERE entity_id = ?3", params![vec_to_json(v), now, entity_id])?;
        }
        if let Some(v) = special_sites {
            conn.execute("UPDATE people SET special_sites = ?1, updated_at = ?2 WHERE entity_id = ?3", params![vec_to_json(v), now, entity_id])?;
        }
        if let Some(v) = availability_start {
            conn.execute("UPDATE people SET availability_start = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = availability_end {
            conn.execute("UPDATE people SET availability_end = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = notes {
            conn.execute("UPDATE people SET notes = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }

        let rec = conn.query_row(
            "SELECT id, entity_id, name, is_senior, skills, special_sites, availability_start, availability_end, notes, created_at, updated_at FROM people WHERE entity_id = ?1",
            params![entity_id],
            |row| {
                let skills_str: String = row.get(4)?;
                let special_sites_str: String = row.get(5)?;
                Ok(PersonRecord {
                    id: row.get(0)?,
                    entity_id: row.get(1)?,
                    name: row.get(2)?,
                    is_senior: int_to_bool(row.get::<_, i32>(3)?),
                    skills: vec_from_json(&skills_str),
                    special_sites: vec_from_json(&special_sites_str),
                    availability_start: row.get(6)?,
                    availability_end: row.get(7)?,
                    notes: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            },
        )?;
        Ok(Some(rec))
    }

    /// Get a person by entity_id.
    pub fn get_person(&self, entity_id: &str) -> Result<Option<PersonRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, is_senior, skills, special_sites, availability_start, availability_end, notes, created_at, updated_at FROM people WHERE entity_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![entity_id], |row| {
            let skills_str: String = row.get(4)?;
            let special_sites_str: String = row.get(5)?;
            Ok(PersonRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                is_senior: int_to_bool(row.get::<_, i32>(3)?),
                skills: vec_from_json(&skills_str),
                special_sites: vec_from_json(&special_sites_str),
                availability_start: row.get(6)?,
                availability_end: row.get(7)?,
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Get a person by internal ID.
    #[allow(dead_code)]
    pub fn get_person_by_id(&self, id: i64) -> Result<Option<PersonRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, is_senior, skills, special_sites, availability_start, availability_end, notes, created_at, updated_at FROM people WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            let skills_str: String = row.get(4)?;
            let special_sites_str: String = row.get(5)?;
            Ok(PersonRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                is_senior: int_to_bool(row.get::<_, i32>(3)?),
                skills: vec_from_json(&skills_str),
                special_sites: vec_from_json(&special_sites_str),
                availability_start: row.get(6)?,
                availability_end: row.get(7)?,
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// List all people.
    pub fn list_people(&self) -> Result<Vec<PersonRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, name, is_senior, skills, special_sites, availability_start, availability_end, notes, created_at, updated_at FROM people ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            let skills_str: String = row.get(4)?;
            let special_sites_str: String = row.get(5)?;
            Ok(PersonRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                name: row.get(2)?,
                is_senior: int_to_bool(row.get::<_, i32>(3)?),
                skills: vec_from_json(&skills_str),
                special_sites: vec_from_json(&special_sites_str),
                availability_start: row.get(6)?,
                availability_end: row.get(7)?,
                notes: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }
}

// ── InventoryItem CRUD ──────────────────────────────────────

impl Store {
    /// Upsert inventory by name. If an item with the given name exists, update
    /// its fields; otherwise create a new item.
    pub fn upsert_inventory(
        &self,
        name: &str,
        sku: Option<&str>,
        category: Option<&str>,
        qty_on_hand: Option<i32>,
        location_area: Option<&str>,
        location_rack: Option<&str>,
        supplier: Option<&str>,
        lead_time_days: Option<i32>,
        reorder_threshold: Option<i32>,
        linked_batch_id: Option<Option<i64>>,
    ) -> Result<InventoryItemRecord> {
        let conn = self.conn.lock().unwrap();

        // Check if item with this name exists
        let existing = conn.query_row(
            "SELECT id FROM inventory_items WHERE name = ?1",
            params![name],
            |row| row.get::<_, i64>(0),
        );

        let now = Self::now();

        match existing {
            Ok(existing_id) => {
                // Update existing
                if let Some(v) = sku {
                    conn.execute("UPDATE inventory_items SET sku = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
                }
                if let Some(v) = category {
                    conn.execute("UPDATE inventory_items SET category = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
                }
                if let Some(v) = qty_on_hand {
                    conn.execute("UPDATE inventory_items SET qty_on_hand = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
                }
                if let Some(v) = location_area {
                    conn.execute("UPDATE inventory_items SET location_area = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
                }
                if let Some(v) = location_rack {
                    conn.execute("UPDATE inventory_items SET location_rack = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
                }
                if let Some(v) = supplier {
                    conn.execute("UPDATE inventory_items SET supplier = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
                }
                if let Some(v) = lead_time_days {
                    conn.execute("UPDATE inventory_items SET lead_time_days = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
                }
                if let Some(v) = reorder_threshold {
                    conn.execute("UPDATE inventory_items SET reorder_threshold = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
                }
                if let Some(v) = linked_batch_id {
                    conn.execute("UPDATE inventory_items SET linked_batch_id = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
                }
                // Update status based on qty_on_hand and reorder_threshold
                conn.execute(
                    "UPDATE inventory_items SET status = CASE
                        WHEN qty_on_hand <= 0 THEN 'out_of_stock'
                        WHEN reorder_threshold > 0 AND qty_on_hand <= reorder_threshold THEN 'low_stock'
                        ELSE 'available'
                    END, updated_at = ?1 WHERE id = ?2",
                    params![now, existing_id],
                )?;

                drop(now);
                self.get_inventory_by_id_internal(&conn, existing_id)
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // Create new
                let entity_id = Self::generate_id();
                let sku = sku.unwrap_or("");
                let category = category.unwrap_or("");
                let qty = qty_on_hand.unwrap_or(0);
                let loc_area = location_area.unwrap_or("");
                let loc_rack = location_rack.unwrap_or("");
                let supp = supplier.unwrap_or("");
                let lead = lead_time_days.unwrap_or(0);
                let reorder = reorder_threshold.unwrap_or(0);
                let batch = linked_batch_id.flatten();

                let status = if qty <= 0 {
                    "out_of_stock"
                } else if reorder > 0 && qty <= reorder {
                    "low_stock"
                } else {
                    "available"
                };

                conn.execute(
                    "INSERT INTO inventory_items (entity_id, sku, name, category, qty_on_hand, qty_reserved, location_area, location_rack, supplier, lead_time_days, reorder_threshold, status, linked_batch_id, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![entity_id, sku, name, category, qty, loc_area, loc_rack, supp, lead, reorder, status, batch, now, now],
                ).context("inserting inventory item")?;
                let id = conn.last_insert_rowid();

                drop(now);
                self.get_inventory_by_id_internal(&conn, id)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Internal helper: fetch inventory by internal ID from an already-locked connection.
    /// Upsert inventory by entity_id. If an item with the given entity_id
    /// exists, update its fields (including name); otherwise return an error.
    #[allow(dead_code)]
    pub fn upsert_inventory_by_entity_id(
        &self,
        entity_id: &str,
        name: Option<&str>,
        sku: Option<&str>,
        category: Option<&str>,
        qty_on_hand: Option<i32>,
        location_area: Option<&str>,
        location_rack: Option<&str>,
        supplier: Option<&str>,
        lead_time_days: Option<i32>,
        reorder_threshold: Option<i32>,
        linked_batch_id: Option<Option<i64>>,
    ) -> Result<InventoryItemRecord> {
        let conn = self.conn.lock().unwrap();
        let existing_id: i64 = conn.query_row(
            "SELECT id FROM inventory_items WHERE entity_id = ?1",
            params![entity_id],
            |row| row.get(0),
        )?;
        let now = Self::now();
        if let Some(v) = name {
            conn.execute("UPDATE inventory_items SET name = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        if let Some(v) = sku {
            conn.execute("UPDATE inventory_items SET sku = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        if let Some(v) = category {
            conn.execute("UPDATE inventory_items SET category = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        if let Some(v) = qty_on_hand {
            conn.execute("UPDATE inventory_items SET qty_on_hand = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        if let Some(v) = location_area {
            conn.execute("UPDATE inventory_items SET location_area = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        if let Some(v) = location_rack {
            conn.execute("UPDATE inventory_items SET location_rack = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        if let Some(v) = supplier {
            conn.execute("UPDATE inventory_items SET supplier = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        if let Some(v) = lead_time_days {
            conn.execute("UPDATE inventory_items SET lead_time_days = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        if let Some(v) = reorder_threshold {
            conn.execute("UPDATE inventory_items SET reorder_threshold = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        if let Some(v) = linked_batch_id {
            conn.execute("UPDATE inventory_items SET linked_batch_id = ?1, updated_at = ?2 WHERE id = ?3", params![v, now, existing_id])?;
        }
        // Update status
        conn.execute(
            "UPDATE inventory_items SET status = CASE
                WHEN qty_on_hand <= 0 THEN 'out_of_stock'
                WHEN reorder_threshold > 0 AND qty_on_hand <= reorder_threshold THEN 'low_stock'
                ELSE 'available'
            END, updated_at = ?1 WHERE id = ?2",
            params![now, existing_id],
        )?;
        self.get_inventory_by_id_internal(&conn, existing_id)
    }

    fn get_inventory_by_id_internal(
        &self,
        conn: &Connection,
        id: i64,
    ) -> Result<InventoryItemRecord> {
        let rec = conn.query_row(
            "SELECT id, entity_id, sku, name, category, qty_on_hand, qty_reserved, location_area, location_rack, supplier, lead_time_days, reorder_threshold, status, linked_batch_id, created_at, updated_at FROM inventory_items WHERE id = ?1",
            params![id],
            |row| {
                Ok(InventoryItemRecord {
                    id: row.get(0)?,
                    entity_id: row.get(1)?,
                    sku: row.get(2)?,
                    name: row.get(3)?,
                    category: row.get(4)?,
                    qty_on_hand: row.get(5)?,
                    qty_reserved: row.get(6)?,
                    location_area: row.get(7)?,
                    location_rack: row.get(8)?,
                    supplier: row.get(9)?,
                    lead_time_days: row.get(10)?,
                    reorder_threshold: row.get(11)?,
                    status: row.get(12)?,
                    linked_batch_id: row.get(13)?,
                    created_at: row.get(14)?,
                    updated_at: row.get(15)?,
                })
            },
        )?;
        Ok(rec)
    }

    /// Get inventory item by entity_id.
    pub fn get_inventory(
        &self,
        entity_id: &str,
    ) -> Result<Option<InventoryItemRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, sku, name, category, qty_on_hand, qty_reserved, location_area, location_rack, supplier, lead_time_days, reorder_threshold, status, linked_batch_id, created_at, updated_at FROM inventory_items WHERE entity_id = ?1",
        )?;
        let mut rows =
            stmt.query_map(params![entity_id], Self::map_inventory_row)?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Get inventory item by internal ID.
    #[allow(dead_code)]
    pub fn get_inventory_by_id(
        &self,
        id: i64,
    ) -> Result<Option<InventoryItemRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, sku, name, category, qty_on_hand, qty_reserved, location_area, location_rack, supplier, lead_time_days, reorder_threshold, status, linked_batch_id, created_at, updated_at FROM inventory_items WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], Self::map_inventory_row)?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Map a SQLite row to an InventoryItemRecord.
    fn map_inventory_row(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<InventoryItemRecord> {
        Ok(InventoryItemRecord {
            id: row.get(0)?,
            entity_id: row.get(1)?,
            sku: row.get(2)?,
            name: row.get(3)?,
            category: row.get(4)?,
            qty_on_hand: row.get(5)?,
            qty_reserved: row.get(6)?,
            location_area: row.get(7)?,
            location_rack: row.get(8)?,
            supplier: row.get(9)?,
            lead_time_days: row.get(10)?,
            reorder_threshold: row.get(11)?,
            status: row.get(12)?,
            linked_batch_id: row.get(13)?,
            created_at: row.get(14)?,
            updated_at: row.get(15)?,
        })
    }

    /// List all inventory items.
    pub fn list_inventory(&self) -> Result<Vec<InventoryItemRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, sku, name, category, qty_on_hand, qty_reserved, location_area, location_rack, supplier, lead_time_days, reorder_threshold, status, linked_batch_id, created_at, updated_at FROM inventory_items ORDER BY name",
        )?;
        let rows = stmt.query_map([], Self::map_inventory_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// List inventory items that are low on stock (qty_on_hand <= reorder_threshold and > 0).
    pub fn list_inventory_low_stock(&self) -> Result<Vec<InventoryItemRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, sku, name, category, qty_on_hand, qty_reserved, location_area, location_rack, supplier, lead_time_days, reorder_threshold, status, linked_batch_id, created_at, updated_at FROM inventory_items WHERE status IN ('low_stock', 'out_of_stock') ORDER BY name",
        )?;
        let rows = stmt.query_map([], Self::map_inventory_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Reserve inventory: decrement available qty in a transaction.
    /// Returns the updated record, or an error if insufficient stock.
    pub fn reserve_inventory(
        &self,
        entity_id: &str,
        quantity: i32,
        linked_batch_id: Option<i64>,
    ) -> Result<InventoryItemRecord> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("BEGIN TRANSACTION")?;

        let result = (|| -> Result<InventoryItemRecord> {
            let (id, qty_on_hand, qty_reserved): (i64, i32, i32) = conn.query_row(
                "SELECT id, qty_on_hand, qty_reserved FROM inventory_items WHERE entity_id = ?1",
                params![entity_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).map_err(|e| anyhow::anyhow!("Inventory item not found: {}", e))?;

            let available = qty_on_hand - qty_reserved;
            if quantity > available {
                conn.execute_batch("ROLLBACK")?;
                anyhow::bail!(
                    "Insufficient stock: item has {} available ({} on hand - {} reserved), but {} requested",
                    available,
                    qty_on_hand,
                    qty_reserved,
                    quantity
                );
            }

            let now = Self::now();
            let new_reserved = qty_reserved + quantity;

            if let Some(batch) = linked_batch_id {
                conn.execute(
                    "UPDATE inventory_items SET qty_reserved = ?1, linked_batch_id = ?2, updated_at = ?3 WHERE entity_id = ?4",
                    params![new_reserved, batch, now, entity_id],
                )?;
            } else {
                conn.execute(
                    "UPDATE inventory_items SET qty_reserved = ?1, updated_at = ?2 WHERE entity_id = ?3",
                    params![new_reserved, now, entity_id],
                )?;
            }

            conn.execute_batch("COMMIT")?;

            // Fetch and return the updated record
            let rec = conn.query_row(
                "SELECT id, entity_id, sku, name, category, qty_on_hand, qty_reserved, location_area, location_rack, supplier, lead_time_days, reorder_threshold, status, linked_batch_id, created_at, updated_at FROM inventory_items WHERE id = ?1",
                params![id],
                Self::map_inventory_row,
            )?;
            Ok(rec)
        })();

        if result.is_err() {
            let _ = conn.execute_batch("ROLLBACK");
        }

        result
    }
}

// ── ProcurementOrder CRUD ───────────────────────────────────

impl Store {
    /// Insert a new procurement order, generating a UUID entity_id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_procurement_order(
        &self,
        item_sku: &str,
        item_name: &str,
        supplier: &str,
        qty: i32,
        lead_time_days: i32,
        linked_batch_id: Option<i64>,
        notes: &str,
    ) -> Result<ProcurementOrderRecord> {
        let entity_id = Self::generate_id();
        let now = Self::now();
        let order_date = now.clone();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO procurement_orders (entity_id, item_sku, item_name, supplier, qty, lead_time_days, order_date, status, linked_batch_id, notes, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9, ?10, ?11)",
            params![entity_id, item_sku, item_name, supplier, qty, lead_time_days, order_date, linked_batch_id, notes, now, now],
        ).context("inserting procurement order")?;
        let id = conn.last_insert_rowid();
        Ok(ProcurementOrderRecord {
            id,
            entity_id,
            order_ref: String::new(),
            item_sku: item_sku.to_string(),
            item_name: item_name.to_string(),
            supplier: supplier.to_string(),
            qty,
            lead_time_days,
            order_date: Some(order_date),
            eta: None,
            status: "pending".to_string(),
            linked_batch_id,
            notes: notes.to_string(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Update an existing procurement order by entity_id.
    pub fn update_procurement_order(
        &self,
        entity_id: &str,
        status: Option<&str>,
        eta: Option<&str>,
        order_ref: Option<&str>,
        supplier: Option<&str>,
        qty: Option<i32>,
        notes: Option<&str>,
    ) -> Result<Option<ProcurementOrderRecord>> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM procurement_orders WHERE entity_id = ?1",
                params![entity_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)?;
        if !exists {
            return Ok(None);
        }

        let now = Self::now();
        if let Some(v) = status {
            conn.execute("UPDATE procurement_orders SET status = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = eta {
            conn.execute("UPDATE procurement_orders SET eta = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = order_ref {
            conn.execute("UPDATE procurement_orders SET order_ref = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = supplier {
            conn.execute("UPDATE procurement_orders SET supplier = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = qty {
            conn.execute("UPDATE procurement_orders SET qty = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = notes {
            conn.execute("UPDATE procurement_orders SET notes = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }

        let rec = conn.query_row(
            "SELECT id, entity_id, order_ref, item_sku, item_name, supplier, qty, lead_time_days, order_date, eta, status, linked_batch_id, notes, created_at, updated_at FROM procurement_orders WHERE entity_id = ?1",
            params![entity_id],
            Self::map_procurement_row,
        )?;
        Ok(Some(rec))
    }

    /// Get a procurement order by order_ref.
    #[allow(dead_code)]
    pub fn get_procurement_order_by_ref(
        &self,
        order_ref: &str,
    ) -> Result<Option<ProcurementOrderRecord>> {
        let conn = self.conn.lock().unwrap();
        if order_ref.is_empty() {
            return Ok(None);
        }
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, order_ref, item_sku, item_name, supplier, qty, lead_time_days, order_date, eta, status, linked_batch_id, notes, created_at, updated_at FROM procurement_orders WHERE order_ref = ?1",
        )?;
        let mut rows =
            stmt.query_map(params![order_ref], Self::map_procurement_row)?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Get a procurement order by entity_id.
    #[allow(dead_code)]
    pub fn get_procurement_order(
        &self,
        entity_id: &str,
    ) -> Result<Option<ProcurementOrderRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, order_ref, item_sku, item_name, supplier, qty, lead_time_days, order_date, eta, status, linked_batch_id, notes, created_at, updated_at FROM procurement_orders WHERE entity_id = ?1",
        )?;
        let mut rows =
            stmt.query_map(params![entity_id], Self::map_procurement_row)?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    fn map_procurement_row(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<ProcurementOrderRecord> {
        Ok(ProcurementOrderRecord {
            id: row.get(0)?,
            entity_id: row.get(1)?,
            order_ref: row.get(2)?,
            item_sku: row.get(3)?,
            item_name: row.get(4)?,
            supplier: row.get(5)?,
            qty: row.get(6)?,
            lead_time_days: row.get(7)?,
            order_date: row.get(8)?,
            eta: row.get(9)?,
            status: row.get(10)?,
            linked_batch_id: row.get(11)?,
            notes: row.get(12)?,
            created_at: row.get(13)?,
            updated_at: row.get(14)?,
        })
    }

    /// List open (not delivered/cancelled) procurement orders.
    pub fn list_open_orders(&self) -> Result<Vec<ProcurementOrderRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, order_ref, item_sku, item_name, supplier, qty, lead_time_days, order_date, eta, status, linked_batch_id, notes, created_at, updated_at FROM procurement_orders WHERE status NOT IN ('delivered', 'cancelled') ORDER BY order_date",
        )?;
        let rows = stmt.query_map([], Self::map_procurement_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }
}

// ── Alert CRUD ──────────────────────────────────────────────

#[allow(dead_code)]
impl Store {
    /// Insert a new alert, generating a UUID entity_id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_alert(
        &self,
        alert_type: &str,
        severity: &str,
        related_entity_type: &str,
        related_entity_id: &str,
        due_date: Option<&str>,
        reason: &str,
        suggested_action: &str,
    ) -> Result<AlertRecord> {
        let entity_id = Self::generate_id();
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO alerts (entity_id, alert_type, severity, related_entity_type, related_entity_id, due_date, reason, suggested_action, resolved, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10)",
            params![entity_id, alert_type, severity, related_entity_type, related_entity_id, due_date, reason, suggested_action, now, now],
        ).context("inserting alert")?;
        let id = conn.last_insert_rowid();
        Ok(AlertRecord {
            id,
            entity_id,
            alert_type: alert_type.to_string(),
            severity: severity.to_string(),
            related_entity_type: related_entity_type.to_string(),
            related_entity_id: related_entity_id.to_string(),
            due_date: due_date.map(String::from),
            reason: reason.to_string(),
            suggested_action: suggested_action.to_string(),
            resolved: false,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Update an existing alert by entity_id.
    #[allow(dead_code)]
    pub fn update_alert(
        &self,
        entity_id: &str,
        alert_type: Option<&str>,
        severity: Option<&str>,
        reason: Option<&str>,
        suggested_action: Option<&str>,
        resolved: Option<bool>,
    ) -> Result<Option<AlertRecord>> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM alerts WHERE entity_id = ?1",
                params![entity_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)?;
        if !exists {
            return Ok(None);
        }

        let now = Self::now();
        if let Some(v) = alert_type {
            conn.execute("UPDATE alerts SET alert_type = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = severity {
            conn.execute("UPDATE alerts SET severity = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = reason {
            conn.execute("UPDATE alerts SET reason = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = suggested_action {
            conn.execute("UPDATE alerts SET suggested_action = ?1, updated_at = ?2 WHERE entity_id = ?3", params![v, now, entity_id])?;
        }
        if let Some(v) = resolved {
            conn.execute("UPDATE alerts SET resolved = ?1, updated_at = ?2 WHERE entity_id = ?3", params![bool_to_int(v), now, entity_id])?;
        }

        let rec = conn.query_row(
            "SELECT id, entity_id, alert_type, severity, related_entity_type, related_entity_id, due_date, reason, suggested_action, resolved, created_at, updated_at FROM alerts WHERE entity_id = ?1",
            params![entity_id],
            |row| {
                Ok(AlertRecord {
                    id: row.get(0)?,
                    entity_id: row.get(1)?,
                    alert_type: row.get(2)?,
                    severity: row.get(3)?,
                    related_entity_type: row.get(4)?,
                    related_entity_id: row.get(5)?,
                    due_date: row.get(6)?,
                    reason: row.get(7)?,
                    suggested_action: row.get(8)?,
                    resolved: int_to_bool(row.get::<_, i32>(9)?),
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            },
        )?;
        Ok(Some(rec))
    }

    /// Find an active (unresolved) alert by type and related entity ID.
    pub fn find_active_alert_by_type_and_entity(
        &self,
        alert_type: &str,
        related_entity_id: &str,
    ) -> Result<Option<AlertRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, alert_type, severity, related_entity_type, related_entity_id, due_date, reason, suggested_action, resolved, created_at, updated_at
             FROM alerts WHERE alert_type = ?1 AND related_entity_id = ?2 AND resolved = 0 LIMIT 1",
        )?;
        let mut rows =
            stmt.query_map(params![alert_type, related_entity_id], |row| {
                Ok(AlertRecord {
                    id: row.get(0)?,
                    entity_id: row.get(1)?,
                    alert_type: row.get(2)?,
                    severity: row.get(3)?,
                    related_entity_type: row.get(4)?,
                    related_entity_id: row.get(5)?,
                    due_date: row.get(6)?,
                    reason: row.get(7)?,
                    suggested_action: row.get(8)?,
                    resolved: int_to_bool(row.get::<_, i32>(9)?),
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// List active (not resolved) alerts, ordered by severity.
    pub fn list_active_alerts(&self) -> Result<Vec<AlertRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, alert_type, severity, related_entity_type, related_entity_id, due_date, reason, suggested_action, resolved, created_at, updated_at FROM alerts WHERE resolved = 0 ORDER BY
                CASE severity
                    WHEN 'overdue' THEN 0
                    WHEN 'urgent' THEN 1
                    WHEN 'upcoming' THEN 2
                    ELSE 3
                END",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(AlertRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                alert_type: row.get(2)?,
                severity: row.get(3)?,
                related_entity_type: row.get(4)?,
                related_entity_id: row.get(5)?,
                due_date: row.get(6)?,
                reason: row.get(7)?,
                suggested_action: row.get(8)?,
                resolved: int_to_bool(row.get::<_, i32>(9)?),
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Resolve an alert by entity_id.
    pub fn resolve_alert(
        &self,
        entity_id: &str,
    ) -> Result<Option<AlertRecord>> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM alerts WHERE entity_id = ?1",
                params![entity_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)?;
        if !exists {
            return Ok(None);
        }
        let now = Self::now();
        conn.execute(
            "UPDATE alerts SET resolved = 1, updated_at = ?1 WHERE entity_id = ?2",
            params![now, entity_id],
        )?;
        let rec = conn.query_row(
            "SELECT id, entity_id, alert_type, severity, related_entity_type, related_entity_id, due_date, reason, suggested_action, resolved, created_at, updated_at FROM alerts WHERE entity_id = ?1",
            params![entity_id],
            |row| {
                Ok(AlertRecord {
                    id: row.get(0)?,
                    entity_id: row.get(1)?,
                    alert_type: row.get(2)?,
                    severity: row.get(3)?,
                    related_entity_type: row.get(4)?,
                    related_entity_id: row.get(5)?,
                    due_date: row.get(6)?,
                    reason: row.get(7)?,
                    suggested_action: row.get(8)?,
                    resolved: int_to_bool(row.get::<_, i32>(9)?),
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            },
        )?;
        Ok(Some(rec))
    }
}

// ── EventLog CRUD ───────────────────────────────────────────

impl Store {
    /// Append an event log entry. Returns the internal ID of the new row.
    pub fn log_event(
        &self,
        timestamp: &str,
        source: &str,
        action_type: &str,
        entity_type: &str,
        entity_id: &str,
        summary: &str,
    ) -> Result<i64> {
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO event_logs (timestamp, source, action_type, entity_type, entity_id, summary, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![timestamp, source, action_type, entity_type, entity_id, summary, now],
        ).context("logging event")?;
        Ok(conn.last_insert_rowid())
    }

    /// List recent event log entries, ordered by most recent first.
    pub fn list_events(&self, limit: i64) -> Result<Vec<EventLogRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, source, action_type, entity_type, entity_id, summary, created_at FROM event_logs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(EventLogRecord {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                source: row.get(2)?,
                action_type: row.get(3)?,
                entity_type: row.get(4)?,
                entity_id: row.get(5)?,
                summary: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }
}

// ── Entity existence helpers ─────────────────────────────────

impl Store {
    /// Check if a site exists by its internal ID.
    pub fn site_exists_by_id(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM sites WHERE id = ?1",
            params![id],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .map_err(Into::into)
    }

    /// Check if a batch exists by its internal ID.
    pub fn batch_exists_by_id(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM batches WHERE id = ?1",
            params![id],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .map_err(Into::into)
    }

    /// Check if a person exists by its internal ID.
    #[allow(dead_code)]
    pub fn person_exists_by_id(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM people WHERE id = ?1",
            params![id],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .map_err(Into::into)
    }

    /// Check if an inventory item exists by SKU or name.
    pub fn inventory_exists_by_sku_or_name(
        &self,
        sku: &str,
        name: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM inventory_items WHERE sku = ?1 OR name = ?2",
            params![sku, name],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .map_err(Into::into)
    }

    /// Get an inventory item by its name (for SKU/name resolution).
    #[allow(dead_code)]
    pub fn get_inventory_by_name(
        &self,
        name: &str,
    ) -> Result<Option<InventoryItemRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, sku, name, category, qty_on_hand, qty_reserved, location_area, location_rack, supplier, lead_time_days, reorder_threshold, status, linked_batch_id, created_at, updated_at FROM inventory_items WHERE name = ?1",
        )?;
        let mut rows =
            stmt.query_map(params![name], Self::map_inventory_row)?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Get an inventory item by its SKU.
    #[allow(dead_code)]
    pub fn get_inventory_by_sku(
        &self,
        sku: &str,
    ) -> Result<Option<InventoryItemRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, sku, name, category, qty_on_hand, qty_reserved, location_area, location_rack, supplier, lead_time_days, reorder_threshold, status, linked_batch_id, created_at, updated_at FROM inventory_items WHERE sku = ?1",
        )?;
        let mut rows = stmt.query_map(params![sku], Self::map_inventory_row)?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }
}

// ── Assignment CRUD ─────────────────────────────────────────

impl Store {
    /// Insert a new assignment, generating a UUID entity_id.
    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn insert_assignment(
        &self,
        person_id: i64,
        site_id: Option<i64>,
        batch_id: Option<i64>,
        role: &str,
        start_date: Option<&str>,
        end_date: Option<&str>,
        handover_to_person_id: Option<i64>,
    ) -> Result<AssignmentRecord> {
        let entity_id = Self::generate_id();
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO assignments (entity_id, person_id, site_id, batch_id, role, start_date, end_date, handover_to_person_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![entity_id, person_id, site_id, batch_id, role, start_date, end_date, handover_to_person_id, now, now],
        ).context("inserting assignment")?;
        let id = conn.last_insert_rowid();
        Ok(AssignmentRecord {
            id,
            entity_id,
            person_id,
            site_id,
            batch_id,
            role: role.to_string(),
            start_date: start_date.map(String::from),
            end_date: end_date.map(String::from),
            handover_to_person_id,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Get an assignment by its entity_id.
    #[allow(dead_code)]
    pub fn get_assignment(
        &self,
        entity_id: &str,
    ) -> Result<Option<AssignmentRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, person_id, site_id, batch_id, role, start_date, end_date, handover_to_person_id, created_at, updated_at FROM assignments WHERE entity_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![entity_id], |row| {
            Ok(AssignmentRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                person_id: row.get(2)?,
                site_id: row.get(3)?,
                batch_id: row.get(4)?,
                role: row.get(5)?,
                start_date: row.get(6)?,
                end_date: row.get(7)?,
                handover_to_person_id: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Update handover fields on an assignment.
    pub fn update_assignment_handover(
        &self,
        entity_id: &str,
        handover_to_person_id: Option<i64>,
        end_date: Option<&str>,
    ) -> Result<Option<AssignmentRecord>> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM assignments WHERE entity_id = ?1",
                params![entity_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)?;
        if !exists {
            return Ok(None);
        }
        let now = Self::now();
        if let Some(h) = handover_to_person_id {
            conn.execute(
                "UPDATE assignments SET handover_to_person_id = ?1, updated_at = ?2 WHERE entity_id = ?3",
                params![h, now, entity_id],
            )?;
        }
        if let Some(d) = end_date {
            conn.execute(
                "UPDATE assignments SET end_date = ?1, updated_at = ?2 WHERE entity_id = ?3",
                params![d, now, entity_id],
            )?;
        }
        let rec = conn.query_row(
            "SELECT id, entity_id, person_id, site_id, batch_id, role, start_date, end_date, handover_to_person_id, created_at, updated_at FROM assignments WHERE entity_id = ?1",
            params![entity_id],
            |row| {
                Ok(AssignmentRecord {
                    id: row.get(0)?,
                    entity_id: row.get(1)?,
                    person_id: row.get(2)?,
                    site_id: row.get(3)?,
                    batch_id: row.get(4)?,
                    role: row.get(5)?,
                    start_date: row.get(6)?,
                    end_date: row.get(7)?,
                    handover_to_person_id: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            },
        )?;
        Ok(Some(rec))
    }

    /// Find an assignment for a person at a specific site.
    pub fn find_assignment_by_person_and_site(
        &self,
        person_id: i64,
        site_id: i64,
    ) -> Result<Option<AssignmentRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, person_id, site_id, batch_id, role, start_date, end_date, handover_to_person_id, created_at, updated_at FROM assignments WHERE person_id = ?1 AND site_id = ?2 ORDER BY start_date DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![person_id, site_id], |row| {
            Ok(AssignmentRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                person_id: row.get(2)?,
                site_id: row.get(3)?,
                batch_id: row.get(4)?,
                role: row.get(5)?,
                start_date: row.get(6)?,
                end_date: row.get(7)?,
                handover_to_person_id: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Find an assignment for a person at a specific batch.
    pub fn find_assignment_by_person_and_batch(
        &self,
        person_id: i64,
        batch_id: i64,
    ) -> Result<Option<AssignmentRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, person_id, site_id, batch_id, role, start_date, end_date, handover_to_person_id, created_at, updated_at FROM assignments WHERE person_id = ?1 AND batch_id = ?2 ORDER BY start_date DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![person_id, batch_id], |row| {
            Ok(AssignmentRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                person_id: row.get(2)?,
                site_id: row.get(3)?,
                batch_id: row.get(4)?,
                role: row.get(5)?,
                start_date: row.get(6)?,
                end_date: row.get(7)?,
                handover_to_person_id: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// List assignments for a specific person.
    #[allow(dead_code)]
    pub fn list_assignments_for_person(
        &self,
        person_id: i64,
    ) -> Result<Vec<AssignmentRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, entity_id, person_id, site_id, batch_id, role, start_date, end_date, handover_to_person_id, created_at, updated_at FROM assignments WHERE person_id = ?1 ORDER BY start_date",
        )?;
        let rows = stmt.query_map(params![person_id], |row| {
            Ok(AssignmentRecord {
                id: row.get(0)?,
                entity_id: row.get(1)?,
                person_id: row.get(2)?,
                site_id: row.get(3)?,
                batch_id: row.get(4)?,
                role: row.get(5)?,
                start_date: row.get(6)?,
                end_date: row.get(7)?,
                handover_to_person_id: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }
}

// ── Imported Google Sheets jobs (title-keyed) ─────────────────

/// Shared SELECT column list for `imported_jobs`.
const IMPORTED_JOB_COLUMNS: &str = "id, entity_id, source, spreadsheet_id, sheet_name, external_job_key, title, location, activity_type, start_date, end_date, job_leader, team_member, robots, additional_info, source_done, local_status, row_hash, source_payload, first_seen_at, last_seen_at, last_import_run_id, created_at, updated_at";

/// Shared SELECT column list for `import_conflicts`.
const IMPORT_CONFLICT_COLUMNS: &str = "id, import_run_id, source, external_job_key, conflict_type, reason, details, status, created_at, resolved_at";

#[allow(dead_code)]
fn imported_job_status_from_str(s: &str) -> ImportedJobStatus {
    match s {
        "completed" => ImportedJobStatus::Completed,
        "ignored" => ImportedJobStatus::Ignored,
        // Unknown/corrupt values fall back to active so a legacy row never
        // breaks a reader.
        _ => ImportedJobStatus::Active,
    }
}

#[allow(dead_code)]
fn imported_job_status_to_str(status: &ImportedJobStatus) -> &'static str {
    match status {
        ImportedJobStatus::Active => "active",
        ImportedJobStatus::Completed => "completed",
        ImportedJobStatus::Ignored => "ignored",
    }
}

#[allow(dead_code)]
fn import_run_status_from_str(s: &str) -> ImportRunStatus {
    match s {
        "applied" => ImportRunStatus::Applied,
        "failed" => ImportRunStatus::Failed,
        _ => ImportRunStatus::Previewed,
    }
}

#[allow(dead_code)]
fn import_run_status_to_str(status: &ImportRunStatus) -> &'static str {
    match status {
        ImportRunStatus::Previewed => "previewed",
        ImportRunStatus::Applied => "applied",
        ImportRunStatus::Failed => "failed",
    }
}

#[allow(dead_code)]
fn import_conflict_status_from_str(s: &str) -> ImportConflictStatus {
    if s == "resolved" {
        ImportConflictStatus::Resolved
    } else {
        ImportConflictStatus::Open
    }
}

#[allow(dead_code)]
fn import_conflict_status_to_str(
    status: &ImportConflictStatus,
) -> &'static str {
    match status {
        ImportConflictStatus::Open => "open",
        ImportConflictStatus::Resolved => "resolved",
    }
}

#[allow(dead_code)]
fn import_conflict_type_from_str(s: &str) -> ImportConflictType {
    match s {
        "blank_title" => ImportConflictType::BlankTitle,
        "duplicate_title" => ImportConflictType::DuplicateTitle,
        "invalid_row" => ImportConflictType::InvalidRow,
        _ => ImportConflictType::ReopenCompleted,
    }
}

#[allow(dead_code)]
fn import_conflict_type_to_str(t: &ImportConflictType) -> &'static str {
    match t {
        ImportConflictType::BlankTitle => "blank_title",
        ImportConflictType::DuplicateTitle => "duplicate_title",
        ImportConflictType::InvalidRow => "invalid_row",
        ImportConflictType::ReopenCompleted => "reopen_completed",
    }
}

/// Map a `imported_jobs` row to a typed record.
fn imported_job_from_row(
    row: &rusqlite::Row,
) -> rusqlite::Result<ImportedJobRecord> {
    let source_done: i32 = row.get(15)?;
    let local_status: String = row.get(16)?;
    let source_payload: String = row.get(18)?;
    Ok(ImportedJobRecord {
        id: row.get(0)?,
        entity_id: row.get(1)?,
        source: row.get(2)?,
        spreadsheet_id: row.get(3)?,
        sheet_name: row.get(4)?,
        external_job_key: row.get(5)?,
        title: row.get(6)?,
        location: row.get(7)?,
        activity_type: row.get(8)?,
        start_date: row.get(9)?,
        end_date: row.get(10)?,
        job_leader: row.get(11)?,
        team_member: row.get(12)?,
        robots: row.get(13)?,
        additional_info: row.get(14)?,
        source_done: int_to_bool(source_done),
        local_status: imported_job_status_from_str(&local_status),
        row_hash: row.get(17)?,
        source_payload: serde_json::from_str(&source_payload)
            .unwrap_or_default(),
        first_seen_at: row.get(19)?,
        last_seen_at: row.get(20)?,
        last_import_run_id: row.get(21)?,
        created_at: row.get(22)?,
        updated_at: row.get(23)?,
    })
}

/// Insert or update one imported job on a connection.
///
/// On conflict the stable `entity_id` and `first_seen_at` are preserved; every
/// mapped source field plus `last_seen_at`/`last_import_run_id` are refreshed.
#[allow(dead_code)]
fn upsert_imported_job_on(
    conn: &Connection,
    input: &ImportedJobInput,
    now: &str,
) -> Result<()> {
    let entity_id = Store::generate_id();
    conn.execute(
        "INSERT INTO imported_jobs (
            entity_id, source, spreadsheet_id, sheet_name, external_job_key,
            title, location, activity_type, start_date, end_date,
            job_leader, team_member, robots, additional_info,
            source_done, local_status, row_hash, source_payload,
            first_seen_at, last_seen_at, last_import_run_id, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5,
            ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18,
            ?19, ?20, ?21, ?22, ?23
        )
        ON CONFLICT (source, external_job_key) DO UPDATE SET
            spreadsheet_id = excluded.spreadsheet_id,
            sheet_name = excluded.sheet_name,
            title = excluded.title,
            location = excluded.location,
            activity_type = excluded.activity_type,
            start_date = excluded.start_date,
            end_date = excluded.end_date,
            job_leader = excluded.job_leader,
            team_member = excluded.team_member,
            robots = excluded.robots,
            additional_info = excluded.additional_info,
            source_done = excluded.source_done,
            local_status = excluded.local_status,
            row_hash = excluded.row_hash,
            source_payload = excluded.source_payload,
            last_seen_at = excluded.last_seen_at,
            last_import_run_id = excluded.last_import_run_id,
            updated_at = excluded.updated_at",
        params![
            entity_id,
            input.source,
            input.spreadsheet_id,
            input.sheet_name,
            input.external_job_key,
            input.title,
            input.location,
            input.activity_type,
            input.start_date,
            input.end_date,
            input.job_leader,
            input.team_member,
            input.robots,
            input.additional_info,
            bool_to_int(input.source_done),
            imported_job_status_to_str(&input.local_status),
            input.row_hash,
            input.source_payload.to_string(),
            now,
            now,
            input.import_run_id,
            now,
            now,
        ],
    )
    .context("upserting imported job")?;
    Ok(())
}

/// Read one imported job by `(source, external_job_key)`.
#[allow(dead_code)]
fn read_imported_job(
    conn: &Connection,
    source: &str,
    external_job_key: &str,
) -> Result<Option<ImportedJobRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {IMPORTED_JOB_COLUMNS} FROM imported_jobs WHERE source = ?1 AND external_job_key = ?2"
    ))?;
    let mut rows = stmt
        .query_map(params![source, external_job_key], imported_job_from_row)?;
    match rows.next() {
        Some(Ok(rec)) => Ok(Some(rec)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// Insert an `import_runs` row on a connection.
#[allow(dead_code)]
fn insert_import_run_on(
    conn: &Connection,
    input: &ImportRunInput,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO import_runs (
            id, source, spreadsheet_id, sheet_name, triggered_by,
            dry_run, status, preview_hash,
            seen_count, created_count, updated_count, unchanged_count,
            conflict_count, invalid_count, skipped_count,
            summary, created_at, completed_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
            ?9, ?10, ?11, ?12, ?13, ?14, ?15,
            ?16, ?17, ?18
        )",
        params![
            input.id,
            input.source,
            input.spreadsheet_id,
            input.sheet_name,
            input.triggered_by,
            bool_to_int(input.dry_run),
            import_run_status_to_str(&input.status),
            input.preview_hash,
            input.seen_count,
            input.created_count,
            input.updated_count,
            input.unchanged_count,
            input.conflict_count,
            input.invalid_count,
            input.skipped_count,
            input.summary.to_string(),
            now,
            now,
        ],
    )
    .context("inserting import run")?;
    Ok(())
}

/// Map an `import_runs` row to a typed record.
fn import_run_from_row(
    row: &rusqlite::Row,
) -> rusqlite::Result<ImportRunRecord> {
    let dry_run: i32 = row.get(5)?;
    let status: String = row.get(6)?;
    let summary: String = row.get(15)?;
    Ok(ImportRunRecord {
        id: row.get(0)?,
        source: row.get(1)?,
        spreadsheet_id: row.get(2)?,
        sheet_name: row.get(3)?,
        triggered_by: row.get(4)?,
        dry_run: int_to_bool(dry_run),
        status: import_run_status_from_str(&status),
        preview_hash: row.get(7)?,
        seen_count: row.get(8)?,
        created_count: row.get(9)?,
        updated_count: row.get(10)?,
        unchanged_count: row.get(11)?,
        conflict_count: row.get(12)?,
        invalid_count: row.get(13)?,
        skipped_count: row.get(14)?,
        summary: serde_json::from_str(&summary).unwrap_or_default(),
        created_at: row.get(16)?,
        completed_at: row.get(17)?,
    })
}

/// Read one import run by id.
#[allow(dead_code)]
fn read_import_run(
    conn: &Connection,
    id: &str,
) -> Result<Option<ImportRunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, source, spreadsheet_id, sheet_name, triggered_by, dry_run, status, preview_hash,
                seen_count, created_count, updated_count, unchanged_count, conflict_count, invalid_count, skipped_count,
                summary, created_at, completed_at
         FROM import_runs WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], import_run_from_row)?;
    match rows.next() {
        Some(Ok(rec)) => Ok(Some(rec)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// Insert an `import_conflicts` row on a connection; returns its row id.
#[allow(dead_code)]
fn insert_import_conflict_on(
    conn: &Connection,
    input: &ImportConflictInput,
    now: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO import_conflicts (
            import_run_id, source, external_job_key, conflict_type,
            reason, details, status, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7)",
        params![
            input.import_run_id,
            input.source,
            input.external_job_key,
            import_conflict_type_to_str(&input.conflict_type),
            input.reason,
            input.details.to_string(),
            now,
        ],
    )
    .context("inserting import conflict")?;
    Ok(conn.last_insert_rowid())
}

/// Map an `import_conflicts` row to a typed record.
fn import_conflict_from_row(
    row: &rusqlite::Row,
) -> rusqlite::Result<ImportConflictRecord> {
    let conflict_type: String = row.get(4)?;
    let details: String = row.get(6)?;
    let status: String = row.get(7)?;
    Ok(ImportConflictRecord {
        id: row.get(0)?,
        import_run_id: row.get(1)?,
        source: row.get(2)?,
        external_job_key: row.get(3)?,
        conflict_type: import_conflict_type_from_str(&conflict_type),
        reason: row.get(5)?,
        details: serde_json::from_str(&details).unwrap_or_default(),
        status: import_conflict_status_from_str(&status),
        created_at: row.get(8)?,
        resolved_at: row.get(9)?,
    })
}

/// Read one import conflict by id.
#[allow(dead_code)]
fn read_import_conflict(
    conn: &Connection,
    id: i64,
) -> Result<Option<ImportConflictRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {IMPORT_CONFLICT_COLUMNS} FROM import_conflicts WHERE id = ?1"
    ))?;
    let mut rows = stmt.query_map(params![id], import_conflict_from_row)?;
    match rows.next() {
        Some(Ok(rec)) => Ok(Some(rec)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

// ── Imported job CRUD ────────────────────────────────────────

#[allow(dead_code)] // entry points are wired by slice 3 tools
impl Store {
    /// Upsert one imported job keyed by `(source, external_job_key)`.
    ///
    /// New keys get a fresh stable `entity_id`; existing keys keep their
    /// `entity_id` and `first_seen_at` while every mapped source field is
    /// refreshed.
    pub fn upsert_imported_job(
        &self,
        input: &ImportedJobInput,
    ) -> Result<ImportedJobRecord> {
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        upsert_imported_job_on(&conn, input, &now)?;
        match read_imported_job(&conn, &input.source, &input.external_job_key)?
        {
            Some(rec) => Ok(rec),
            None => anyhow::bail!("imported job disappeared after upsert"),
        }
    }

    /// Get one imported job by `(source, external_job_key)`.
    pub fn get_imported_job(
        &self,
        source: &str,
        external_job_key: &str,
    ) -> Result<Option<ImportedJobRecord>> {
        let conn = self.conn.lock().unwrap();
        read_imported_job(&conn, source, external_job_key)
    }

    /// Get one imported job by its stable local UUID.
    pub fn get_imported_job_by_entity_id(
        &self,
        entity_id: &str,
    ) -> Result<Option<ImportedJobRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {IMPORTED_JOB_COLUMNS} FROM imported_jobs WHERE entity_id = ?1"
        ))?;
        let mut rows =
            stmt.query_map(params![entity_id], imported_job_from_row)?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// List all imported jobs for a source, ordered by title.
    pub fn list_imported_jobs(
        &self,
        source: &str,
    ) -> Result<Vec<ImportedJobRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {IMPORTED_JOB_COLUMNS} FROM imported_jobs WHERE source = ?1 ORDER BY title"
        ))?;
        let rows = stmt.query_map(params![source], imported_job_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// List imported jobs for a source filtered by local status.
    pub fn list_imported_jobs_by_status(
        &self,
        source: &str,
        status: ImportedJobStatus,
    ) -> Result<Vec<ImportedJobRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {IMPORTED_JOB_COLUMNS} FROM imported_jobs WHERE source = ?1 AND local_status = ?2 ORDER BY title"
        ))?;
        let rows = stmt.query_map(
            params![source, imported_job_status_to_str(&status)],
            imported_job_from_row,
        )?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Mark an imported job completed from the source (`Done = TRUE`).
    ///
    /// Sets `source_done = 1` and `local_status = 'completed'`. Returns `None`
    /// when no such job exists.
    pub fn mark_imported_job_completed(
        &self,
        source: &str,
        external_job_key: &str,
        import_run_id: Option<&str>,
    ) -> Result<Option<ImportedJobRecord>> {
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        let changed = conn
            .execute(
                "UPDATE imported_jobs
                 SET source_done = 1, local_status = 'completed',
                     last_seen_at = ?3, last_import_run_id = ?4, updated_at = ?3
                 WHERE source = ?1 AND external_job_key = ?2",
                params![source, external_job_key, now, import_run_id],
            )
            .context("marking imported job completed")?;
        if changed == 0 {
            return Ok(None);
        }
        read_imported_job(&conn, source, external_job_key)
    }

    /// Mark an imported job ignored locally.
    pub fn mark_imported_job_ignored(
        &self,
        source: &str,
        external_job_key: &str,
    ) -> Result<Option<ImportedJobRecord>> {
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        let changed = conn
            .execute(
                "UPDATE imported_jobs
                 SET local_status = 'ignored', updated_at = ?3
                 WHERE source = ?1 AND external_job_key = ?2",
                params![source, external_job_key, now],
            )
            .context("marking imported job ignored")?;
        if changed == 0 {
            return Ok(None);
        }
        read_imported_job(&conn, source, external_job_key)
    }
}

// ── Import runs and conflicts ───────────────────────────────

#[allow(dead_code)] // entry points are wired by slice 3 tools
impl Store {
    /// Insert an import run record (preview or apply bookkeeping).
    pub fn create_import_run(
        &self,
        input: &ImportRunInput,
    ) -> Result<ImportRunRecord> {
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        insert_import_run_on(&conn, input, &now)?;
        match read_import_run(&conn, &input.id)? {
            Some(rec) => Ok(rec),
            None => anyhow::bail!("import run disappeared after insert"),
        }
    }

    /// Get one import run by id.
    pub fn get_import_run(&self, id: &str) -> Result<Option<ImportRunRecord>> {
        let conn = self.conn.lock().unwrap();
        read_import_run(&conn, id)
    }

    /// List recent import runs for a source, newest first.
    pub fn list_import_runs(
        &self,
        source: &str,
        limit: i64,
    ) -> Result<Vec<ImportRunRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, source, spreadsheet_id, sheet_name, triggered_by, dry_run, status, preview_hash,
                    seen_count, created_count, updated_count, unchanged_count, conflict_count, invalid_count, skipped_count,
                    summary, created_at, completed_at
             FROM import_runs WHERE source = ?1 ORDER BY rowid DESC LIMIT ?2",
        )?;
        let rows =
            stmt.query_map(params![source, limit], import_run_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Insert an open import conflict for operator review.
    pub fn insert_import_conflict(
        &self,
        input: &ImportConflictInput,
    ) -> Result<ImportConflictRecord> {
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        let id = insert_import_conflict_on(&conn, input, &now)?;
        match read_import_conflict(&conn, id)? {
            Some(rec) => Ok(rec),
            None => anyhow::bail!("import conflict disappeared after insert"),
        }
    }

    /// List import conflicts for a source filtered by review status.
    pub fn list_import_conflicts(
        &self,
        source: &str,
        status: ImportConflictStatus,
    ) -> Result<Vec<ImportConflictRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {IMPORT_CONFLICT_COLUMNS} FROM import_conflicts WHERE source = ?1 AND status = ?2 ORDER BY id"
        ))?;
        let rows = stmt.query_map(
            params![source, import_conflict_status_to_str(&status)],
            import_conflict_from_row,
        )?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Resolve an open import conflict. Returns `None` if it is not open.
    pub fn resolve_import_conflict(
        &self,
        id: i64,
    ) -> Result<Option<ImportConflictRecord>> {
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        let changed = conn
            .execute(
                "UPDATE import_conflicts
                 SET status = 'resolved', resolved_at = ?2
                 WHERE id = ?1 AND status = 'open'",
                params![id, now],
            )
            .context("resolving import conflict")?;
        if changed == 0 {
            return Ok(None);
        }
        read_import_conflict(&conn, id)
    }

    /// Atomically apply an import in one SQLite transaction.
    ///
    /// Inserts the `import_runs` row, upserts `creates`/`updates`, marks each
    /// key in `complete_keys` completed from the source, records conflicts,
    /// and commits — or rolls everything back on any error.
    pub fn apply_import(
        &self,
        run: &ImportRunInput,
        creates: &[ImportedJobInput],
        updates: &[ImportedJobInput],
        complete_keys: &[String],
        conflict_inputs: &[ImportConflictInput],
    ) -> Result<ImportRunRecord> {
        let now = Self::now();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().context("beginning import transaction")?;

        insert_import_run_on(&tx, run, &now).context("inserting import run")?;
        for input in creates {
            upsert_imported_job_on(&tx, input, &now).with_context(|| {
                format!("applying create for '{}'", input.external_job_key)
            })?;
        }
        for input in updates {
            upsert_imported_job_on(&tx, input, &now).with_context(|| {
                format!("applying update for '{}'", input.external_job_key)
            })?;
        }
        for key in complete_keys {
            let changed = tx
                .execute(
                    "UPDATE imported_jobs
                     SET source_done = 1, local_status = 'completed',
                         last_seen_at = ?3, last_import_run_id = ?4, updated_at = ?3
                     WHERE source = ?1 AND external_job_key = ?2",
                    params![run.source, key, now, run.id],
                )
                .with_context(|| format!("marking imported job completed for '{key}'"))?;
            if changed == 0 {
                anyhow::bail!("cannot complete missing imported job: {key}");
            }
        }
        for input in conflict_inputs {
            insert_import_conflict_on(&tx, input, &now).with_context(|| {
                format!("recording conflict for '{}'", input.external_job_key)
            })?;
        }

        let record = read_import_run(&tx, &run.id)?
            .context("import run disappeared during apply")?;
        tx.commit().context("committing import transaction")?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_store() -> Store {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::pm::schema::bootstrap(&conn).expect("bootstrap");
        Store::new(conn)
    }

    // ── Site tests ──

    #[test]
    fn test_insert_and_get_site() {
        let store = create_store();
        let site = store
            .insert_site(
                "Test Site",
                "Location A",
                None,
                None,
                7,
                false,
                "notes",
            )
            .expect("insert site");
        assert!(site.id > 0);
        assert!(!site.entity_id.is_empty());

        let fetched = store.get_site(&site.entity_id).expect("get site");
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().name, "Test Site");
    }

    #[test]
    fn test_update_site() {
        let store = create_store();
        let site = store
            .insert_site("Old Name", "", None, None, 0, false, "")
            .expect("insert site");
        let updated = store
            .update_site(
                &site.entity_id,
                Some("New Name"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("update site");
        assert!(updated.is_some());
        assert_eq!(updated.unwrap().name, "New Name");
    }

    #[test]
    fn test_list_sites() {
        let store = create_store();
        store
            .insert_site("A", "", None, None, 0, false, "")
            .unwrap();
        store
            .insert_site("B", "", None, None, 0, false, "")
            .unwrap();
        let sites = store.list_sites().expect("list sites");
        assert_eq!(sites.len(), 2);
    }

    #[test]
    fn test_get_or_create_site() {
        let store = create_store();
        let first = store.get_or_create_site("Unique").expect("get or create");
        let second = store
            .get_or_create_site("Unique")
            .expect("get or create again");
        assert_eq!(first.id, second.id);
        assert_eq!(first.entity_id, second.entity_id);
    }

    #[test]
    fn test_site_not_found() {
        let store = create_store();
        let fetched = store.get_site("nonexistent").expect("get site");
        assert!(fetched.is_none());
    }

    // ── Batch tests ──

    #[test]
    fn test_insert_and_get_batch() {
        let store = create_store();
        let batch = store
            .insert_batch(
                "Batch 1",
                &["R001".into(), "R002".into()],
                None,
                Some("2026-08-18"),
                6,
                "planned",
                "",
            )
            .expect("insert batch");
        assert!(batch.id > 0);
        assert_eq!(batch.robot_serials.len(), 2);

        let fetched = store.get_batch(&batch.entity_id).expect("get batch");
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().name, "Batch 1");
    }

    #[test]
    fn test_update_batch() {
        let store = create_store();
        let batch = store
            .insert_batch("B1", &[], None, None, 0, "planned", "")
            .unwrap();
        let updated = store
            .update_batch(
                &batch.entity_id,
                Some("B2"),
                None,
                None,
                None,
                None,
                Some("ready"),
                None,
            )
            .expect("update batch");
        assert!(updated.is_some());
        assert_eq!(updated.unwrap().status, "ready");
    }

    #[test]
    fn test_list_batches() {
        let store = create_store();
        store
            .insert_batch("A", &[], None, None, 0, "planned", "")
            .unwrap();
        store
            .insert_batch("B", &[], None, None, 0, "planned", "")
            .unwrap();
        assert_eq!(store.list_batches().unwrap().len(), 2);
    }

    // ── Person tests ──

    #[test]
    fn test_insert_and_get_person() {
        let store = create_store();
        let person = store
            .insert_person(
                "John",
                true,
                &["welding".into(), "electrical".into()],
                &["Site A".into()],
                None,
                None,
                "",
            )
            .expect("insert person");
        assert!(person.is_senior);
        assert_eq!(person.skills.len(), 2);
        assert_eq!(person.special_sites.len(), 1);

        let fetched = store.get_person(&person.entity_id).expect("get person");
        assert!(fetched.is_some());
    }

    #[test]
    fn test_list_people() {
        let store = create_store();
        store
            .insert_person("A", false, &[], &[], None, None, "")
            .unwrap();
        store
            .insert_person("B", true, &[], &[], None, None, "")
            .unwrap();
        assert_eq!(store.list_people().unwrap().len(), 2);
    }

    // ── Inventory tests ──

    #[test]
    fn test_upsert_inventory_create() {
        let store = create_store();
        let item = store
            .upsert_inventory(
                "Hammer",
                Some("HAM-01"),
                Some("tool"),
                Some(10),
                Some("WH-A"),
                None,
                Some("Acme"),
                Some(5),
                Some(3),
                None,
            )
            .expect("upsert inventory");
        assert!(item.id > 0);
        assert_eq!(item.name, "Hammer");
        assert_eq!(item.qty_on_hand, 10);
    }

    #[test]
    fn test_upsert_inventory_update() {
        let store = create_store();
        store
            .upsert_inventory(
                "Hammer",
                Some("HAM-01"),
                Some("tool"),
                Some(10),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("create");
        let updated = store
            .upsert_inventory(
                "Hammer",
                None,
                None,
                Some(20),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("update");
        assert_eq!(updated.qty_on_hand, 20);
    }

    #[test]
    fn test_list_inventory() {
        let store = create_store();
        store
            .upsert_inventory(
                "A",
                None,
                None,
                Some(5),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .upsert_inventory(
                "B",
                None,
                None,
                Some(5),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(store.list_inventory().unwrap().len(), 2);
    }

    #[test]
    fn test_low_stock_filter() {
        let store = create_store();
        store
            .upsert_inventory(
                "LowItem",
                None,
                None,
                Some(2),
                None,
                None,
                None,
                None,
                Some(5),
                None,
            )
            .unwrap();
        store
            .upsert_inventory(
                "OkItem",
                None,
                None,
                Some(10),
                None,
                None,
                None,
                None,
                Some(5),
                None,
            )
            .unwrap();
        let low = store.list_inventory_low_stock().expect("low stock");
        assert_eq!(low.len(), 1);
        assert_eq!(low[0].name, "LowItem");
    }

    #[test]
    fn test_reserve_inventory() {
        let store = create_store();
        let item = store
            .upsert_inventory(
                "ReserveTest",
                None,
                None,
                Some(100),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("create");
        let reserved = store
            .reserve_inventory(&item.entity_id, 30, None)
            .expect("reserve");
        assert_eq!(reserved.qty_reserved, 30);

        // Try over-reserving
        let result = store.reserve_inventory(&item.entity_id, 80, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_reserve_inventory_not_found() {
        let store = create_store();
        let result = store.reserve_inventory("nonexistent", 10, None);
        assert!(result.is_err());
    }

    // ── Procurement tests ──

    #[test]
    fn test_insert_and_get_procurement_order() {
        let store = create_store();
        let po = store
            .insert_procurement_order(
                "SKU-001", "Item", "Supplier", 100, 14, None, "",
            )
            .expect("insert PO");
        assert!(po.id > 0);

        let fetched =
            store.get_procurement_order(&po.entity_id).expect("get PO");
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().item_name, "Item");
    }

    #[test]
    fn test_list_open_orders() {
        let store = create_store();
        let po1 = store
            .insert_procurement_order("S1", "I1", "Sup", 10, 7, None, "")
            .unwrap();
        let po2 = store
            .insert_procurement_order("S2", "I2", "Sup", 20, 7, None, "")
            .unwrap();
        store
            .update_procurement_order(
                &po2.entity_id,
                Some("delivered"),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let open = store.list_open_orders().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].entity_id, po1.entity_id);
    }

    // ── Alert tests ──

    #[test]
    fn test_alert_lifecycle() {
        let store = create_store();
        let alert = store
            .insert_alert(
                "low_stock",
                "urgent",
                "inventory_item",
                "inv-uuid",
                Some("2026-08-01"),
                "Below threshold",
                "Order more",
            )
            .expect("insert alert");
        assert!(!alert.resolved);

        let active = store.list_active_alerts().expect("active alerts");
        assert_eq!(active.len(), 1);

        let resolved = store.resolve_alert(&alert.entity_id).expect("resolve");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().resolved);

        let active_after =
            store.list_active_alerts().expect("active after resolve");
        assert_eq!(active_after.len(), 0);
    }

    // ── EventLog tests ──

    #[test]
    fn test_log_and_list_events() {
        let store = create_store();
        let id = store
            .log_event(
                "2026-07-01T12:00:00Z",
                "user",
                "created",
                "site",
                "site-uuid",
                "Created site Test",
            )
            .expect("log event");
        assert!(id > 0);

        let events = store.list_events(10).expect("list events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "user");
        assert_eq!(events[0].action_type, "created");
    }

    // ── Existence check tests ──

    #[test]
    fn test_site_exists() {
        let store = create_store();
        let site = store
            .insert_site("Exists", "", None, None, 0, false, "")
            .unwrap();
        assert!(store.site_exists_by_id(site.id).unwrap());
        assert!(!store.site_exists_by_id(999).unwrap());
    }

    // ── Batch-site FK test ──

    #[test]
    fn test_batch_with_site_reference() {
        let store = create_store();
        let site = store
            .insert_site("FK Site", "", None, None, 0, false, "")
            .unwrap();
        let batch = store
            .insert_batch(
                "FK Batch",
                &[],
                Some(site.id),
                None,
                0,
                "planned",
                "",
            )
            .expect("insert batch with site");
        assert_eq!(batch.target_site_id, Some(site.id));
        let fetched = store.get_batch(&batch.entity_id).expect("get");
        assert_eq!(fetched.unwrap().target_site_id, Some(site.id));
    }

    // ── Imported Google Sheets job tests ──

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
            team_member: "".into(),
            robots: "R1".into(),
            additional_info: "".into(),
            source_done: false,
            local_status: ImportedJobStatus::Active,
            row_hash: row_hash.into(),
            source_payload: serde_json::json!({"title": title}),
            import_run_id: None,
        }
    }

    #[test]
    fn test_upsert_imported_job_creates_and_gets() {
        let store = create_store();
        let created = store
            .upsert_imported_job(&job_input(
                "Site A",
                "Site A",
                "100 Bay St",
                "h1",
            ))
            .expect("upsert create");
        assert!(created.id > 0);
        assert!(!created.entity_id.is_empty());
        assert_eq!(created.external_job_key, "Site A");
        assert_eq!(created.local_status, ImportedJobStatus::Active);
        assert_eq!(created.start_date.as_deref(), Some("2026-08-05"));

        let fetched = store
            .get_imported_job("google_sheets", "Site A")
            .expect("get")
            .expect("job exists");
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.entity_id, created.entity_id);

        let missing = store
            .get_imported_job("google_sheets", "Nope")
            .expect("get missing");
        assert!(missing.is_none());
    }

    #[test]
    fn test_upsert_imported_job_updates_preserve_entity_id() {
        let store = create_store();
        let first = store
            .upsert_imported_job(&job_input(
                "Site A",
                "Site A",
                "100 Bay St",
                "h1",
            ))
            .expect("first upsert");
        let second = store
            .upsert_imported_job(&job_input(
                "Site A",
                "Site A (renamed)",
                "200 New Rd",
                "h2",
            ))
            .expect("second upsert");
        // Same stable identity, refreshed mapped fields.
        assert_eq!(second.id, first.id);
        assert_eq!(second.entity_id, first.entity_id);
        assert_eq!(second.title, "Site A (renamed)");
        assert_eq!(second.location, "200 New Rd");
        assert_eq!(second.row_hash, "h2");
        assert_eq!(second.local_status, ImportedJobStatus::Active);
        // first_seen_at is preserved on update.
        assert_eq!(second.first_seen_at, first.first_seen_at);
    }

    #[test]
    fn test_list_imported_jobs_and_by_status() {
        let store = create_store();
        store
            .upsert_imported_job(&job_input("A", "A", "loc", "h1"))
            .unwrap();
        store
            .upsert_imported_job(&job_input("B", "B", "loc", "h2"))
            .unwrap();
        store
            .mark_imported_job_completed("google_sheets", "B", None)
            .unwrap();

        let all = store.list_imported_jobs("google_sheets").unwrap();
        assert_eq!(all.len(), 2);

        let active = store
            .list_imported_jobs_by_status(
                "google_sheets",
                ImportedJobStatus::Active,
            )
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].external_job_key, "A");

        let completed = store
            .list_imported_jobs_by_status(
                "google_sheets",
                ImportedJobStatus::Completed,
            )
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].external_job_key, "B");
    }

    #[test]
    fn test_mark_imported_job_completed_and_ignored() {
        let store = create_store();
        store
            .upsert_imported_job(&job_input("Site A", "Site A", "loc", "h1"))
            .unwrap();

        let completed = store
            .mark_imported_job_completed(
                "google_sheets",
                "Site A",
                Some("run-1"),
            )
            .expect("complete")
            .expect("job exists");
        assert!(completed.source_done);
        assert_eq!(completed.local_status, ImportedJobStatus::Completed);
        assert_eq!(completed.last_import_run_id.as_deref(), Some("run-1"));

        // Job still exists after completion (no deletion).
        let still_there =
            store.get_imported_job("google_sheets", "Site A").unwrap();
        assert!(still_there.is_some());

        let ignored = store
            .mark_imported_job_ignored("google_sheets", "Site A")
            .expect("ignore")
            .expect("job exists");
        assert_eq!(ignored.local_status, ImportedJobStatus::Ignored);

        // Missing keys return None.
        assert!(
            store
                .mark_imported_job_completed("google_sheets", "Nope", None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_imported_job_never_deleted_when_missing() {
        let store = create_store();
        store
            .upsert_imported_job(&job_input("Gone", "Gone", "loc", "h1"))
            .unwrap();
        // A later read simply has no create/update/complete for the job; the
        // store must not delete it. There is no delete API, so list must still
        // contain it after any import bookkeeping.
        let all = store.list_imported_jobs("google_sheets").unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].external_job_key, "Gone");
    }

    // ── Import run tests ──

    fn run_input(
        id: &str,
        dry_run: bool,
        status: ImportRunStatus,
    ) -> ImportRunInput {
        ImportRunInput {
            id: id.into(),
            source: "google_sheets".into(),
            spreadsheet_id: "spreadsheet-1".into(),
            sheet_name: "US".into(),
            triggered_by: "manual".into(),
            dry_run,
            status,
            preview_hash: "hash-123".into(),
            seen_count: 5,
            created_count: 1,
            updated_count: 1,
            unchanged_count: 2,
            conflict_count: 1,
            invalid_count: 0,
            skipped_count: 0,
            summary: serde_json::json!({"creates": 1}),
        }
    }

    #[test]
    fn test_create_and_read_import_run() {
        let store = create_store();
        let rec = store
            .create_import_run(&run_input(
                "run-1",
                true,
                ImportRunStatus::Previewed,
            ))
            .expect("create run");
        assert_eq!(rec.id, "run-1");
        assert!(rec.dry_run);
        assert_eq!(rec.status, ImportRunStatus::Previewed);
        assert_eq!(rec.created_count, 1);
        assert_eq!(rec.summary["creates"], 1);

        let fetched =
            store.get_import_run("run-1").unwrap().expect("run exists");
        assert_eq!(fetched.id, "run-1");

        assert!(store.get_import_run("nope").unwrap().is_none());
    }

    #[test]
    fn test_list_import_runs_newest_first() {
        let store = create_store();
        store
            .create_import_run(&run_input(
                "run-1",
                false,
                ImportRunStatus::Applied,
            ))
            .unwrap();
        store
            .create_import_run(&run_input(
                "run-2",
                false,
                ImportRunStatus::Applied,
            ))
            .unwrap();
        let runs = store.list_import_runs("google_sheets", 10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "run-2");
        assert_eq!(runs[1].id, "run-1");

        let limited = store.list_import_runs("google_sheets", 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, "run-2");
    }

    // ── Import conflict tests ──

    #[test]
    fn test_insert_list_and_resolve_import_conflict() {
        let store = create_store();
        let conflict = store
            .insert_import_conflict(&ImportConflictInput {
                import_run_id: Some("run-1".into()),
                source: "google_sheets".into(),
                external_job_key: "Shared title".into(),
                conflict_type: ImportConflictType::DuplicateTitle,
                reason: "Two rows share this title".into(),
                details: serde_json::json!({"row_numbers": [1, 2]}),
            })
            .expect("insert conflict");
        assert_eq!(conflict.status, ImportConflictStatus::Open);
        assert_eq!(conflict.details["row_numbers"][0], 1);

        let open = store
            .list_import_conflicts("google_sheets", ImportConflictStatus::Open)
            .unwrap();
        assert_eq!(open.len(), 1);

        let resolved = store
            .resolve_import_conflict(conflict.id)
            .expect("resolve")
            .expect("conflict exists");
        assert_eq!(resolved.status, ImportConflictStatus::Resolved);
        assert!(resolved.resolved_at.is_some());

        let open_after = store
            .list_import_conflicts("google_sheets", ImportConflictStatus::Open)
            .unwrap();
        assert!(open_after.is_empty());

        // Resolving again is a no-op (already resolved).
        assert!(
            store
                .resolve_import_conflict(conflict.id)
                .unwrap()
                .is_none()
        );
    }

    // ── Atomic apply tests ──

    #[test]
    fn test_apply_import_atomic() {
        let store = create_store();
        let run = run_input("run-apply", false, ImportRunStatus::Applied);
        let creates = vec![job_input("New A", "New A", "loc1", "ha")];
        let updates = vec![job_input("Existing", "Existing v2", "loc2", "hb")];
        let complete_keys = vec!["Done job".to_string()];
        let conflicts = vec![ImportConflictInput {
            import_run_id: Some("run-apply".into()),
            source: "google_sheets".into(),
            external_job_key: "Shared".into(),
            conflict_type: ImportConflictType::DuplicateTitle,
            reason: "dup".into(),
            details: serde_json::json!({"row_numbers": [1, 2]}),
        }];

        // Seed one existing job and one job to be completed.
        store
            .upsert_imported_job(&job_input(
                "Existing",
                "Existing v1",
                "loc",
                "h0",
            ))
            .unwrap();
        store
            .upsert_imported_job(&job_input(
                "Done job", "Done job", "loc", "h0",
            ))
            .unwrap();

        let record = store
            .apply_import(&run, &creates, &updates, &complete_keys, &conflicts)
            .expect("apply import");
        assert_eq!(record.id, "run-apply");
        assert_eq!(record.status, ImportRunStatus::Applied);
        assert!(!record.dry_run);

        // New job created.
        let new_job = store
            .get_imported_job("google_sheets", "New A")
            .unwrap()
            .unwrap();
        assert_eq!(new_job.title, "New A");
        // Existing job updated, identity preserved.
        let updated_job = store
            .get_imported_job("google_sheets", "Existing")
            .unwrap()
            .unwrap();
        assert_eq!(updated_job.title, "Existing v2");
        // Done job completed from source.
        let done_job = store
            .get_imported_job("google_sheets", "Done job")
            .unwrap()
            .unwrap();
        assert!(done_job.source_done);
        assert_eq!(done_job.local_status, ImportedJobStatus::Completed);
        assert_eq!(done_job.last_import_run_id.as_deref(), Some("run-apply"));
        // Conflicts recorded.
        let open = store
            .list_import_conflicts("google_sheets", ImportConflictStatus::Open)
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].external_job_key, "Shared");
        // Run persisted.
        assert!(store.get_import_run("run-apply").unwrap().is_some());
    }
}
