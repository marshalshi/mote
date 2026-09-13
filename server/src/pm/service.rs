//! Business rules and validation for PM operations.
//!
//! `PmService` wraps `Store` with deterministic validation before every write:
//! - No negative inventory
//! - Cannot reserve more than available
//! - Referenced entities must exist
//! - Procurement orders must reference valid inventory items
//!
//! Each write operation: validate → write → log event → return result.

use std::fmt;

use anyhow::{Context, Result};

use crate::pm::alerts;
use crate::pm::events;
use crate::pm::store::Store;

use marshaling_protocol::pm::{
    AlertGenerationSummary, AlertRecord, AssignmentRecord, BatchRecord,
    BlockerInfo, InventoryItemRecord, PersonRecord, ProcurementOrderRecord,
    SiteRecord,
};

// ── Validation error ─────────────────────────────────────────

/// Errors that occur when PM business rules are violated.
#[derive(Debug, Clone)]
pub enum ValidationError {
    /// Referenced entity does not exist.
    EntityNotFound {
        entity_type: &'static str,
        entity_id: String,
    },
    /// Operation would cause a negative quantity.
    NegativeInventory { item: String, would_be: i32 },
    /// Not enough available stock to reserve.
    InsufficientStock {
        item: String,
        available: i32,
        requested: i32,
    },
    /// Invalid cross-entity reference.
    InvalidReference { detail: String },
    /// Generic validation failure.
    Other(String),
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::EntityNotFound {
                entity_type,
                entity_id,
            } => {
                write!(f, "{} not found: {}", entity_type, entity_id)
            }
            ValidationError::NegativeInventory { item, would_be } => {
                write!(
                    f,
                    "Negative inventory would result for '{}': qty would be {}",
                    item, would_be
                )
            }
            ValidationError::InsufficientStock {
                item,
                available,
                requested,
            } => {
                write!(
                    f,
                    "Insufficient stock for '{}': {} available, {} requested",
                    item, available, requested
                )
            }
            ValidationError::InvalidReference { detail } => {
                write!(f, "Invalid reference: {}", detail)
            }
            ValidationError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for ValidationError {}

// ── Service ──────────────────────────────────────────────────

/// PM service layer with business rules and validation.
#[derive(Debug, Clone)]
pub struct PmService {
    store: Store,
}

impl PmService {
    /// Create a new service wrapping the given store.
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// Access the underlying store for read-only queries.
    pub fn store(&self) -> &Store {
        &self.store
    }
}

// ── Internal helpers ────────────────────────────────────────

impl PmService {
    /// Validate that a site exists by its internal ID.
    fn require_site(&self, id: i64, label: &str) -> Result<()> {
        if !self.store.site_exists_by_id(id)? {
            Err(ValidationError::EntityNotFound {
                entity_type: "site",
                entity_id: format!("{} (id={})", label, id),
            }
            .into())
        } else {
            Ok(())
        }
    }

    /// Validate that a batch exists by its internal ID.
    fn require_batch(&self, id: i64, label: &str) -> Result<()> {
        if !self.store.batch_exists_by_id(id)? {
            Err(ValidationError::EntityNotFound {
                entity_type: "batch",
                entity_id: format!("{} (id={})", label, id),
            }
            .into())
        } else {
            Ok(())
        }
    }

    /// Validate that a person exists by its internal ID.
    #[allow(dead_code)]
    fn require_person(&self, id: i64, label: &str) -> Result<()> {
        if !self.store.person_exists_by_id(id)? {
            Err(ValidationError::EntityNotFound {
                entity_type: "person",
                entity_id: format!("{} (id={})", label, id),
            }
            .into())
        } else {
            Ok(())
        }
    }

    /// Validate that an inventory item exists by SKU or name.
    fn require_inventory_sku_or_name(
        &self,
        sku: &str,
        name: &str,
    ) -> Result<()> {
        if !self.store.inventory_exists_by_sku_or_name(sku, name)? {
            Err(ValidationError::InvalidReference {
                detail: format!(
                    "No inventory item found with SKU '{}' or name '{}'",
                    sku, name
                ),
            }
            .into())
        } else {
            Ok(())
        }
    }
}

// ── Site operations ─────────────────────────────────────────

impl PmService {
    /// Create a new site with validation and event logging.
    pub fn create_site(
        &self,
        name: &str,
        location: &str,
        deploy_window_start: Option<&str>,
        deploy_window_end: Option<&str>,
        onsite_duration_days: i32,
        files_confirmed: bool,
        notes: &str,
    ) -> Result<SiteRecord> {
        let site = self
            .store
            .insert_site(
                name,
                location,
                deploy_window_start,
                deploy_window_end,
                onsite_duration_days,
                files_confirmed,
                notes,
            )
            .context("creating site")?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "created",
            "site",
            &site.entity_id,
            format_args!("Created site '{}' at {}", name, location),
        )?;

