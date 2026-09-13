//! Shared PM protocol/domain types.
//!
//! Serde-only types shared across server PM modules. No tokio or server-only
//! imports — these are transport-safe domain concepts used by PM tool inputs,
//! outputs, and event logging.

use serde::{Deserialize, Serialize};

use crate::types::HistoryMessage;

// ── Entity IDs ────────────────────────────────────────────

/// Canonical ID type for PM entities (sites, batches, people, etc.).
/// Stored as a simple string in the transport layer.
pub type EntityId = String;

// ── Status enums ──────────────────────────────────────────

/// Status of a batch/robot deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchStatus {
    /// Planned but not yet in preparation.
    Planned,
    /// Preparation in progress (inventory, staffing, files).
    InPreparation,
    /// Ready for deployment.
    Ready,
    /// Currently deployed / onsite.
    Deployed,
    /// Completed and demobilized.
    Completed,
    /// Cancelled / no longer needed.
    Cancelled,
}

/// Status of an inventory item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InventoryStatus {
    /// In stock and available.
    Available,
    /// Low stock (below reorder threshold).
    LowStock,
    /// Out of stock.
    OutOfStock,
    /// Discontinued / no longer tracked.
    Discontinued,
}

/// Status of a procurement order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcurementStatus {
    /// Ordered but awaiting dispatch.
    Pending,
    /// Shipped / in transit.
    Shipped,
    /// Delivered and received.
    Delivered,
    /// Cancelled.
    Cancelled,
}

/// Severity level for alerts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    /// Past the due date or deadline.
    Overdue,
    /// Imminent deadline within lead-time window.
    Urgent,
    /// Approaching but not yet urgent.
    Upcoming,
    /// No action needed.
    Ok,
}

/// Physical location of inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryLocation {
    /// Warehouse / storage area name.
    pub area: String,
    /// Optional rack or shelf identifier.
    #[serde(default)]
    pub rack: Option<String>,
}

impl std::fmt::Display for InventoryLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.rack {
            Some(rack) => write!(f, "{}/{}", self.area, rack),
            None => write!(f, "{}", self.area),
        }
    }
}

/// Source of an audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSource {
    /// Direct user command via chat.
    User,
    /// Automated sync from external system (Google Sheets, shared folder).
    Sync,
    /// System-generated (alert engine, scheduler).
    System,
}

/// Action type for audit events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventAction {
    /// Entity was created.
    Created,
    /// Entity was updated.
    Updated,
    /// Entity was deleted.
    Deleted,
    /// Entity was assigned.
    Assigned,
    /// Inventory was reserved.
    Reserved,
    /// Inventory reservation was released.
    Released,
    /// Procurement order was placed.
    Ordered,
    /// Alert was generated.
    AlertGenerated,
    /// Alert was resolved.
    AlertResolved,
    /// External data was synced.
    Synced,
}

/// A single audit event entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventLogEntry {
    /// ISO-8601 timestamp.
    pub timestamp: String,
    /// Source of the event.
    pub source: EventSource,
    /// Action performed.
    pub action: EventAction,
    /// Entity type (e.g., "site", "batch", "inventory_item").
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Optional summary of what changed.
    #[serde(default)]
    pub summary: Option<String>,
}

// ── Persisted record types ───────────────────────────────────
//
// These map 1:1 to rows in the SQLite tables. Internal fields (`id`) are
// server-side integer primary keys; `entity_id` is the stable public UUID.

