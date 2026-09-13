//! PM (project management) module for the mote agent platform.
//!
//! This module provides the agent loop with PM-specific tools, persistence,
//! business rules, and alerting.
//!
//! # Architecture
//!
//! - `schema` — SQLite schema bootstrap / migrations
//! - `store` — SQLite persistence layer
//! - `service` — Business rules and validation
//! - `imports` — Title-keyed Google Sheets import diff/preview/apply
//! - `google_sheets` — Read-only Google Sheets sync foundation (service-account auth, fetch, normalize)
//! - `google_sheets_tools` — PM agent tools for manual preview/apply/list of sheet imports
//! - `events` — Audit log helpers
//! - `tools` — PM tool adapters for the agent loop
//! - `alerts` — Lead-time and blocker engine

pub mod alerts;
pub mod events;
pub mod google_sheets;
pub mod google_sheets_tools;
pub mod imports;
pub mod schema;
pub mod service;
pub mod store;
pub mod tools;

use std::sync::Arc;

use std::path::PathBuf;

use anyhow::{Context, Result};
pub use service::PmService;
pub use store::Store;

/// Import source key used by the Google Sheets import tools and store.
pub const GOOGLE_SHEETS_IMPORT_SOURCE: &str = "google_sheets";

/// PM runtime context holding the SQLite store.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PmContext {
    /// Persistent store for PM entities.
    pub store: Store,
}

impl PmContext {
    /// Open or create the PM database and bootstrap the schema.
    ///
    /// Creates the database directory if it does not exist, opens (or creates)
    /// the SQLite file, and runs idempotent schema bootstrap.
    #[allow(dead_code)]
    pub fn open(db_dir: PathBuf) -> Result<PmContext> {
        std::fs::create_dir_all(&db_dir).with_context(|| {
            format!("creating PM db dir: {}", db_dir.display())
        })?;
        let db_path = db_dir.join("pm.db");
        let conn = rusqlite::Connection::open(&db_path).with_context(|| {
            format!("opening PM database at {}", db_path.display())
        })?;
        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        // Enable foreign key enforcement
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        // Bootstrap schema
        schema::bootstrap(&conn).context("bootstrapping PM schema")?;
        let store = Store::new(conn);
        Ok(Self { store })
    }
}

#[allow(dead_code)]
impl PmContext {
    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<PmContext> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        schema::bootstrap(&conn)?;
        let store = Store::new(conn);
        Ok(Self { store })
    }
}

#[allow(dead_code)]
impl PmContext {
    /// Backward-compatible stub for the old placeholder API.
    pub fn ensure_db(&self) -> Result<(), String> {
        Ok(())
    }
}

/// List of all PM tool names.
///
/// Used by tool registration in `main.rs` to augment the tool set for the
/// `pm` agent. Tool names must match the `Tool::def().function.name` of
/// each tool implementation in `server/src/pm/tools.rs`.
#[allow(dead_code)]
pub const PM_TOOL_NAMES: &[&str] = &[
    "pm_get_status",
    "pm_list_sites",
    "pm_list_batches",
    "pm_list_people",
    "pm_list_inventory",
    "pm_list_procurement_orders",
    "pm_get_timing_plan",
    "pm_create_or_update_site",
    "pm_create_or_update_batch",
    "pm_assign_person",
    "pm_record_handover",
    "pm_upsert_inventory",
    "pm_reserve_inventory",
    "pm_create_procurement_order",
    "pm_check_deploy_readiness",
    "pm_check_alerts",
    "pm_list_blockers",
    "pm_preview_google_sheet_site_jobs_import",
    "pm_apply_google_sheet_site_jobs_import",
    "pm_list_google_sheet_import_conflicts",
    "pm_list_imported_google_sheet_jobs",
];

/// Build the PM tool set backed by the real service layer.
///
/// `gs_config` carries the optional `[google_sheets]` config into the import
/// tools; it only ever holds a credential *path*, never credential contents.
pub fn pm_tools(
    ctx: PmContext,
    gs_config: crate::config::GoogleSheetsConfig,
) -> Vec<Box<dyn crate::llm::Tool>> {
    let service = Arc::new(PmService::new(ctx.store));
    let mut tools: Vec<Box<dyn crate::llm::Tool>> = vec![
        Box::new(tools::PmGetStatusTool::new(Arc::clone(&service))),
        Box::new(tools::PmListSitesTool::new(Arc::clone(&service))),
        Box::new(tools::PmListBatchesTool::new(Arc::clone(&service))),
        Box::new(tools::PmListPeopleTool::new(Arc::clone(&service))),
        Box::new(tools::PmListInventoryTool::new(Arc::clone(&service))),
        Box::new(tools::PmListProcurementOrdersTool::new(Arc::clone(
            &service,
        ))),
        Box::new(tools::PmGetTimingPlanTool::new(Arc::clone(&service))),
        Box::new(tools::PmCreateOrUpdateSiteTool::new(Arc::clone(&service))),
        Box::new(tools::PmCreateOrUpdateBatchTool::new(Arc::clone(&service))),
        Box::new(tools::PmAssignPersonTool::new(Arc::clone(&service))),
        Box::new(tools::PmRecordHandoverTool::new(Arc::clone(&service))),
        Box::new(tools::PmUpsertInventoryTool::new(Arc::clone(&service))),
        Box::new(tools::PmReserveInventoryTool::new(Arc::clone(&service))),
        Box::new(tools::PmCreateProcurementOrderTool::new(Arc::clone(
            &service,
        ))),
        Box::new(tools::PmCheckDeployReadinessTool::new(Arc::clone(&service))),
        Box::new(tools::PmCheckAlertsTool::new(Arc::clone(&service))),
        Box::new(tools::PmListBlockersTool::new(Arc::clone(&service))),
    ];
    tools.extend(google_sheets_tools::google_sheets_tools(service, gs_config));
    tools
}