        Ok(site)
    }

    /// Update an existing site with existence validation and event logging.
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
    ) -> Result<SiteRecord> {
        let updated = self
            .store
            .update_site(
                entity_id,
                name,
                location,
                deploy_window_start,
                deploy_window_end,
                onsite_duration_days,
                files_confirmed,
                notes,
            )
            .context("updating site")?
            .ok_or_else(|| ValidationError::EntityNotFound {
                entity_type: "site",
                entity_id: entity_id.to_string(),
            })?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "updated",
            "site",
            entity_id,
            format_args!("Updated site '{}'", updated.name),
        )?;

        Ok(updated)
    }

    /// Get or create a site by name (upsert pattern).
    #[allow(dead_code)]
    pub fn get_or_create_site(&self, name: &str) -> Result<SiteRecord> {
        // get_or_create_site in Store may create; if it creates, log event.
        let existing = self.store.get_site_by_name(name)?;
        if let Some(site) = existing {
            return Ok(site);
        }
        // Create new
        let site = self.create_site(name, "", None, None, 0, false, "")?;
        Ok(site)
    }
}

// ── Batch operations ────────────────────────────────────────

impl PmService {
    /// Create a new batch, validating that the referenced site exists.
    pub fn create_batch(
        &self,
        name: &str,
        robot_serials: &[String],
        target_site_id: Option<i64>,
        target_deploy_date: Option<&str>,
        onsite_duration_days: i32,
        status: &str,
        notes: &str,
    ) -> Result<BatchRecord> {
        // Validate site reference
        if let Some(site_id) = target_site_id {
            self.require_site(site_id, "target_site")?;
        }

        let batch = self
            .store
            .insert_batch(
                name,
                robot_serials,
                target_site_id,
                target_deploy_date,
                onsite_duration_days,
                status,
                notes,
            )
            .context("creating batch")?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "created",
            "batch",
            &batch.entity_id,
            format_args!("Created batch '{}' with status {}", name, status),
        )?;

        Ok(batch)
    }

    /// Update an existing batch, validating references.
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
    ) -> Result<BatchRecord> {
        // If target_site_id is being set, validate existence
        if let Some(Some(site_id)) = target_site_id {
            self.require_site(site_id, "target_site")?;
        }

        let updated = self
            .store
            .update_batch(
                entity_id,
                name,
                robot_serials,
                target_site_id,
                target_deploy_date,
                onsite_duration_days,
                status,
                notes,
            )
            .context("updating batch")?
            .ok_or_else(|| ValidationError::EntityNotFound {
                entity_type: "batch",
                entity_id: entity_id.to_string(),
            })?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "updated",
            "batch",
            entity_id,
            format_args!("Updated batch '{}'", updated.name),
        )?;

        Ok(updated)
    }
}

// ── Person operations ───────────────────────────────────────

#[allow(dead_code)]
impl PmService {
    /// Create a new person.
    pub fn create_person(
        &self,
        name: &str,
        is_senior: bool,
        skills: &[String],
        special_sites: &[String],
        availability_start: Option<&str>,
        availability_end: Option<&str>,
        notes: &str,
    ) -> Result<PersonRecord> {
        let person = self
            .store
            .insert_person(
                name,
                is_senior,
                skills,
                special_sites,
                availability_start,
                availability_end,
                notes,
            )
            .context("creating person")?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "created",
            "person",
            &person.entity_id,
            format_args!("Created person '{}'", name),
        )?;

        Ok(person)
    }

    /// Update an existing person.
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
    ) -> Result<PersonRecord> {
        let updated = self
            .store
            .update_person(
                entity_id,
                name,
                is_senior,
                skills,
                special_sites,
                availability_start,
                availability_end,
                notes,
            )
            .context("updating person")?
            .ok_or_else(|| ValidationError::EntityNotFound {
                entity_type: "person",
                entity_id: entity_id.to_string(),
            })?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "updated",
            "person",
            entity_id,
            format_args!("Updated person '{}'", updated.name),
        )?;

        Ok(updated)
    }
}

// ── Inventory operations ────────────────────────────────────

impl PmService {
    /// Upsert (create or update) an inventory item.
    ///
    /// Validates:
    /// - No negative qty_on_hand (if provided)
    /// - If linked_batch_id is set, the batch must exist
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
        // Validate: no negative qty
        if let Some(qty) = qty_on_hand {
            if qty < 0 {
                return Err(ValidationError::NegativeInventory {
                    item: name.to_string(),
                    would_be: qty,
                }
                .into());
            }
        }