/// A site (deployment location).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteRecord {
    pub id: i64,
    pub entity_id: EntityId,
    pub name: String,
    pub location: String,
    pub deploy_window_start: Option<String>,
    pub deploy_window_end: Option<String>,
    pub onsite_duration_days: i32,
    pub files_confirmed: bool,
    pub notes: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A batch (deployment batch with robot assignments).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRecord {
    pub id: i64,
    pub entity_id: EntityId,
    pub name: String,
    pub robot_serials: Vec<String>,
    pub target_site_id: Option<i64>,
    pub target_deploy_date: Option<String>,
    pub onsite_duration_days: i32,
    pub status: String,
    pub notes: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A person (team member).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonRecord {
    pub id: i64,
    pub entity_id: EntityId,
    pub name: String,
    pub is_senior: bool,
    pub skills: Vec<String>,
    pub special_sites: Vec<String>,
    pub availability_start: Option<String>,
    pub availability_end: Option<String>,
    pub notes: String,
    pub created_at: String,
    pub updated_at: String,
}

/// An assignment linking a person to a site or batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentRecord {
    pub id: i64,
    pub entity_id: EntityId,
    pub person_id: i64,
    pub site_id: Option<i64>,
    pub batch_id: Option<i64>,
    pub role: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub handover_to_person_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

/// An inventory item (supplies and equipment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryItemRecord {
    pub id: i64,
    pub entity_id: EntityId,
    pub sku: String,
    pub name: String,
    pub category: String,
    pub qty_on_hand: i32,
    pub qty_reserved: i32,
    pub location_area: String,
    pub location_rack: String,
    pub supplier: String,
    pub lead_time_days: i32,
    pub reorder_threshold: i32,
    pub status: String,
    pub linked_batch_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

/// A procurement order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcurementOrderRecord {
    pub id: i64,
    pub entity_id: EntityId,
    pub order_ref: String,
    pub item_sku: String,
    pub item_name: String,
    pub supplier: String,
    pub qty: i32,
    pub lead_time_days: i32,
    pub order_date: Option<String>,
    pub eta: Option<String>,
    pub status: String,
    pub linked_batch_id: Option<i64>,
    pub notes: String,
    pub created_at: String,
    pub updated_at: String,
}

/// An alert (actionable notification).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: i64,
    pub entity_id: EntityId,
    pub alert_type: String,
    pub severity: String,
    pub related_entity_type: String,
    pub related_entity_id: String,
    pub due_date: Option<String>,
    pub reason: String,
    pub suggested_action: String,
    pub resolved: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// An event log entry (audit trail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogRecord {
    pub id: i64,
    pub timestamp: String,
    pub source: String,
    pub action_type: String,
    pub entity_type: String,
    pub entity_id: String,
    pub summary: String,
    pub created_at: String,
}

// ── Imported Google Sheets jobs ─────────────────────────────────
//
// These types back the title-keyed Google Sheets import feature. An imported
// job is keyed by the exact `Title of Activity` cell (`external_job_key`)
// scoped per `source`, so the same sheet title maps to exactly one local job.

/// Lifecycle status of an imported job in the local PM store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportedJobStatus {
    /// Active — currently open in the source sheet.
    Active,
    /// Completed — the source marked the row Done (or it was completed).
    Completed,
    /// Ignored — excluded from local planning.
    Ignored,
}

/// Status of a Google Sheets import run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportRunStatus {
    /// Previewed — dry-run classification produced, nothing mutated.
    Previewed,
    /// Applied — mutations were committed to the store.
    Applied,
    /// Failed — the apply aborted.
    Failed,
}

/// Review status of an import conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportConflictStatus {
    /// Open — awaiting operator review.
    Open,
    /// Resolved — the operator marked it resolved.
    Resolved,
}

/// Kind of import conflict recorded for operator review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportConflictType {
    /// An eligible row had a blank title key.
    BlankTitle,
    /// Two or more eligible rows shared the same title.
    DuplicateTitle,
    /// An eligible row could not be normalized (e.g. unparseable date).
    InvalidRow,
    /// A completed/ignored job reappeared as active.
    ReopenCompleted,
}