        // Validate batch reference if provided
        if let Some(Some(batch_id)) = linked_batch_id {
            self.require_batch(batch_id, "linked_batch")?;
        }

        let item = self
            .store
            .upsert_inventory(
                name,
                sku,
                category,
                qty_on_hand,
                location_area,
                location_rack,
                supplier,
                lead_time_days,
                reorder_threshold,
                linked_batch_id,
            )
            .context("upserting inventory")?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "updated",
            "inventory_item",
            &item.entity_id,
            format_args!(
                "Upserted inventory '{}': qty={}, sku={}",
                name, item.qty_on_hand, item.sku
            ),
        )?;

        Ok(item)
    }

    /// Reserve inventory with validation.
    ///
    /// Validates:
    /// - Item must exist
    /// - Quantity must be positive
    /// - Available stock must be sufficient
    /// - Linked batch must exist if provided
    pub fn reserve_inventory(
        &self,
        item_entity_id: &str,
        quantity: i32,
        linked_batch_id: Option<i64>,
    ) -> Result<InventoryItemRecord> {
        if quantity <= 0 {
            return Err(ValidationError::Other(format!(
                "Reservation quantity must be positive, got {}",
                quantity
            ))
            .into());
        }

        // Check item exists and get current state
        let item =
            self.store.get_inventory(item_entity_id)?.ok_or_else(|| {
                ValidationError::EntityNotFound {
                    entity_type: "inventory_item",
                    entity_id: item_entity_id.to_string(),
                }
            })?;

        // Check available stock
        let available = item.qty_on_hand - item.qty_reserved;
        if quantity > available {
            return Err(ValidationError::InsufficientStock {
                item: item.name.clone(),
                available,
                requested: quantity,
            }
            .into());
        }

        // Validate batch reference if provided
        if let Some(batch_id) = linked_batch_id {
            self.require_batch(batch_id, "linked_batch")?;
        }

        let updated = self
            .store
            .reserve_inventory(item_entity_id, quantity, linked_batch_id)
            .context("reserving inventory")?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "reserved",
            "inventory_item",
            item_entity_id,
            format_args!(
                "Reserved {} of '{}' (now {} reserved of {})",
                quantity,
                updated.name,
                updated.qty_reserved,
                updated.qty_on_hand
            ),
        )?;

        Ok(updated)
    }
}

// ── Procurement operations ──────────────────────────────────

impl PmService {
    /// Create a procurement order with validation.
    ///
    /// Validates:
    /// - The referenced inventory item (by SKU or name) must exist
    /// - Linked batch must exist if provided
    /// - Quantity must be positive
    pub fn create_procurement_order(
        &self,
        item_sku: &str,
        item_name: &str,
        supplier: &str,
        qty: i32,
        lead_time_days: Option<i32>,
        linked_batch_id: Option<i64>,
        notes: Option<&str>,
    ) -> Result<ProcurementOrderRecord> {
        if qty <= 0 {
            return Err(ValidationError::Other(format!(
                "Order quantity must be positive, got {}",
                qty
            ))
            .into());
        }

        // Validate: inventory item must exist
        self.require_inventory_sku_or_name(item_sku, item_name)?;

        // Validate batch reference if provided
        if let Some(batch_id) = linked_batch_id {
            self.require_batch(batch_id, "linked_batch")?;
        }

        let lead = lead_time_days.unwrap_or(0);

        let po = self
            .store
            .insert_procurement_order(
                item_sku,
                item_name,
                supplier,
                qty,
                lead,
                linked_batch_id,
                notes.unwrap_or(""),
            )
            .context("creating procurement order")?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "ordered",
            "procurement_order",
            &po.entity_id,
            format_args!(
                "Created procurement order for {} x '{}' from {}",
                qty, item_name, supplier
            ),
        )?;

        Ok(po)
    }

    /// Update an existing procurement order.
    #[allow(dead_code)]
    pub fn update_procurement_order(
        &self,
        entity_id: &str,
        status: Option<&str>,
        eta: Option<&str>,
        order_ref: Option<&str>,
        supplier: Option<&str>,
        qty: Option<i32>,
        notes: Option<&str>,
    ) -> Result<ProcurementOrderRecord> {
        let updated = self
            .store
            .update_procurement_order(
                entity_id, status, eta, order_ref, supplier, qty, notes,
            )
            .context("updating procurement order")?
            .ok_or_else(|| ValidationError::EntityNotFound {
                entity_type: "procurement_order",
                entity_id: entity_id.to_string(),
            })?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "updated",
            "procurement_order",
            entity_id,
            format_args!("Updated procurement order '{}'", updated.item_name),
        )?;

        Ok(updated)
    }
}

// ── Assignment operations ──────────────────────────────────

impl PmService {
    /// Assign a person to a site or batch with validation.
    pub fn assign_person(
        &self,
        person_entity_id: &str,
        entity_type: &str,
        entity_id: &str,
        role: &str,
        start_date: Option<&str>,
        end_date: Option<&str>,
    ) -> Result<AssignmentRecord> {
        // Validate person exists
        let person =
            self.store.get_person(person_entity_id)?.ok_or_else(|| {
                ValidationError::EntityNotFound {
                    entity_type: "person",
                    entity_id: person_entity_id.to_string(),
                }
            })?;

        let (site_id, batch_id) = match entity_type {
            "site" => {
                let site =
                    self.store.get_site(entity_id)?.ok_or_else(|| {
                        ValidationError::EntityNotFound {
                            entity_type: "site",
                            entity_id: entity_id.to_string(),
                        }
                    })?;
                (Some(site.id), None)
            }
            "batch" => {
                let batch =
                    self.store.get_batch(entity_id)?.ok_or_else(|| {
                        ValidationError::EntityNotFound {
                            entity_type: "batch",
                            entity_id: entity_id.to_string(),
                        }
                    })?;
                (None, Some(batch.id))
            }
            _ => {
                return Err(ValidationError::InvalidReference {
                    detail: format!("Unknown entity_type: {}", entity_type),
                }
                .into());
            }
        };

        let assignment = self
            .store
            .insert_assignment(
                person.id, site_id, batch_id, role, start_date, end_date, None,
            )
            .context("creating assignment")?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "assigned",
            "assignment",
            &assignment.entity_id,
            format_args!(
                "Assigned person '{}' to {} '{}' as {}",
                person.name, entity_type, entity_id, role
            ),
        )?;

        Ok(assignment)
    }

    /// Record a handover by finding the assignment for the outgoing person
    /// and setting the handover target.
    pub fn record_handover(
        &self,
        entity_type: &str,
        entity_id: &str,
        from_person_entity_id: &str,
        to_person_entity_id: &str,
        handover_date: Option<&str>,
    ) -> Result<AssignmentRecord> {
        // Validate both people exist
        let from_person = self
            .store
            .get_person(from_person_entity_id)?
            .ok_or_else(|| ValidationError::EntityNotFound {
                entity_type: "person",
                entity_id: from_person_entity_id.to_string(),
            })?;
        let to_person = self
            .store
            .get_person(to_person_entity_id)?
            .ok_or_else(|| ValidationError::EntityNotFound {
                entity_type: "person",
                entity_id: to_person_entity_id.to_string(),
            })?;

        // Find the assignment
        let assignment = match entity_type {
            "site" => {
                let site =
                    self.store.get_site(entity_id)?.ok_or_else(|| {
                        ValidationError::EntityNotFound {
                            entity_type: "site",
                            entity_id: entity_id.to_string(),
                        }
                    })?;
                self.store
                    .find_assignment_by_person_and_site(
                        from_person.id,
                        site.id,
                    )?
                    .ok_or_else(|| ValidationError::InvalidReference {
                        detail: format!(
                            "No assignment found for person '{}' at site '{}'",
                            from_person.name, entity_id
                        ),
                    })?
            }
            "batch" => {
                let batch =
                    self.store.get_batch(entity_id)?.ok_or_else(|| {
                        ValidationError::EntityNotFound {
                            entity_type: "batch",
                            entity_id: entity_id.to_string(),
                        }
                    })?;
                self.store
                    .find_assignment_by_person_and_batch(
                        from_person.id,
                        batch.id,
                    )?
                    .ok_or_else(|| ValidationError::InvalidReference {
                        detail: format!(
                            "No assignment found for person '{}' at batch '{}'",
                            from_person.name, entity_id
                        ),
                    })?
            }
            _ => {
                return Err(ValidationError::InvalidReference {
                    detail: format!("Unknown entity_type: {}", entity_type),
                }
                .into());
            }
        };

        let updated = self
            .store
            .update_assignment_handover(
                &assignment.entity_id,
                Some(to_person.id),
                handover_date,
            )?
            .ok_or_else(|| ValidationError::EntityNotFound {
                entity_type: "assignment",
                entity_id: assignment.entity_id.clone(),
            })?;

        events::log_write_event_fmt(
            &self.store,
            "user",
            "updated",
            "assignment",
            &updated.entity_id,
            format_args!(
                "Handover recorded: '{}' -> '{}' on {}",
                from_person.name,
                to_person.name,
                handover_date.unwrap_or("(no date)")
            ),
        )?;

        Ok(updated)
    }
}

// ── Status summary ──────────────────────────────────────────