/// A row in the `imported_jobs` table.
///
/// `external_job_key` is the exact source title and is UNIQUE per `source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedJobRecord {
    pub id: i64,
    /// Stable local UUID; preserved across source updates.
    pub entity_id: EntityId,
    /// Import source, e.g. `google_sheets`.
    pub source: String,
    /// Spreadsheet provenance.
    pub spreadsheet_id: String,
    /// Tab provenance.
    pub sheet_name: String,
    /// Exact `Title of Activity` cell — the stable external key.
    pub external_job_key: String,
    /// Display title (same as the key for Google Sheets).
    pub title: String,
    pub location: String,
    pub activity_type: String,
    /// Start date `YYYY-MM-DD` (nullable when the source cell is blank).
    pub start_date: Option<String>,
    /// End date `YYYY-MM-DD` (nullable when the source cell is blank).
    pub end_date: Option<String>,
    pub job_leader: String,
    pub team_member: String,
    pub robots: String,
    pub additional_info: String,
    /// Whether the source row is currently marked Done.
    pub source_done: bool,
    /// Local lifecycle status.
    pub local_status: ImportedJobStatus,
    /// Stable hash of the mapped source fields (change detection).
    pub row_hash: String,
    /// Raw normalized source payload JSON (provenance).
    pub source_payload: serde_json::Value,
    pub first_seen_at: String,
    pub last_seen_at: String,
    /// Import run that last wrote this row.
    pub last_import_run_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A row in the `import_runs` table (one preview or apply).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRunRecord {
    /// UUID/string primary key.
    pub id: String,
    pub source: String,
    pub spreadsheet_id: String,
    pub sheet_name: String,
    /// Who triggered the run, e.g. `manual`.
    pub triggered_by: String,
    /// Whether the run only previewed without mutating.
    pub dry_run: bool,
    pub status: ImportRunStatus,
    /// Stable hash of the classified diff.
    pub preview_hash: String,
    pub seen_count: i64,
    pub created_count: i64,
    pub updated_count: i64,
    pub unchanged_count: i64,
    pub conflict_count: i64,
    pub invalid_count: i64,
    pub skipped_count: i64,
    /// Free-form summary JSON.
    pub summary: serde_json::Value,
    pub created_at: String,
    pub completed_at: Option<String>,
}

/// A row in the `import_conflicts` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportConflictRecord {
    pub id: i64,
    /// Import run that recorded the conflict, when known.
    pub import_run_id: Option<String>,
    pub source: String,
    /// External job key involved; empty for row-level normalization errors.
    pub external_job_key: String,
    pub conflict_type: ImportConflictType,
    pub reason: String,
    /// Extra detail JSON (row numbers, incoming values, ...).
    pub details: serde_json::Value,
    pub status: ImportConflictStatus,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

// ── Imported Google Sheets job inputs ───────────────────────────

/// Input for upserting one imported job, keyed by `(source, external_job_key)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedJobInput {
    pub source: String,
    pub spreadsheet_id: String,
    pub sheet_name: String,
    /// Exact source title — the stable external key.
    pub external_job_key: String,
    pub title: String,
    pub location: String,
    pub activity_type: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub job_leader: String,
    pub team_member: String,
    pub robots: String,
    pub additional_info: String,
    pub source_done: bool,
    pub local_status: ImportedJobStatus,
    /// Stable hash of the mapped source fields.
    pub row_hash: String,
    /// Raw normalized source payload JSON (provenance).
    pub source_payload: serde_json::Value,
    /// Import run that wrote this row, when known.
    pub import_run_id: Option<String>,
}

/// Input for recording an import run (preview or apply bookkeeping).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRunInput {
    pub id: String,
    pub source: String,
    pub spreadsheet_id: String,
    pub sheet_name: String,
    pub triggered_by: String,
    pub dry_run: bool,
    pub status: ImportRunStatus,
    pub preview_hash: String,
    pub seen_count: i64,
    pub created_count: i64,
    pub updated_count: i64,
    pub unchanged_count: i64,
    pub conflict_count: i64,
    pub invalid_count: i64,
    pub skipped_count: i64,
    pub summary: serde_json::Value,
}

/// Input for recording an import conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportConflictInput {
    pub import_run_id: Option<String>,
    pub source: String,
    pub external_job_key: String,
    pub conflict_type: ImportConflictType,
    pub reason: String,
    pub details: serde_json::Value,
}

// ── Input / mutation structs ─────────────────────────────────