impl PmService {
    /// Get a summary of the current PM state.
    pub fn get_status_summary(
        &self,
    ) -> Result<marshaling_protocol::pm::StatusSummary> {
        let active_alerts = self.store.list_active_alerts()?;
        let all_batches = self.store.list_batches()?;
        let low_stock_items = self.store.list_inventory_low_stock()?;
        let pending_orders = self.store.list_open_orders()?;
        let total_sites = self.store.list_sites()?.len();
        let total_people = self.store.list_people()?.len();
        let recent_events = self.store.list_events(20)?;

        // Use alerts::detect_blocked_batches() to determine which batches
        // are actually blocked (not just by status).
        let blocker_info = alerts::detect_blocked_batches(&self.store)?;
        let blocked_entity_ids: std::collections::HashSet<String> =
            blocker_info
                .iter()
                .map(|b| b.batch_entity_id.clone())
                .collect();
        let blocked_batches: Vec<BatchRecord> = all_batches
            .into_iter()
            .filter(|b| blocked_entity_ids.contains(&b.entity_id))
            .collect();

        Ok(marshaling_protocol::pm::StatusSummary {
            active_alerts,
            blocked_batches,
            low_stock_items,
            pending_orders,
            total_sites,
            total_people,
            recent_events,
        })
    }
}

// ── Readiness and blockers ──────────────────────────────────

impl PmService {
    /// Check deploy readiness for a batch.
    pub fn check_deploy_readiness(
        &self,
        batch_entity_id: &str,
    ) -> Result<marshaling_protocol::pm::ReadinessReport> {
        let batch =
            self.store.get_batch(batch_entity_id)?.ok_or_else(|| {
                ValidationError::EntityNotFound {
                    entity_type: "batch",
                    entity_id: batch_entity_id.to_string(),
                }
            })?;

        let mut blockers: Vec<marshaling_protocol::pm::BlockerInfo> =
            Vec::new();

        // Check 1: Is the batch already at a terminal status?
        if matches!(
            batch.status.as_str(),
            "completed" | "cancelled" | "deployed"
        ) {
            let is_ready =
                batch.status == "deployed" || batch.status == "completed";
            return Ok(marshaling_protocol::pm::ReadinessReport {
                batch_entity_id: batch_entity_id.to_string(),
                batch_name: batch.name,
                status: batch.status,
                is_ready,
                blockers,
            });
        }

        // Check 2: Active alerts related to this batch
        let alerts = self.store.list_active_alerts()?;
        for alert in &alerts {
            if alert.related_entity_id == batch_entity_id
                || alert.related_entity_id == batch.name
            {
                blockers.push(marshaling_protocol::pm::BlockerInfo {
                    batch_entity_id: batch_entity_id.to_string(),
                    batch_name: batch.name.clone(),
                    reason: format!(
                        "Alert [{}]: {} - {}",
                        alert.severity, alert.alert_type, alert.reason
                    ),
                    severity: alert.severity.clone(),
                });
            }
        }

        // Check 3: Inventory reserved for this batch
        let inventory_items = self.store.list_inventory()?;
        for item in &inventory_items {
            if item.linked_batch_id == Some(batch.id) && item.qty_reserved == 0
            {
                blockers.push(marshaling_protocol::pm::BlockerInfo {
                    batch_entity_id: batch_entity_id.to_string(),
                    batch_name: batch.name.clone(),
                    reason: format!(
                        "Item '{}' linked to batch but not reserved",
                        item.name
                    ),
                    severity: "urgent".to_string(),
                });
            }
        }

        // Check 4: Procurement orders for this batch that are not delivered
        let all_orders = self.store.list_open_orders()?;
        let batch_orders: Vec<_> = all_orders
            .iter()
            .filter(|o| o.linked_batch_id == Some(batch.id))
            .collect();
        for po in &batch_orders {
            if po.status != "delivered" && po.status != "shipped" {
                blockers.push(marshaling_protocol::pm::BlockerInfo {
                    batch_entity_id: batch_entity_id.to_string(),
                    batch_name: batch.name.clone(),
                    reason: format!(
                        "Procurement order for '{}' ({}) not yet delivered",
                        po.item_name, po.status
                    ),
                    severity: "upcoming".to_string(),
                });
            }
        }

        // Check 5: Target site assigned?
        if batch.target_site_id.is_none() {
            blockers.push(marshaling_protocol::pm::BlockerInfo {
                batch_entity_id: batch_entity_id.to_string(),
                batch_name: batch.name.clone(),
                reason: "No target site assigned".to_string(),
                severity: "urgent".to_string(),
            });
        }

        // Check 6: Deploy date set?
        if batch.target_deploy_date.is_none() {
            blockers.push(marshaling_protocol::pm::BlockerInfo {
                batch_entity_id: batch_entity_id.to_string(),
                batch_name: batch.name.clone(),
                reason: "No target deploy date set".to_string(),
                severity: "upcoming".to_string(),
            });
        }

        let is_ready = blockers.is_empty() && batch.status == "ready";

        Ok(marshaling_protocol::pm::ReadinessReport {
            batch_entity_id: batch_entity_id.to_string(),
            batch_name: batch.name,
            status: batch.status,
            is_ready,
            blockers,
        })
    }