/// Input for creating a new site.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSiteInput {
    pub name: String,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub deploy_window_start: Option<String>,
    #[serde(default)]
    pub deploy_window_end: Option<String>,
    #[serde(default)]
    pub onsite_duration_days: Option<i32>,
    #[serde(default)]
    pub files_confirmed: Option<bool>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Input for updating an existing site.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSiteInput {
    pub entity_id: EntityId,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub deploy_window_start: Option<String>,
    #[serde(default)]
    pub deploy_window_end: Option<String>,
    #[serde(default)]
    pub onsite_duration_days: Option<i32>,
    #[serde(default)]
    pub files_confirmed: Option<bool>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Input for creating a new batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBatchInput {
    pub name: String,
    #[serde(default)]
    pub robot_serials: Option<Vec<String>>,
    #[serde(default)]
    pub target_site_id: Option<i64>,
    #[serde(default)]
    pub target_deploy_date: Option<String>,
    #[serde(default)]
    pub onsite_duration_days: Option<i32>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Input for updating an existing batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateBatchInput {
    pub entity_id: EntityId,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub robot_serials: Option<Vec<String>>,
    #[serde(default)]
    pub target_site_id: Option<i64>,
    #[serde(default)]
    pub target_deploy_date: Option<String>,
    #[serde(default)]
    pub onsite_duration_days: Option<i32>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Input for creating a new person.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePersonInput {
    pub name: String,
    #[serde(default)]
    pub is_senior: Option<bool>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub special_sites: Option<Vec<String>>,
    #[serde(default)]
    pub availability_start: Option<String>,
    #[serde(default)]
    pub availability_end: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Input for updating an existing person.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePersonInput {
    pub entity_id: EntityId,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub is_senior: Option<bool>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub special_sites: Option<Vec<String>>,
    #[serde(default)]
    pub availability_start: Option<String>,
    #[serde(default)]
    pub availability_end: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Input for upserting inventory by SKU/name (create or update).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertInventoryInput {
    pub name: String,
    #[serde(default)]
    pub sku: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub qty_on_hand: Option<i32>,
    #[serde(default)]
    pub location_area: Option<String>,
    #[serde(default)]
    pub location_rack: Option<String>,
    #[serde(default)]
    pub supplier: Option<String>,
    #[serde(default)]
    pub lead_time_days: Option<i32>,
    #[serde(default)]
    pub reorder_threshold: Option<i32>,
    #[serde(default)]
    pub linked_batch_id: Option<i64>,
}

/// Input for reserving inventory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReserveInventoryInput {
    pub item_entity_id: EntityId,
    pub quantity: i32,
    #[serde(default)]
    pub linked_batch_id: Option<i64>,
}

/// Input for creating a procurement order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProcurementOrderInput {
    pub item_sku: String,
    pub item_name: String,
    pub supplier: String,
    pub qty: i32,
    #[serde(default)]
    pub lead_time_days: Option<i32>,
    #[serde(default)]
    pub linked_batch_id: Option<i64>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Input for updating a procurement order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProcurementOrderInput {
    pub entity_id: EntityId,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub eta: Option<String>,
    #[serde(default)]
    pub order_ref: Option<String>,
    #[serde(default)]
    pub supplier: Option<String>,
    #[serde(default)]
    pub qty: Option<i32>,
    #[serde(default)]
    pub notes: Option<String>,
}

// ── Alert generation summary ─────────────────────────────────

/// Summary of alert generation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertGenerationSummary {
    /// Number of inventory alerts created or updated.
    pub inventory_alerts: usize,
    /// Number of procurement alerts created or updated.
    pub procurement_alerts: usize,
    /// Total alerts touched.
    pub total: usize,
}

// ── Summary DTOs used by tool outputs ─────────────────────

/// Concise summary of a site for tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteSummary {
    pub id: EntityId,
    pub name: String,
    pub status: String,
}

/// Concise summary of a batch for tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchSummary {
    pub id: EntityId,
    pub name: String,
    pub target_site: String,
    pub deploy_date: String,
    pub status: String,
    pub blocked: bool,
}

/// Concise summary of a person for tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonSummary {
    pub id: EntityId,
    pub name: String,
    pub role: String,
    pub available: bool,
}

/// Concise summary of an inventory item for tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventorySummary {
    pub id: EntityId,
    pub name: String,
    pub quantity_on_hand: i32,
    pub quantity_reserved: i32,
    pub location: String,
    pub status: String,
}

/// Concise summary of a procurement order for tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcurementSummary {
    pub id: EntityId,
    pub item_name: String,
    pub quantity: i32,
    pub status: String,
    pub eta: Option<String>,
}

/// Concise summary of an alert for tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertSummary {
    pub id: EntityId,
    pub severity: AlertSeverity,
    pub category: String,
    pub message: String,
    pub related_entity: String,
    pub due_date: Option<String>,
    pub resolved: bool,
}

// ── Status / readiness / blocker DTOs ────────────────────────

/// Status summary for pm_get_status tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSummary {
    /// Active (unresolved) alerts grouped by severity.
    pub active_alerts: Vec<AlertRecord>,
    /// Batches that are blocked (status is not ready/deployed/completed).
    pub blocked_batches: Vec<BatchRecord>,
    /// Inventory items below or at reorder threshold.
    pub low_stock_items: Vec<InventoryItemRecord>,
    /// Procurement orders that are not yet delivered or cancelled.
    pub pending_orders: Vec<ProcurementOrderRecord>,
    /// Total number of sites.
    pub total_sites: usize,
    /// Total number of people (team members).
    pub total_people: usize,
    /// Recent event log entries (last 20).
    pub recent_events: Vec<EventLogRecord>,
}

/// Readiness report for a single batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessReport {
    pub batch_entity_id: String,
    pub batch_name: String,
    pub status: String,
    pub is_ready: bool,
    pub blockers: Vec<BlockerInfo>,
}

/// Information about a single blocker condition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerInfo {
    pub batch_entity_id: String,
    pub batch_name: String,
    pub reason: String,
    pub severity: String,
}

// ── PM bridge transport types ─────────────────────────────

/// Local PM-only bridge request used by external adapters such as the
/// DingTalk sidecar. The server always routes this to the `pm` agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PmBridgeChatRequest {
    /// Latest user message to send to the PM agent.
    pub message: String,
    /// Prior visible conversation history maintained by the external adapter.
    #[serde(default)]
    pub history: Vec<HistoryMessage>,
    /// Stable runtime session key used for server-side mutable state scoping.
    pub runtime_session_key: String,
}

/// Final text response returned by the local PM-only bridge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PmBridgeChatResponse {
    pub reply: String,
    pub tokens_input: u64,
    pub tokens_output: u64,
    /// Terminal status of the PM run.
    pub status: String,
}

/// Input for assign_person tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignPersonInput {
    pub person_entity_id: String,
    pub entity_type: String,
    pub entity_id: String,
    pub role: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
}