    /// List all currently blocked batches with reasons.
    pub fn list_blockers(
        &self,
    ) -> Result<Vec<marshaling_protocol::pm::BlockerInfo>> {
        let batches = self.store.list_batches()?;
        let active_alerts = self.store.list_active_alerts()?;
        let inventory_items = self.store.list_inventory()?;
        let open_orders = self.store.list_open_orders()?;

        let mut all_blockers: Vec<marshaling_protocol::pm::BlockerInfo> =
            Vec::new();

        for batch in &batches {
            if matches!(
                batch.status.as_str(),
                "completed" | "cancelled" | "deployed" | "ready"
            ) {
                continue;
            }

            // Alerts for this batch
            for alert in &active_alerts {
                if alert.related_entity_id == batch.entity_id
                    || alert.related_entity_id == batch.name
                {
                    all_blockers.push(marshaling_protocol::pm::BlockerInfo {
                        batch_entity_id: batch.entity_id.clone(),
                        batch_name: batch.name.clone(),
                        reason: format!(
                            "Alert [{}]: {} - {}",
                            alert.severity, alert.alert_type, alert.reason
                        ),
                        severity: alert.severity.clone(),
                    });
                }
            }

            // Unreserved linked inventory
            for item in &inventory_items {
                if item.linked_batch_id == Some(batch.id)
                    && item.qty_reserved == 0
                {
                    all_blockers.push(marshaling_protocol::pm::BlockerInfo {
                        batch_entity_id: batch.entity_id.clone(),
                        batch_name: batch.name.clone(),
                        reason: format!(
                            "Item '{}' linked to batch but not reserved",
                            item.name
                        ),
                        severity: "urgent".to_string(),
                    });
                }
            }

            // Open procurement for this batch
            for po in &open_orders {
                if po.linked_batch_id == Some(batch.id) {
                    all_blockers.push(marshaling_protocol::pm::BlockerInfo {
                        batch_entity_id: batch.entity_id.clone(),
                        batch_name: batch.name.clone(),
                        reason: format!(
                            "Open procurement order for '{}' ({})",
                            po.item_name, po.status
                        ),
                        severity: "upcoming".to_string(),
                    });
                }
            }

            // Missing target site
            if batch.target_site_id.is_none() {
                all_blockers.push(marshaling_protocol::pm::BlockerInfo {
                    batch_entity_id: batch.entity_id.clone(),
                    batch_name: batch.name.clone(),
                    reason: "No target site assigned".to_string(),
                    severity: "urgent".to_string(),
                });
            }

            // Missing deploy date
            if batch.target_deploy_date.is_none() {
                all_blockers.push(marshaling_protocol::pm::BlockerInfo {
                    batch_entity_id: batch.entity_id.clone(),
                    batch_name: batch.name.clone(),
                    reason: "No target deploy date set".to_string(),
                    severity: "upcoming".to_string(),
                });
            }
        }

        Ok(all_blockers)
    }
}

// ── Alert operations ────────────────────────────────────────

#[allow(dead_code)]
impl PmService {
    /// Create a new alert and log the event.
    pub fn create_alert(
        &self,
        alert_type: &str,
        severity: &str,
        related_entity_type: &str,
        related_entity_id: &str,
        due_date: Option<&str>,
        reason: &str,
        suggested_action: &str,
    ) -> Result<AlertRecord> {
        let alert = self
            .store
            .insert_alert(
                alert_type,
                severity,
                related_entity_type,
                related_entity_id,
                due_date,
                reason,
                suggested_action,
            )
            .context("creating alert")?;

        events::log_write_event_fmt(
            &self.store,
            "system",
            "alert_generated",
            "alert",
            &alert.entity_id,
            format_args!("Alert [{}] {}: {}", severity, alert_type, reason),
        )?;

        Ok(alert)
    }

    /// Resolve an alert and log the event.
    pub fn resolve_alert(&self, entity_id: &str) -> Result<AlertRecord> {
        let alert = self
            .store
            .resolve_alert(entity_id)
            .context("resolving alert")?
            .ok_or_else(|| ValidationError::EntityNotFound {
                entity_type: "alert",
                entity_id: entity_id.to_string(),
            })?;

        events::log_write_event_fmt(
            &self.store,
            "system",
            "alert_resolved",
            "alert",
            entity_id,
            format_args!(
                "Resolved alert [{}] {}",
                alert.severity, alert.alert_type
            ),
        )?;

        Ok(alert)
    }
}