/// Input for record_handover tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordHandoverInput {
    pub entity_type: String,
    pub entity_id: String,
    pub from_person_entity_id: String,
    pub to_person_entity_id: String,
    pub handover_date: Option<String>,
    pub notes: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_status_roundtrip() {
        let statuses = [
            BatchStatus::Planned,
            BatchStatus::InPreparation,
            BatchStatus::Ready,
            BatchStatus::Deployed,
            BatchStatus::Completed,
            BatchStatus::Cancelled,
        ];
        for s in &statuses {
            let json = serde_json::to_string(s).unwrap();
            let back: BatchStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(*s, back);
        }
    }

    #[test]
    fn test_inventory_status_roundtrip() {
        let json = serde_json::to_string(&InventoryStatus::Available).unwrap();
        assert_eq!(json, "\"available\"");
        let back: InventoryStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, InventoryStatus::Available);
    }

    #[test]
    fn test_alert_severity_roundtrip() {
        let json = serde_json::to_string(&AlertSeverity::Overdue).unwrap();
        assert_eq!(json, "\"overdue\"");
        let back: AlertSeverity = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AlertSeverity::Overdue);
    }

    #[test]
    fn test_inventory_location_display() {
        let loc = InventoryLocation {
            area: "Warehouse A".into(),
            rack: Some("R12".into()),
        };
        assert_eq!(loc.to_string(), "Warehouse A/R12");
        let loc2 = InventoryLocation {
            area: "Shelf 3".into(),
            rack: None,
        };
        assert_eq!(loc2.to_string(), "Shelf 3");
    }

    #[test]
    fn test_inventory_location_roundtrip() {
        let loc = InventoryLocation {
            area: "Main".into(),
            rack: Some("A1".into()),
        };
        let json = serde_json::to_string(&loc).unwrap();
        let back: InventoryLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(loc, back);
    }

    #[test]
    fn test_event_source_roundtrip() {
        let json = serde_json::to_string(&EventSource::User).unwrap();
        let back: EventSource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, EventSource::User);
    }

    #[test]
    fn test_event_action_roundtrip() {
        let json = serde_json::to_string(&EventAction::Created).unwrap();
        assert_eq!(json, "\"created\"");
        let back: EventAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, EventAction::Created);
    }

    #[test]
    fn test_procurement_status_roundtrip() {
        let json =
            serde_json::to_string(&ProcurementStatus::Delivered).unwrap();
        assert_eq!(json, "\"delivered\"");
        let back: ProcurementStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ProcurementStatus::Delivered);
    }

    #[test]
    fn test_site_record_roundtrip() {
        let rec = SiteRecord {
            id: 1,
            entity_id: "site-uuid".into(),
            name: "Site A".into(),
            location: "Warehouse 1".into(),
            deploy_window_start: Some("2026-08-01".into()),
            deploy_window_end: Some("2026-08-15".into()),
            onsite_duration_days: 14,
            files_confirmed: true,
            notes: "Main site".into(),
            created_at: "2026-07-01T00:00:00Z".into(),
            updated_at: "2026-07-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: SiteRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec.id, back.id);
        assert_eq!(rec.entity_id, back.entity_id);
        assert_eq!(rec.name, back.name);
        assert!(back.files_confirmed);
    }

    #[test]
    fn test_batch_record_roundtrip() {
        let rec = BatchRecord {
            id: 1,
            entity_id: "batch-uuid".into(),
            name: "Batch 12".into(),
            robot_serials: vec!["R001".into(), "R002".into()],
            target_site_id: Some(1),
            target_deploy_date: Some("2026-08-18".into()),
            onsite_duration_days: 6,
            status: "planned".into(),
            notes: "".into(),
            created_at: "2026-07-01T00:00:00Z".into(),
            updated_at: "2026-07-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: BatchRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec.name, back.name);
        assert_eq!(back.robot_serials.len(), 2);
    }

    #[test]
    fn test_person_record_roundtrip() {
        let rec = PersonRecord {
            id: 1,
            entity_id: "person-uuid".into(),
            name: "John".into(),
            is_senior: true,
            skills: vec!["welding".into(), "electrical".into()],
            special_sites: vec!["Site A".into()],
            availability_start: Some("2026-08-01".into()),
            availability_end: Some("2026-09-01".into()),
            notes: "".into(),
            created_at: "2026-07-01T00:00:00Z".into(),
            updated_at: "2026-07-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: PersonRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec.name, back.name);
        assert!(back.is_senior);
        assert_eq!(back.skills.len(), 2);
    }

    #[test]
    fn test_assignment_record_roundtrip() {
        let rec = AssignmentRecord {
            id: 1,
            entity_id: "assign-uuid".into(),
            person_id: 1,
            site_id: Some(1),
            batch_id: None,
            role: "lead".into(),
            start_date: Some("2026-08-18".into()),
            end_date: Some("2026-08-24".into()),
            handover_to_person_id: Some(2),
            created_at: "2026-07-01T00:00:00Z".into(),
            updated_at: "2026-07-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: AssignmentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, "lead");
        assert_eq!(back.handover_to_person_id, Some(2));
    }

    #[test]
    fn test_inventory_item_record_roundtrip() {
        let rec = InventoryItemRecord {
            id: 1,
            entity_id: "inv-uuid".into(),
            sku: "DB-3MM".into(),
            name: "Drillbit 3mm".into(),
            category: "consumable".into(),
            qty_on_hand: 50,
            qty_reserved: 10,
            location_area: "Warehouse A".into(),
            location_rack: "R12".into(),
            supplier: "ToolCo".into(),
            lead_time_days: 14,
            reorder_threshold: 20,
            status: "available".into(),
            linked_batch_id: Some(1),
            created_at: "2026-07-01T00:00:00Z".into(),
            updated_at: "2026-07-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: InventoryItemRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec.name, back.name);
        assert_eq!(back.qty_on_hand, 50);
    }

    #[test]
    fn test_procurement_order_record_roundtrip() {
        let rec = ProcurementOrderRecord {
            id: 1,
            entity_id: "po-uuid".into(),
            order_ref: "PO-001".into(),
            item_sku: "DB-3MM".into(),
            item_name: "Drillbit 3mm".into(),
            supplier: "ToolCo".into(),
            qty: 100,
            lead_time_days: 14,
            order_date: Some("2026-07-15".into()),
            eta: Some("2026-07-29".into()),
            status: "pending".into(),
            linked_batch_id: None,
            notes: "".into(),
            created_at: "2026-07-01T00:00:00Z".into(),
            updated_at: "2026-07-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: ProcurementOrderRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.order_ref, "PO-001");
    }

    #[test]
    fn test_alert_record_roundtrip() {
        let rec = AlertRecord {
            id: 1,
            entity_id: "alert-uuid".into(),
            alert_type: "low_stock".into(),
            severity: "urgent".into(),
            related_entity_type: "inventory_item".into(),
            related_entity_id: "inv-uuid".into(),
            due_date: Some("2026-08-01".into()),
            reason: "Below reorder threshold".into(),
            suggested_action: "Order more".into(),
            resolved: false,
            created_at: "2026-07-01T00:00:00Z".into(),
            updated_at: "2026-07-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: AlertRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.alert_type, "low_stock");
        assert!(!back.resolved);
    }

    #[test]
    fn test_event_log_record_roundtrip() {
        let rec = EventLogRecord {
            id: 1,
            timestamp: "2026-07-01T12:00:00Z".into(),
            source: "user".into(),
            action_type: "created".into(),
            entity_type: "site".into(),
            entity_id: "site-uuid".into(),
            summary: "Created site Site A".into(),
            created_at: "2026-07-01T12:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: EventLogRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, "user");
        assert_eq!(back.action_type, "created");
    }

    #[test]
    fn test_create_site_input_roundtrip() {
        let input = CreateSiteInput {
            name: "Site B".into(),
            location: Some("Area 51".into()),
            deploy_window_start: None,
            deploy_window_end: None,
            onsite_duration_days: Some(7),
            files_confirmed: Some(false),
            notes: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        let back: CreateSiteInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "Site B");
    }

    #[test]
    fn test_upsert_inventory_input_roundtrip() {
        let input = UpsertInventoryInput {
            name: "Hammer".into(),
            sku: Some("HAM-01".into()),
            category: Some("tool".into()),
            qty_on_hand: Some(10),
            location_area: None,
            location_rack: None,
            supplier: Some("Acme".into()),
            lead_time_days: Some(5),
            reorder_threshold: Some(3),
            linked_batch_id: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        let back: UpsertInventoryInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "Hammer");
    }

    #[test]
    fn test_summary_dtos_serialize() {
        let alert = AlertSummary {
            id: "alert-1".into(),
            severity: AlertSeverity::Urgent,
            category: "low_stock".into(),
            message: "Drillbits below threshold".into(),
            related_entity: "inventory/drillbit-3mm".into(),
            due_date: Some("2026-08-01".into()),
            resolved: false,
        };
        let json = serde_json::to_string(&alert).unwrap();
        assert!(json.contains("urgent"));
        assert!(json.contains("low_stock"));
        let _back: AlertSummary = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_pm_bridge_chat_request_roundtrip() {
        let req = PmBridgeChatRequest {
            message: "What should I order this week?".into(),
            history: vec![HistoryMessage {
                role: "user".into(),
                content: "Hello".into(),
            }],
            runtime_session_key: "dingtalk:private:user-1".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: PmBridgeChatRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runtime_session_key, "dingtalk:private:user-1");
        assert_eq!(back.history.len(), 1);
    }

    #[test]
    fn test_pm_bridge_chat_response_roundtrip() {
        let resp = PmBridgeChatResponse {
            reply: "Order drillbits now.".into(),
            tokens_input: 10,
            tokens_output: 20,
            status: "done".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: PmBridgeChatResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, "done");
        assert_eq!(back.tokens_output, 20);
    }

    #[test]
    fn test_imported_job_status_roundtrip() {
        let cases = [
            ImportedJobStatus::Active,
            ImportedJobStatus::Completed,
            ImportedJobStatus::Ignored,
        ];
        for s in &cases {
            let json = serde_json::to_string(s).unwrap();
            let back: ImportedJobStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(*s, back);
        }
        assert_eq!(
            serde_json::to_string(&ImportedJobStatus::Completed).unwrap(),
            "\"completed\""
        );
    }

    #[test]
    fn test_import_run_status_roundtrip() {
        let json = serde_json::to_string(&ImportRunStatus::Applied).unwrap();
        assert_eq!(json, "\"applied\"");
        let back: ImportRunStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ImportRunStatus::Applied);
    }

    #[test]
    fn test_import_conflict_type_roundtrip() {
        let json =
            serde_json::to_string(&ImportConflictType::DuplicateTitle).unwrap();
        assert_eq!(json, "\"duplicate_title\"");
        let back: ImportConflictType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ImportConflictType::DuplicateTitle);
    }

    #[test]
    fn test_imported_job_record_roundtrip() {
        let rec = ImportedJobRecord {
            id: 1,
            entity_id: "job-uuid".into(),
            source: "google_sheets".into(),
            spreadsheet_id: "spreadsheet-1".into(),
            sheet_name: "US".into(),
            external_job_key: "Site A install".into(),
            title: "Site A install".into(),
            location: "100 Bay St".into(),
            activity_type: "Boris Job".into(),
            start_date: Some("2026-08-05".into()),
            end_date: Some("2026-08-12".into()),
            job_leader: "Alice".into(),
            team_member: "Bob".into(),
            robots: "R1".into(),
            additional_info: "bring drill".into(),
            source_done: false,
            local_status: ImportedJobStatus::Active,
            row_hash: "abc123".into(),
            source_payload: serde_json::json!({"office": "Shenzhen"}),
            first_seen_at: "2026-08-01T00:00:00Z".into(),
            last_seen_at: "2026-08-02T00:00:00Z".into(),
            last_import_run_id: Some("run-uuid".into()),
            created_at: "2026-08-01T00:00:00Z".into(),
            updated_at: "2026-08-02T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: ImportedJobRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entity_id, "job-uuid");
        assert_eq!(back.external_job_key, "Site A install");
        assert_eq!(back.local_status, ImportedJobStatus::Active);
        assert_eq!(back.source_payload["office"], "Shenzhen");
    }

    #[test]
    fn test_import_run_record_roundtrip() {
        let rec = ImportRunRecord {
            id: "run-uuid".into(),
            source: "google_sheets".into(),
            spreadsheet_id: "spreadsheet-1".into(),
            sheet_name: "US".into(),
            triggered_by: "manual".into(),
            dry_run: true,
            status: ImportRunStatus::Previewed,
            preview_hash: "hash".into(),
            seen_count: 10,
            created_count: 2,
            updated_count: 1,
            unchanged_count: 5,
            conflict_count: 1,
            invalid_count: 1,
            skipped_count: 0,
            summary: serde_json::json!({"creates": 2}),
            created_at: "2026-08-01T00:00:00Z".into(),
            completed_at: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: ImportRunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "run-uuid");
        assert_eq!(back.status, ImportRunStatus::Previewed);
        assert!(back.dry_run);
    }

    #[test]
    fn test_import_conflict_record_roundtrip() {
        let rec = ImportConflictRecord {
            id: 1,
            import_run_id: Some("run-uuid".into()),
            source: "google_sheets".into(),
            external_job_key: "Shared title".into(),
            conflict_type: ImportConflictType::DuplicateTitle,
            reason: "Two rows share this title".into(),
            details: serde_json::json!({"row_numbers": [1, 2]}),
            status: ImportConflictStatus::Open,
            created_at: "2026-08-01T00:00:00Z".into(),
            resolved_at: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: ImportConflictRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.conflict_type, ImportConflictType::DuplicateTitle);
        assert_eq!(back.status, ImportConflictStatus::Open);
    }
}