// ── Alert check operations ───────────────────────────────────

/// Result of running alert generation and blocked-batch detection.
#[derive(Debug, Clone)]
pub struct AlertCheckReport {
    /// Summary of generated alerts.
    pub generation: AlertGenerationSummary,
    /// Blocked batches detected from current state.
    pub blocked_batches: Vec<BlockerInfo>,
    /// All currently active (unresolved) alerts.
    pub active_alerts: Vec<AlertRecord>,
}

impl PmService {
    /// Run the full alert check pipeline.
    ///
    /// 1. Generate inventory and procurement alerts
    /// 2. Detect blocked batches
    /// 3. Return a combined report
    pub fn run_alert_checks(&self) -> Result<AlertCheckReport> {
        let generation = alerts::generate_all_alerts(&self.store)?;
        let blocked_batches = alerts::detect_blocked_batches(&self.store)?;
        let active_alerts = self.store.list_active_alerts()?;

        Ok(AlertCheckReport {
            generation,
            blocked_batches,
            active_alerts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm::schema;
    use rusqlite::Connection;

    fn create_service() -> PmService {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        schema::bootstrap(&conn).expect("bootstrap");
        let store = Store::new(conn);
        PmService::new(store)
    }

    // ── Site tests ──

    #[test]
    fn test_create_and_get_site() {
        let svc = create_service();
        let site = svc
            .create_site("Test Site", "Location", None, None, 7, false, "notes")
            .expect("create site");
        assert!(site.id > 0);
    }

    #[test]
    fn test_update_site_not_found() {
        let svc = create_service();
        let err = svc
            .update_site(
                "nonexistent",
                Some("New"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("not found"), "error: {}", desc);
    }

    // ── Batch tests ──

    #[test]
    fn test_create_batch_with_invalid_site() {
        let svc = create_service();
        let err = svc
            .create_batch("Bad Batch", &[], Some(999), None, 0, "planned", "")
            .unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("not found"), "error: {}", desc);
    }

    #[test]
    fn test_create_batch_with_valid_site() {
        let svc = create_service();
        let site = svc
            .create_site("Site", "", None, None, 0, false, "")
            .unwrap();
        let batch = svc
            .create_batch(
                "Good Batch",
                &[],
                Some(site.id),
                None,
                0,
                "planned",
                "",
            )
            .expect("create batch");
        assert_eq!(batch.target_site_id, Some(site.id));
    }

    #[test]
    fn test_update_batch_not_found() {
        let svc = create_service();
        let err = svc
            .update_batch(
                "nonexistent",
                Some("N"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(format!("{}", err).contains("not found"));
    }

    // ── Person tests ──

    #[test]
    fn test_create_person() {
        let svc = create_service();
        let person = svc
            .create_person(
                "Alice",
                true,
                &["coding".into()],
                &[],
                None,
                None,
                "",
            )
            .expect("create person");
        assert!(person.is_senior);
    }

    #[test]
    fn test_update_person_not_found() {
        let svc = create_service();
        let err = svc
            .update_person(
                "nonexistent",
                Some("N"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(format!("{}", err).contains("not found"));
    }

    // ── Inventory tests ──

    #[test]
    fn test_upsert_inventory_negative_qty() {
        let svc = create_service();
        let err = svc
            .upsert_inventory(
                "Bad",
                None,
                None,
                Some(-5),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("Negative"), "error: {}", desc);
    }

    #[test]
    fn test_reserve_inventory_success() {
        let svc = create_service();
        let item = svc
            .upsert_inventory(
                "TestItem",
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
            .expect("upsert");
        let reserved = svc
            .reserve_inventory(&item.entity_id, 30, None)
            .expect("reserve");
        assert_eq!(reserved.qty_reserved, 30);
    }

    #[test]
    fn test_reserve_inventory_insufficient() {
        let svc = create_service();
        let item = svc
            .upsert_inventory(
                "TestItem",
                None,
                None,
                Some(10),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("upsert");
        let err = svc
            .reserve_inventory(&item.entity_id, 20, None)
            .unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("Insufficient"), "error: {}", desc);
    }

    #[test]
    fn test_reserve_inventory_not_found() {
        let svc = create_service();
        let err = svc.reserve_inventory("nonexistent", 5, None).unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("not found"), "error: {}", desc);
    }

    #[test]
    fn test_reserve_zero_or_negative() {
        let svc = create_service();
        let item = svc
            .upsert_inventory(
                "X",
                None,
                None,
                Some(10),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let err = svc.reserve_inventory(&item.entity_id, 0, None).unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("positive"), "error: {}", desc);
    }

    #[test]
    fn test_upsert_inventory_with_invalid_batch() {
        let svc = create_service();
        let err = svc
            .upsert_inventory(
                "X",
                None,
                None,
                Some(10),
                None,
                None,
                None,
                None,
                None,
                Some(Some(999)),
            )
            .unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("not found"), "error: {}", desc);
    }

    // ── Procurement tests ──

    #[test]
    fn test_create_procurement_order_without_inventory() {
        let svc = create_service();
        let err = svc
            .create_procurement_order(
                "NO-SKU", "NoName", "Sup", 10, None, None, None,
            )
            .unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("No inventory"), "error: {}", desc);
    }

    #[test]
    fn test_create_procurement_order_with_inventory() {
        let svc = create_service();
        svc.upsert_inventory(
            "MyItem",
            Some("MY-SKU"),
            None,
            Some(10),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("upsert inventory");
        let po = svc
            .create_procurement_order(
                "MY-SKU",
                "MyItem",
                "Supplier",
                50,
                Some(14),
                None,
                None,
            )
            .expect("create PO");
        assert_eq!(po.qty, 50);
        assert_eq!(po.item_name, "MyItem");
    }

    #[test]
    fn test_create_procurement_order_zero_qty() {
        let svc = create_service();
        svc.upsert_inventory(
            "Z",
            Some("Z"),
            None,
            Some(10),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let err = svc
            .create_procurement_order("Z", "Z", "Sup", 0, None, None, None)
            .unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("positive"), "error: {}", desc);
    }

    #[test]
    fn test_create_procurement_order_with_invalid_batch() {
        let svc = create_service();
        svc.upsert_inventory(
            "X",
            Some("X"),
            None,
            Some(10),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let err = svc
            .create_procurement_order(
                "X",
                "X",
                "Sup",
                10,
                None,
                Some(999),
                None,
            )
            .unwrap_err();
        let desc = format!("{}", err);
        assert!(desc.contains("not found"), "error: {}", desc);
    }

    // ── Alert tests ──

    #[test]
    fn test_create_and_resolve_alert() {
        let svc = create_service();
        let alert = svc
            .create_alert(
                "low_stock",
                "urgent",
                "inventory_item",
                "inv-uuid",
                None,
                "Low",
                "Order",
            )
            .expect("create alert");
        assert!(!alert.resolved);

        let resolved = svc.resolve_alert(&alert.entity_id).expect("resolve");
        assert!(resolved.resolved);
    }

    #[test]
    fn test_resolve_alert_not_found() {
        let svc = create_service();
        let err = svc.resolve_alert("nonexistent").unwrap_err();
        assert!(format!("{}", err).contains("not found"));
    }

    // ── Missing store helper test ──
    // get_site_by_name is needed by get_or_create_site service method
    #[test]
    fn test_get_or_create_site() {
        let svc = create_service();
        let first = svc.get_or_create_site("MySite").expect("get or create");
        let second = svc
            .get_or_create_site("MySite")
            .expect("get or create again");
        assert_eq!(first.entity_id, second.entity_id);
    }

    // ── Alert check tests ──

    #[test]
    fn test_run_alert_checks_with_seeded_data() {
        let svc = create_service();
        // Create an inventory item below threshold
        svc.upsert_inventory(
            "Bolt",
            Some("BOLT"),
            None,
            Some(2),
            None,
            None,
            None,
            None,
            Some(10),
            None,
        )
        .expect("create low-stock item");

        // Create a procurement order with overdue ETA
        let po = svc
            .create_procurement_order(
                "BOLT", "Bolt", "Sup", 50, None, None, None,
            )
            .expect("create PO");
        let past_eta = (chrono::Utc::now().date_naive()
            - chrono::Duration::days(3))
        .format("%Y-%m-%d")
        .to_string();
        svc.store()
            .update_procurement_order(
                &po.entity_id,
                None,
                Some(&past_eta),
                None,
                None,
                None,
                None,
            )
            .expect("set ETA");

        // Create a batch missing site and date
        svc.store()
            .insert_batch("Orphan Batch", &[], None, None, 0, "planned", "")
            .expect("create batch");

        let report = svc.run_alert_checks().expect("run alert checks");
        assert!(report.generation.total > 0, "should have generated alerts");
        assert!(
            !report.active_alerts.is_empty(),
            "should have active alerts"
        );
        assert!(
            !report.blocked_batches.is_empty(),
            "should have blocked batches"
        );
        assert!(
            report
                .blocked_batches
                .iter()
                .any(|b| b.batch_name == "Orphan Batch"),
            "orphan batch should be blocked"
        );
    }
}
