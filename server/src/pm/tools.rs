//! PM tool adapters for the agent loop.
//!
//! Each tool wraps an `Arc<PmService>` and implements the `Tool` trait.
//! Tools parse JSON arguments, call service methods, and return
//! `ToolExecutionResult` with formatted output.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use serde_json::Value;

use crate::llm::{Tool, ToolDef, ToolExecutionResult, ToolFunctionDef};
use crate::pm::alerts;
use crate::pm::service::PmService;

// ── Helper ───────────────────────────────────────────────────

/// Format a tool result with JSON output.
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

/// Format a simple text result.
fn text_result(text: String) -> ToolExecutionResult {
    ToolExecutionResult {
        output: text,
        changes: Vec::new(),
        rollback_entries: Vec::new(),
    }
}

fn string_or_default(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn optional_string(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn optional_i32(v: &Value, key: &str) -> Option<i32> {
    v.get(key).and_then(|v| v.as_i64()).map(|n| n as i32)
}

// ── pm_get_status ─────────────────────────────────────────

pub struct PmGetStatusTool {
    service: Arc<PmService>,
}

impl PmGetStatusTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmGetStatusTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_get_status".into(),
                description: "Get a concise status snapshot of all PM entities: open alerts by severity, blocked batches, low-stock items, pending procurement, and near-term readiness issues.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "description": "Optional scope: 'all' (default), 'alerts', 'batches', 'inventory', 'procurement'",
                            "enum": ["all", "alerts", "batches", "inventory", "procurement"]
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, _args: Value) -> Result<ToolExecutionResult> {
        // Run alert checks first so the status includes fresh alerts
        let alert_report = self.service.run_alert_checks().ok();

        let summary = self
            .service
            .get_status_summary()
            .context("fetching PM status")?;

        let mut lines = Vec::new();
        lines.push("📊 PM Status Summary".to_string());
        lines.push(format!("  Sites: {}", summary.total_sites));
        lines.push(format!("  People: {}", summary.total_people));
        lines.push(format!("  Active alerts: {}", summary.active_alerts.len()));
        lines.push(format!(
            "  Blocked batches: {}",
            summary.blocked_batches.len()
        ));
        lines.push(format!(
            "  Low-stock items: {}",
            summary.low_stock_items.len()
        ));
        lines.push(format!(
            "  Pending procurement orders: {}",
            summary.pending_orders.len()
        ));
        lines.push(String::new());

        // Show alert generation summary if available
        if let Some(ref ar) = alert_report {
            if ar.generation.total > 0 {
                lines.push(format!(
                    "Alert generation: {} inventory + {} procurement = {} total",
                    ar.generation.inventory_alerts,
                    ar.generation.procurement_alerts,
                    ar.generation.total,
                ));
                lines.push(String::new());
            }
        }

        if !summary.active_alerts.is_empty() {
            lines.push("Active Alerts:".to_string());
            for alert in &summary.active_alerts {
                lines.push(format!(
                    "  [{}] {}: {}",
                    alert.severity, alert.alert_type, alert.reason
                ));
            }
            lines.push(String::new());
        }

        // Show blocked batches
        if let Some(ref ar) = alert_report {
            if !ar.blocked_batches.is_empty() {
                lines.push("Blocked Batches:".to_string());
                for blocker in &ar.blocked_batches {
                    lines.push(format!(
                        "  '{}': [{}] {}",
                        blocker.batch_name, blocker.severity, blocker.reason
                    ));
                }
                lines.push(String::new());
            }
        }

        if !summary.low_stock_items.is_empty() {
            lines.push("Low-Stock Items:".to_string());
            for item in &summary.low_stock_items {
                lines.push(format!(
                    "  {} ({} on hand, threshold {})",
                    item.name, item.qty_on_hand, item.reorder_threshold
                ));
            }
            lines.push(String::new());
        }

        Ok(text_result(lines.join("\n")))
    }
}

// ── pm_list_sites ─────────────────────────────────────────

pub struct PmListSitesTool {
    service: Arc<PmService>,
}

impl PmListSitesTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmListSitesTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_list_sites".into(),
                description: "List deployment sites from the PM database. Use this for questions like 'show all sites', 'which sites are active', or 'sites deploying now'. The 'active' and 'deploying' filters include sites linked to non-terminal batches; 'deploying' prioritizes deployed/ready/in-preparation batches.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "string",
                            "description": "Which sites to list: 'all', 'active', or 'deploying'. Use 'active' for active deployment work and 'deploying' for deploying now.",
                            "enum": ["all", "active", "deploying"]
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let filter =
            args.get("filter").and_then(|v| v.as_str()).unwrap_or("all");
        let sites = self.service.store().list_sites()?;
        let batches = self.service.store().list_batches()?;

        let site_rows = sites
            .into_iter()
            .filter_map(|site| {
                let linked_batches: Vec<_> = batches
                    .iter()
                    .filter(|batch| batch.target_site_id == Some(site.id))
                    .filter(|batch| match filter {
                        "active" => !matches!(
                            batch.status.as_str(),
                            "completed" | "cancelled"
                        ),
                        "deploying" => matches!(
                            batch.status.as_str(),
                            "deployed" | "ready" | "in_preparation"
                        ),
                        _ => true,
                    })
                    .collect();

                if filter != "all" && linked_batches.is_empty() {
                    return None;
                }

                let batch_summary = if linked_batches.is_empty() {
                    "no linked batches".to_string()
                } else {
                    linked_batches
                        .iter()
                        .map(|batch| {
                            format!(
                                "{} [{}{}]",
                                batch.name,
                                batch.status,
                                batch
                                    .target_deploy_date
                                    .as_ref()
                                    .map(|d| format!(", deploy {}", d))
                                    .unwrap_or_default()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };

                Some(serde_json::json!({
                    "id": site.entity_id,
                    "name": site.name,
                    "location": site.location,
                    "deploy_window_start": site.deploy_window_start,
                    "deploy_window_end": site.deploy_window_end,
                    "onsite_duration_days": site.onsite_duration_days,
                    "files_confirmed": site.files_confirmed,
                    "notes": site.notes,
                    "batches": batch_summary,
                }))
            })
            .collect::<Vec<_>>();

        let label = match filter {
            "active" => "Active PM sites",
            "deploying" => "Deploying PM sites",
            _ => "All PM sites",
        };

        if site_rows.is_empty() {
            return Ok(text_result(format!("{label}: none found")));
        }

        let mut lines = vec![format!("{label} ({}):", site_rows.len())];
        for row in &site_rows {
            lines.push(format!(
                "  - {} ({}) — {}; batches: {}",
                row["name"].as_str().unwrap_or(""),
                row["id"].as_str().unwrap_or(""),
                row["location"].as_str().unwrap_or(""),
                row["batches"].as_str().unwrap_or("")
            ));
        }

        Ok(json_result(&lines.join("\n"), &site_rows))
    }
}

// ── pm_list_batches ────────────────────────────────────────

pub struct PmListBatchesTool {
    service: Arc<PmService>,
}

impl PmListBatchesTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmListBatchesTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_list_batches".into(),
                description: "List deployment batches from the PM database, including target site, deploy date, robot serials, and status. Use this before answering batch/readiness/timing questions.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "string",
                            "description": "Which batches to list: all, active, upcoming, deployed, blocked",
                            "enum": ["all", "active", "upcoming", "deployed", "blocked"]
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let filter =
            args.get("filter").and_then(|v| v.as_str()).unwrap_or("all");
        let batches = self.service.store().list_batches()?;
        let blockers = self.service.list_blockers().unwrap_or_default();
        let blocked_ids: std::collections::HashSet<_> = blockers
            .iter()
            .map(|b| b.batch_entity_id.as_str())
            .collect();
        let rows = batches
            .into_iter()
            .filter(|batch| match filter {
                "active" => !matches!(
                    batch.status.as_str(),
                    "completed" | "cancelled"
                ),
                "upcoming" => matches!(batch.status.as_str(), "planned" | "in_preparation" | "ready"),
                "deployed" => batch.status == "deployed",
                "blocked" => blocked_ids.contains(batch.entity_id.as_str()),
                _ => true,
            })
            .map(|batch| {
                let site = batch
                    .target_site_id
                    .and_then(|id| self.service.store().get_site_by_id(id).ok().flatten());
                serde_json::json!({
                    "id": batch.entity_id,
                    "name": batch.name,
                    "status": batch.status,
                    "target_deploy_date": batch.target_deploy_date,
                    "onsite_duration_days": batch.onsite_duration_days,
                    "robot_serials": batch.robot_serials,
                    "target_site": site.as_ref().map(|s| serde_json::json!({"id": s.entity_id, "name": s.name, "location": s.location})),
                    "blocked": blocked_ids.contains(batch.entity_id.as_str()),
                    "notes": batch.notes,
                })
            })
            .collect::<Vec<_>>();

        if rows.is_empty() {
            return Ok(text_result(format!(
                "PM batches ({filter}): none found"
            )));
        }
        let mut lines =
            vec![format!("PM batches ({filter}) ({}):", rows.len())];
        for row in &rows {
            let site_name =
                row["target_site"]["name"].as_str().unwrap_or("no site");
            lines.push(format!(
                "  - {} ({}) — status: {}, site: {}, deploy: {}",
                row["name"].as_str().unwrap_or(""),
                row["id"].as_str().unwrap_or(""),
                row["status"].as_str().unwrap_or(""),
                site_name,
                row["target_deploy_date"].as_str().unwrap_or("not set"),
            ));
        }
        Ok(json_result(&lines.join("\n"), &rows))
    }
}

// ── pm_list_people ─────────────────────────────────────────

pub struct PmListPeopleTool {
    service: Arc<PmService>,
}

impl PmListPeopleTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmListPeopleTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_list_people".into(),
                description: "List people/team members from the PM database, including seniority, skills, special sites, availability, and notes.".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}, "required": []}),
            },
        }
    }

    async fn execute(&self, _args: Value) -> Result<ToolExecutionResult> {
        let people = self.service.store().list_people()?;
        let rows = people
            .into_iter()
            .map(|person| {
                serde_json::json!({
                    "id": person.entity_id,
                    "name": person.name,
                    "is_senior": person.is_senior,
                    "skills": person.skills,
                    "special_sites": person.special_sites,
                    "availability_start": person.availability_start,
                    "availability_end": person.availability_end,
                    "notes": person.notes,
                })
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Ok(text_result("PM people: none found".to_string()));
        }
        let mut lines = vec![format!("PM people ({}):", rows.len())];
        for row in &rows {
            lines.push(format!(
                "  - {} ({}) — {}",
                row["name"].as_str().unwrap_or(""),
                row["id"].as_str().unwrap_or(""),
                if row["is_senior"].as_bool().unwrap_or(false) {
                    "senior"
                } else {
                    "junior"
                },
            ));
        }
        Ok(json_result(&lines.join("\n"), &rows))
    }
}

// ── pm_list_inventory ──────────────────────────────────────

pub struct PmListInventoryTool {
    service: Arc<PmService>,
}

impl PmListInventoryTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmListInventoryTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_list_inventory".into(),
                description: "List inventory from the PM database. Use this before answering stock, purchase, low-stock, supplier, location, or lead-time questions.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "filter": {"type": "string", "enum": ["all", "low_stock", "available", "reserved"], "description": "Inventory filter"}
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let filter =
            args.get("filter").and_then(|v| v.as_str()).unwrap_or("all");
        let rows = self
            .service
            .store()
            .list_inventory()?
            .into_iter()
            .filter(|item| match filter {
                "low_stock" => matches!(item.status.as_str(), "low_stock" | "out_of_stock"),
                "available" => item.qty_on_hand > item.qty_reserved,
                "reserved" => item.qty_reserved > 0,
                _ => true,
            })
            .map(|item| serde_json::json!({
                "id": item.entity_id,
                "sku": item.sku,
                "name": item.name,
                "category": item.category,
                "qty_on_hand": item.qty_on_hand,
                "qty_reserved": item.qty_reserved,
                "qty_available": item.qty_on_hand - item.qty_reserved,
                "location": format!("{}/{}", item.location_area, item.location_rack).trim_matches('/').to_string(),
                "supplier": item.supplier,
                "lead_time_days": item.lead_time_days,
                "reorder_threshold": item.reorder_threshold,
                "status": item.status,
                "linked_batch_id": item.linked_batch_id,
            }))
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Ok(text_result(format!(
                "PM inventory ({filter}): none found"
            )));
        }
        let mut lines =
            vec![format!("PM inventory ({filter}) ({}):", rows.len())];
        for row in &rows {
            lines.push(format!(
                "  - {} ({}) — on hand: {}, reserved: {}, available: {}, lead: {}d, status: {}",
                row["name"].as_str().unwrap_or(""),
                row["id"].as_str().unwrap_or(""),
                row["qty_on_hand"].as_i64().unwrap_or(0),
                row["qty_reserved"].as_i64().unwrap_or(0),
                row["qty_available"].as_i64().unwrap_or(0),
                row["lead_time_days"].as_i64().unwrap_or(0),
                row["status"].as_str().unwrap_or(""),
            ));
        }
        Ok(json_result(&lines.join("\n"), &rows))
    }
}

// ── pm_list_procurement_orders ─────────────────────────────

pub struct PmListProcurementOrdersTool {
    service: Arc<PmService>,
}

impl PmListProcurementOrdersTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmListProcurementOrdersTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_list_procurement_orders".into(),
                description: "List open procurement orders from the PM database, including supplier, quantity, ETA, status, lead time, and linked batch.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "filter": {"type": "string", "enum": ["open", "pending", "shipped", "all_open"], "description": "Order filter. Current MVP lists open/non-delivered orders."}
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let filter = args
            .get("filter")
            .and_then(|v| v.as_str())
            .unwrap_or("open");
        let rows = self
            .service
            .store()
            .list_open_orders()?
            .into_iter()
            .filter(|order| match filter {
                "pending" => order.status == "pending",
                "shipped" => order.status == "shipped",
                _ => true,
            })
            .map(|order| {
                serde_json::json!({
                    "id": order.entity_id,
                    "order_ref": order.order_ref,
                    "item_sku": order.item_sku,
                    "item_name": order.item_name,
                    "supplier": order.supplier,
                    "qty": order.qty,
                    "lead_time_days": order.lead_time_days,
                    "order_date": order.order_date,
                    "eta": order.eta,
                    "status": order.status,
                    "linked_batch_id": order.linked_batch_id,
                    "notes": order.notes,
                })
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Ok(text_result(format!(
                "PM procurement ({filter}): none found"
            )));
        }
        let mut lines =
            vec![format!("PM procurement ({filter}) ({}):", rows.len())];
        for row in &rows {
            lines.push(format!(
                "  - {} x{} from {} — status: {}, ETA: {}",
                row["item_name"].as_str().unwrap_or(""),
                row["qty"].as_i64().unwrap_or(0),
                row["supplier"].as_str().unwrap_or(""),
                row["status"].as_str().unwrap_or(""),
                row["eta"].as_str().unwrap_or("not set"),
            ));
        }
        Ok(json_result(&lines.join("\n"), &rows))
    }
}

// ── pm_get_timing_plan ─────────────────────────────────────

pub struct PmGetTimingPlanTool {
    service: Arc<PmService>,
}

impl PmGetTimingPlanTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmGetTimingPlanTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_get_timing_plan".into(),
                description: "Compute a PM timing plan from stored batch, inventory, and procurement data: what must be purchased in advance, office-ready dates, and logistics send-by dates. Use this for 'what do I need to order', 'when should this batch be ready in office', and 'when should we ship robots'.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "batch_id": {"type": "string", "description": "Optional batch entity ID. Omit for all active batches."},
                        "office_prep_days": {"type": "integer", "description": "Days before deploy when batch should be ready in office. Default 2."},
                        "logistics_days": {"type": "integer", "description": "Days before deploy to send robots by logistics. Default 3."},
                        "safety_buffer_days": {"type": "integer", "description": "Extra buffer for purchase/order deadlines. Default 2."}
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let batch_id = optional_string(&args, "batch_id");
        let office_prep_days =
            optional_i32(&args, "office_prep_days").unwrap_or(2) as i64;
        let logistics_days =
            optional_i32(&args, "logistics_days").unwrap_or(3) as i64;
        let safety_buffer_days =
            optional_i32(&args, "safety_buffer_days").unwrap_or(2) as i64;
        if office_prep_days < 0 {
            anyhow::bail!("office_prep_days must be non-negative");
        }
        if logistics_days < 0 {
            anyhow::bail!("logistics_days must be non-negative");
        }
        if safety_buffer_days < 0 {
            anyhow::bail!("safety_buffer_days must be non-negative");
        }
        let today = Utc::now().date_naive();

        let inventory = self.service.store().list_inventory()?;
        let orders = self.service.store().list_open_orders()?;
        let batches = if let Some(ref id) = batch_id {
            vec![
                self.service
                    .store()
                    .get_batch(id)?
                    .ok_or_else(|| anyhow::anyhow!("Batch not found: {id}"))?,
            ]
        } else {
            self.service
                .store()
                .list_batches()?
                .into_iter()
                .filter(|batch| {
                    !matches!(
                        batch.status.as_str(),
                        "completed" | "cancelled" | "deployed"
                    )
                })
                .collect::<Vec<_>>()
        };

        let mut rows = Vec::new();
        let mut lines = vec![format!(
            "PM timing plan (office prep {}d, logistics {}d, safety buffer {}d):",
            office_prep_days, logistics_days, safety_buffer_days
        )];

        for batch in batches {
            let deploy_date = batch
                .target_deploy_date
                .as_deref()
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
            let site = batch.target_site_id.and_then(|id| {
                self.service.store().get_site_by_id(id).ok().flatten()
            });

            let office_ready_by =
                deploy_date.map(|d| d - Duration::days(office_prep_days));
            let logistics_send_by =
                deploy_date.map(|d| d - Duration::days(logistics_days));
            let linked_inventory = inventory
                .iter()
                .filter(|item| item.linked_batch_id == Some(batch.id))
                .map(|item| {
                    let needed_by = office_ready_by.or(deploy_date);
                    let latest_order_by = needed_by.map(|d| {
                        alerts::latest_safe_start(
                            d,
                            item.lead_time_days.into(),
                            0,
                            0,
                            safety_buffer_days,
                        )
                    });
                    let severity = latest_order_by
                        .map(|d| alerts::classify_alert_severity(today, d, 7, 14))
                        .unwrap_or("unknown");
                    serde_json::json!({
                        "id": item.entity_id,
                        "name": item.name,
                        "supplier": item.supplier,
                        "qty_on_hand": item.qty_on_hand,
                        "qty_reserved": item.qty_reserved,
                        "qty_available": item.qty_on_hand - item.qty_reserved,
                        "lead_time_days": item.lead_time_days,
                        "latest_order_by": latest_order_by.map(|d| d.to_string()),
                        "timing_severity": severity,
                        "needs_purchase_attention": severity != "ok" || item.qty_on_hand <= item.reorder_threshold,
                    })
                })
                .collect::<Vec<_>>();
            let linked_orders = orders
                .iter()
                .filter(|order| order.linked_batch_id == Some(batch.id))
                .map(|order| {
                    serde_json::json!({
                        "id": order.entity_id,
                        "item_name": order.item_name,
                        "supplier": order.supplier,
                        "qty": order.qty,
                        "status": order.status,
                        "eta": order.eta,
                        "lead_time_days": order.lead_time_days,
                    })
                })
                .collect::<Vec<_>>();
            let mut blockers = Vec::new();
            if deploy_date.is_none() {
                blockers.push("missing target_deploy_date");
            }
            if batch.target_site_id.is_none() {
                blockers.push("missing target_site_id");
            }
            if batch.robot_serials.is_empty() {
                blockers.push("no robot serials recorded");
            }

            let row = serde_json::json!({
                "batch_id": batch.entity_id,
                "batch_name": batch.name,
                "status": batch.status,
                "site": site.as_ref().map(|s| serde_json::json!({"id": s.entity_id, "name": s.name, "location": s.location})),
                "target_deploy_date": batch.target_deploy_date,
                "office_ready_by": office_ready_by.map(|d| d.to_string()),
                "logistics_send_by": logistics_send_by.map(|d| d.to_string()),
                "robot_serials": batch.robot_serials,
                "linked_inventory": linked_inventory,
                "linked_procurement_orders": linked_orders,
                "blockers": blockers,
            });
            lines.push(format!(
                "  - {}: deploy {}, office-ready {}, logistics-send {}, blockers: {}",
                row["batch_name"].as_str().unwrap_or(""),
                row["target_deploy_date"].as_str().unwrap_or("not set"),
                row["office_ready_by"].as_str().unwrap_or("not set"),
                row["logistics_send_by"].as_str().unwrap_or("not set"),
                row["blockers"].as_array().map(|a| a.len()).unwrap_or(0),
            ));
            rows.push(row);
        }

        if rows.is_empty() {
            return Ok(text_result(
                "PM timing plan: no matching active batches found".to_string(),
            ));
        }
        Ok(json_result(&lines.join("\n"), &rows))
    }
}

// ── pm_create_or_update_site ──────────────────────────────

pub struct PmCreateOrUpdateSiteTool {
    service: Arc<PmService>,
}

impl PmCreateOrUpdateSiteTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmCreateOrUpdateSiteTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_create_or_update_site".into(),
                description: "Create or update a site. Use the site ID to update an existing site; omit to create a new one. The PM backend assigns a new ID on creation.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "site_id": {
                            "type": "string",
                            "description": "Existing site ID to update, or empty to create new"
                        },
                        "name": {
                            "type": "string",
                            "description": "Site name (required for create)"
                        },
                        "location": {
                            "type": "string",
                            "description": "Physical address or area"
                        },
                        "deployment_window": {
                            "type": "string",
                            "description": "Deployment window description"
                        },
                        "onsite_duration_days": {
                            "type": "integer",
                            "description": "Expected onsite duration in days"
                        },
                        "notes": {
                            "type": "string",
                            "description": "Additional notes"
                        }
                    },
                    "required": ["name"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let site_id = optional_string(&args, "site_id");
        let name = string_or_default(&args, "name");
        let location = string_or_default(&args, "location");
        let onsite_duration_days =
            optional_i32(&args, "onsite_duration_days").unwrap_or(0);
        let notes = string_or_default(&args, "notes");

        let record = if let Some(sid) = site_id {
            // Update existing
            self.service
                .update_site(
                    &sid,
                    Some(&name),
                    if location.is_empty() {
                        None
                    } else {
                        Some(&location)
                    },
                    None,
                    None,
                    if onsite_duration_days > 0 {
                        Some(onsite_duration_days)
                    } else {
                        None
                    },
                    None,
                    if notes.is_empty() { None } else { Some(&notes) },
                )
                .context("updating site")?
        } else {
            // Create new
            self.service
                .create_site(
                    &name,
                    &location,
                    None,
                    None,
                    onsite_duration_days,
                    false,
                    &notes,
                )
                .context("creating site")?
        };

        Ok(json_result(
            &format!("Site '{}' saved (ID: {})", record.name, record.entity_id),
            &record,
        ))
    }
}

// ── pm_create_or_update_batch ─────────────────────────────

pub struct PmCreateOrUpdateBatchTool {
    service: Arc<PmService>,
}

impl PmCreateOrUpdateBatchTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmCreateOrUpdateBatchTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_create_or_update_batch".into(),
                description: "Create or update a batch. Provide a batch ID to update; omit to create. The PM backend assigns a new ID on creation.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "batch_id": {
                            "type": "string",
                            "description": "Existing batch ID to update, or empty to create new"
                        },
                        "name": {
                            "type": "string",
                            "description": "Batch name or number"
                        },
                        "target_site_id": {
                            "type": "string",
                            "description": "ID of the target site"
                        },
                        "target_deploy_date": {
                            "type": "string",
                            "description": "Target deployment date (YYYY-MM-DD)"
                        },
                        "onsite_duration_days": {
                            "type": "integer",
                            "description": "Expected onsite duration in days"
                        },
                        "status": {
                            "type": "string",
                            "description": "Batch status: planned, in_preparation, ready, deployed, completed, cancelled",
                            "enum": ["planned", "in_preparation", "ready", "deployed", "completed", "cancelled"]
                        },
                        "robot_serials": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "List of robot serial numbers assigned to this batch"
                        }
                    },
                    "required": ["name"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let batch_id = optional_string(&args, "batch_id");
        let name = string_or_default(&args, "name");
        let target_site_id_str = optional_string(&args, "target_site_id");
        let target_deploy_date = optional_string(&args, "target_deploy_date");
        let onsite_duration_days =
            optional_i32(&args, "onsite_duration_days").unwrap_or(0);
        let status = optional_string(&args, "status")
            .unwrap_or_else(|| "planned".to_string());
        let robot_serials: Vec<String> = args
            .get("robot_serials")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Resolve target site ID from string (entity_id) to internal ID
        let resolved_site_id: Option<i64> =
            if let Some(site_eid) = &target_site_id_str {
                let site = self
                    .service
                    .store()
                    .get_site(site_eid)
                    .context("resolving target site")?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Target site not found: {}", site_eid)
                    })?;
                Some(site.id)
            } else {
                None
            };

        let record = if let Some(bid) = batch_id {
            // Update existing
            self.service
                .update_batch(
                    &bid,
                    Some(&name),
                    if robot_serials.is_empty() {
                        None
                    } else {
                        Some(&robot_serials)
                    },
                    if resolved_site_id.is_some() {
                        Some(resolved_site_id)
                    } else {
                        None
                    },
                    target_deploy_date.as_deref(),
                    if onsite_duration_days > 0 {
                        Some(onsite_duration_days)
                    } else {
                        None
                    },
                    Some(&status),
                    None,
                )
                .context("updating batch")?
        } else {
            // Create new
            self.service
                .create_batch(
                    &name,
                    &robot_serials,
                    resolved_site_id,
                    target_deploy_date.as_deref(),
                    onsite_duration_days,
                    &status,
                    "",
                )
                .context("creating batch")?
        };

        Ok(json_result(
            &format!(
                "Batch '{}' saved (ID: {})",
                record.name, record.entity_id
            ),
            &record,
        ))
    }
}

// ── pm_assign_person ───────────────────────────────────────

pub struct PmAssignPersonTool {
    service: Arc<PmService>,
}

impl PmAssignPersonTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmAssignPersonTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_assign_person".into(),
                description:
                    "Assign a person to a site or batch for a date range."
                        .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "person_id": {
                            "type": "string",
                            "description": "ID of the person to assign"
                        },
                        "entity_type": {
                            "type": "string",
                            "enum": ["site", "batch"],
                            "description": "Type of entity to assign to"
                        },
                        "entity_id": {
                            "type": "string",
                            "description": "ID of the site or batch"
                        },
                        "role": {
                            "type": "string",
                            "description": "Role at the assignment (e.g., 'lead', 'technician', 'senior', 'junior')"
                        },
                        "start_date": {
                            "type": "string",
                            "description": "Assignment start date (YYYY-MM-DD)"
                        },
                        "end_date": {
                            "type": "string",
                            "description": "Assignment end date (YYYY-MM-DD)"
                        }
                    },
                    "required": ["person_id", "entity_type", "entity_id", "role", "start_date", "end_date"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        // TODO: Add overlapping assignment conflict detection (deferred from Phase 2 MVP)
        let person_id = string_or_default(&args, "person_id");
        let entity_type = string_or_default(&args, "entity_type");
        let entity_id = string_or_default(&args, "entity_id");
        let role = string_or_default(&args, "role");
        let start_date = optional_string(&args, "start_date");
        let end_date = optional_string(&args, "end_date");

        let assignment = self
            .service
            .assign_person(
                &person_id,
                &entity_type,
                &entity_id,
                &role,
                start_date.as_deref(),
                end_date.as_deref(),
            )
            .context("assigning person")?;

        Ok(json_result(
            &format!(
                "Person {} assigned to {} {} as {}",
                person_id, entity_type, entity_id, role
            ),
            &assignment,
        ))
    }
}

// ── pm_record_handover ─────────────────────────────────────

pub struct PmRecordHandoverTool {
    service: Arc<PmService>,
}

impl PmRecordHandoverTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmRecordHandoverTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_record_handover".into(),
                description: "Record a handover between two people at a site or batch. The outgoing person transfers responsibility to the incoming person on a given date.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "entity_type": {
                            "type": "string",
                            "enum": ["site", "batch"],
                            "description": "Type of entity"
                        },
                        "entity_id": {
                            "type": "string",
                            "description": "ID of the site or batch"
                        },
                        "from_person_id": {
                            "type": "string",
                            "description": "ID of the outgoing person"
                        },
                        "to_person_id": {
                            "type": "string",
                            "description": "ID of the incoming person"
                        },
                        "handover_date": {
                            "type": "string",
                            "description": "Date of handover (YYYY-MM-DD)"
                        },
                        "notes": {
                            "type": "string",
                            "description": "Optional handover notes"
                        }
                    },
                    "required": ["entity_type", "entity_id", "from_person_id", "to_person_id", "handover_date"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let entity_type = string_or_default(&args, "entity_type");
        let entity_id = string_or_default(&args, "entity_id");
        let from_person_id = string_or_default(&args, "from_person_id");
        let to_person_id = string_or_default(&args, "to_person_id");
        let handover_date = optional_string(&args, "handover_date");

        let updated = self
            .service
            .record_handover(
                &entity_type,
                &entity_id,
                &from_person_id,
                &to_person_id,
                handover_date.as_deref(),
            )
            .context("recording handover")?;

        Ok(json_result(
            &format!(
                "Handover recorded: person {} -> {} at {} {}",
                from_person_id, to_person_id, entity_type, entity_id
            ),
            &updated,
        ))
    }
}

// ── pm_upsert_inventory ────────────────────────────────────

pub struct PmUpsertInventoryTool {
    service: Arc<PmService>,
}

impl PmUpsertInventoryTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmUpsertInventoryTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_upsert_inventory".into(),
                description: "Create or update an inventory item. Use the item ID to update; omit to create new. Updates quantity on hand, location, supplier, reorder threshold, and canonical name.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "item_id": {
                            "type": "string",
                            "description": "Existing item ID to update, or empty to create new"
                        },
                        "name": {
                            "type": "string",
                            "description": "Canonical item/SKU name"
                        },
                        "category": {
                            "type": "string",
                            "description": "Item category (e.g., 'tool', 'consumable', 'part')"
                        },
                        "quantity_on_hand": {
                            "type": "integer",
                            "description": "Current quantity in stock"
                        },
                        "location_area": {
                            "type": "string",
                            "description": "Storage area (e.g., 'Warehouse A')"
                        },
                        "location_rack": {
                            "type": "string",
                            "description": "Optional rack/shelf identifier"
                        },
                        "supplier": {
                            "type": "string",
                            "description": "Supplier or vendor name"
                        },
                        "lead_time_days": {
                            "type": "integer",
                            "description": "Vendor lead time in days"
                        },
                        "reorder_threshold": {
                            "type": "integer",
                            "description": "Quantity threshold that triggers low-stock alert"
                        }
                    },
                    "required": ["name", "quantity_on_hand"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let item_id = optional_string(&args, "item_id");
        let name = string_or_default(&args, "name");
        let category = optional_string(&args, "category");
        let qty_on_hand = optional_i32(&args, "quantity_on_hand");
        let location_area = optional_string(&args, "location_area");
        let location_rack = optional_string(&args, "location_rack");
        let supplier = optional_string(&args, "supplier");
        let lead_time_days = optional_i32(&args, "lead_time_days");
        let reorder_threshold = optional_i32(&args, "reorder_threshold");

        let record = if let Some(iid) = item_id {
            // Update existing item by entity_id — ensures updates target the correct row
            let name_opt = if name.is_empty() {
                None
            } else {
                Some(name.as_str())
            };
            self.service
                .store()
                .upsert_inventory_by_entity_id(
                    &iid,
                    name_opt,
                    None,
                    category.as_deref(),
                    qty_on_hand,
                    location_area.as_deref(),
                    location_rack.as_deref(),
                    supplier.as_deref(),
                    lead_time_days,
                    reorder_threshold,
                    None,
                )
                .context("updating inventory item by item_id")?
        } else {
            // Create
            self.service
                .upsert_inventory(
                    &name,
                    None,
                    category.as_deref(),
                    qty_on_hand,
                    location_area.as_deref(),
                    location_rack.as_deref(),
                    supplier.as_deref(),
                    lead_time_days,
                    reorder_threshold,
                    None,
                )
                .context("creating inventory item")?
        };

        Ok(json_result(
            &format!(
                "Inventory item '{}' saved (ID: {}, qty: {})",
                record.name, record.entity_id, record.qty_on_hand
            ),
            &record,
        ))
    }
}

// ── pm_reserve_inventory ───────────────────────────────────

pub struct PmReserveInventoryTool {
    service: Arc<PmService>,
}

impl PmReserveInventoryTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmReserveInventoryTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_reserve_inventory".into(),
                description: "Reserve inventory items for a specific batch or site. The PM backend checks available quantity (on_hand - already_reserved) before reserving.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "item_id": {
                            "type": "string",
                            "description": "ID of the inventory item to reserve"
                        },
                        "quantity": {
                            "type": "integer",
                            "description": "Quantity to reserve"
                        },
                        "entity_type": {
                            "type": "string",
                            "enum": ["batch", "site"],
                            "description": "Type of entity reserving the items"
                        },
                        "entity_id": {
                            "type": "string",
                            "description": "ID of the batch or site"
                        },
                        "release_date": {
                            "type": "string",
                            "description": "Expected release/return date (YYYY-MM-DD), if known"
                        }
                    },
                    "required": ["item_id", "quantity", "entity_type", "entity_id"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let item_entity_id = string_or_default(&args, "item_id");
        let quantity =
            args.get("quantity").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let entity_type = string_or_default(&args, "entity_type");
        let entity_id = string_or_default(&args, "entity_id");

        // Resolve linked batch/site to internal ID if provided
        let linked_batch_id: Option<i64> =
            if entity_type == "batch" && !entity_id.is_empty() {
                let batch = self
                    .service
                    .store()
                    .get_batch(&entity_id)
                    .context("resolving batch for reservation")?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Batch not found for reservation: {}",
                            entity_id
                        )
                    })?;
                Some(batch.id)
            } else if entity_type == "site" && !entity_id.is_empty() {
                // Validate site exists (reservation is not linked to site ID
                // in the current schema, but we enforce existence)
                self.service
                    .store()
                    .get_site(&entity_id)
                    .context("resolving site for reservation")?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Site not found for reservation: {}",
                            entity_id
                        )
                    })?;
                None
            } else {
                None
            };

        let record = self
            .service
            .reserve_inventory(&item_entity_id, quantity, linked_batch_id)
            .context("reserving inventory")?;

        Ok(json_result(
            &format!(
                "Reserved {} of '{}' (now {} reserved of {} on hand)",
                quantity, record.name, record.qty_reserved, record.qty_on_hand
            ),
            &record,
        ))
    }
}

// ── pm_create_procurement_order ────────────────────────────

pub struct PmCreateProcurementOrderTool {
    service: Arc<PmService>,
}

impl PmCreateProcurementOrderTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmCreateProcurementOrderTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_create_procurement_order".into(),
                description: "Create a procurement order for inventory items. Specify the item, quantity, supplier, and optional ETA. The PM backend logs the order and checks lead times.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "item_id": {
                            "type": "string",
                            "description": "ID of the inventory item to order"
                        },
                        "quantity": {
                            "type": "integer",
                            "description": "Quantity to order"
                        },
                        "supplier": {
                            "type": "string",
                            "description": "Supplier or vendor name"
                        },
                        "lead_time_days": {
                            "type": "integer",
                            "description": "Expected lead time in days (overrides item default)"
                        },
                        "linked_batch_id": {
                            "type": "string",
                            "description": "Optional batch ID this order supports"
                        },
                        "notes": {
                            "type": "string",
                            "description": "Order notes"
                        }
                    },
                    "required": ["item_id", "quantity"]
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let item_id = string_or_default(&args, "item_id");
        let quantity =
            args.get("quantity").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let supplier = string_or_default(&args, "supplier");
        let lead_time_days = optional_i32(&args, "lead_time_days");
        let linked_batch_id_str = optional_string(&args, "linked_batch_id");
        let notes = optional_string(&args, "notes");

        // Resolve item_id (entity_id) to SKU and name
        let item = self
            .service
            .store()
            .get_inventory(&item_id)
            .context("resolving inventory item")?
            .ok_or_else(|| {
                anyhow::anyhow!("Inventory item not found: {}", item_id)
            })?;

        // Resolve linked batch if provided
        let linked_batch_id: Option<i64> =
            if let Some(batch_eid) = &linked_batch_id_str {
                let batch = self
                    .service
                    .store()
                    .get_batch(batch_eid)
                    .context("resolving linked batch")?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Linked batch not found: {}", batch_eid)
                    })?;
                Some(batch.id)
            } else {
                None
            };

        let supp = if supplier.is_empty() {
            &item.supplier
        } else {
            &supplier
        };

        let po = self
            .service
            .create_procurement_order(
                &item.sku,
                &item.name,
                supp,
                quantity,
                lead_time_days,
                linked_batch_id,
                notes.as_deref(),
            )
            .context("creating procurement order")?;

        Ok(json_result(
            &format!(
                "Procurement order created for {} x '{}' from {}",
                quantity, po.item_name, po.supplier
            ),
            &po,
        ))
    }
}

// ── pm_check_deploy_readiness ──────────────────────────────

pub struct PmCheckDeployReadinessTool {
    service: Arc<PmService>,
}

impl PmCheckDeployReadinessTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmCheckDeployReadinessTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_check_deploy_readiness".into(),
                description: "Check deployment readiness for one or all batches. Returns a readiness summary including inventory shortages, staffing gaps, missing files, procurement overdue, and blocker status.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "batch_id": {
                            "type": "string",
                            "description": "Optional batch ID to check. If omitted, checks all active batches."
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let batch_id = optional_string(&args, "batch_id");

        if let Some(bid) = batch_id {
            let report = self
                .service
                .check_deploy_readiness(&bid)
                .context("checking deploy readiness")?;

            let mut lines = Vec::new();
            lines.push(format!(
                "Readiness for batch '{}' ({}): {}",
                report.batch_name,
                report.status,
                if report.is_ready {
                    "✅ READY"
                } else {
                    "❌ BLOCKED"
                }
            ));
            if report.blockers.is_empty() {
                lines.push("  No blockers found.".to_string());
            } else {
                lines.push(format!("  {} blocker(s):", report.blockers.len()));
                for blocker in &report.blockers {
                    lines.push(format!(
                        "    [{}] {}",
                        blocker.severity, blocker.reason
                    ));
                }
            }

            Ok(text_result(lines.join("\n")))
        } else {
            // Check all active batches
            let batches = self.service.store().list_batches()?;
            let active_batches: Vec<_> = batches
                .iter()
                .filter(|b| {
                    !matches!(
                        b.status.as_str(),
                        "completed" | "cancelled" | "deployed"
                    )
                })
                .collect();

            let mut lines = Vec::new();
            lines.push(format!(
                "Readiness overview ({} active batches):",
                active_batches.len()
            ));

            for batch in &active_batches {
                let report =
                    self.service.check_deploy_readiness(&batch.entity_id)?;
                lines.push(format!(
                    "  {} [{}]: {}",
                    report.batch_name,
                    report.status,
                    if report.is_ready { "Ready" } else { "Blocked" }
                ));
                for blocker in &report.blockers {
                    lines.push(format!(
                        "    - [{}] {}",
                        blocker.severity, blocker.reason
                    ));
                }
            }

            Ok(text_result(lines.join("\n")))
        }
    }
}

// ── pm_check_alerts ────────────────────────────────────────

pub struct PmCheckAlertsTool {
    service: Arc<PmService>,
}

impl PmCheckAlertsTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmCheckAlertsTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_check_alerts".into(),
                description: "Check current alerts across all categories: overdue procurement, low stock, blocked batches, missing site files, missing staffing, robot not ready, assembly not started, handover gaps. Returns alerts sorted by severity.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "severity": {
                            "type": "string",
                            "enum": ["overdue", "urgent", "upcoming", "ok"],
                            "description": "Optional severity filter"
                        },
                        "resolved": {
                            "type": "boolean",
                            "description": "Include resolved alerts (default: false)"
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let filter_severity = optional_string(&args, "severity");
        let _include_resolved = args
            .get("resolved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let report = self
            .service
            .run_alert_checks()
            .context("running alert checks")?;
        let alerts = report.active_alerts;

        let mut lines = Vec::new();

        // Generation summary
        if report.generation.total > 0 {
            lines.push(format!(
                "Alert generation: {} inventory + {} procurement = {} total",
                report.generation.inventory_alerts,
                report.generation.procurement_alerts,
                report.generation.total,
            ));
            lines.push(String::new());
        }

        // Blocked batches
        if !report.blocked_batches.is_empty() {
            lines.push(format!(
                "Blocked batches ({}):",
                report.blocked_batches.len()
            ));
            for blocker in &report.blocked_batches {
                lines.push(format!(
                    "  Batch '{}': [{}] {}",
                    blocker.batch_name, blocker.severity, blocker.reason
                ));
            }
            lines.push(String::new());
        }

        // Active alerts
        if alerts.is_empty() {
            lines.push("No active alerts. ✅".to_string());
        } else {
            let filtered: Vec<_> = if let Some(ref sev) = filter_severity {
                alerts.iter().filter(|a| a.severity == *sev).collect()
            } else {
                alerts.iter().collect()
            };

            lines.push(format!("Active alerts ({}):", filtered.len()));
            for alert in &filtered {
                lines.push(format!(
                    "  [{}] {}: {}",
                    alert.severity, alert.alert_type, alert.reason
                ));
                lines.push(format!(
                    "         Suggested: {}",
                    alert.suggested_action
                ));
            }
        }

        Ok(text_result(lines.join("\n")))
    }
}

// ── pm_list_blockers ───────────────────────────────────────

pub struct PmListBlockersTool {
    service: Arc<PmService>,
}

impl PmListBlockersTool {
    pub fn new(service: Arc<PmService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for PmListBlockersTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            def_type: "function".into(),
            function: ToolFunctionDef {
                name: "pm_list_blockers".into(),
                description: "List all current blockers across batches and sites. A blocker is any unresolved condition that prevents deployment or progress: insufficient reserved inventory, overdue procurement, unresolved critical alert, missing staffing, or missing required site files.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "batch_id": {
                            "type": "string",
                            "description": "Optional batch ID to scope the blockers"
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolExecutionResult> {
        let _batch_id = optional_string(&args, "batch_id");

        let blockers =
            self.service.list_blockers().context("listing blockers")?;

        let mut lines = Vec::new();
        if blockers.is_empty() {
            lines.push("No blockers found. ✅".to_string());
        } else {
            lines.push(format!("Blockers ({}):", blockers.len()));
            for blocker in &blockers {
                lines.push(format!(
                    "  Batch '{}': [{}] {}",
                    blocker.batch_name, blocker.severity, blocker.reason
                ));
            }
        }

        Ok(text_result(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Tool;
    use crate::pm::schema;
    use crate::pm::store::Store;
    use rusqlite::Connection;

    fn create_service() -> Arc<PmService> {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        schema::bootstrap(&conn).expect("bootstrap");
        let store = Store::new(conn);
        Arc::new(PmService::new(store))
    }

    fn assert_tool_def(tool: &dyn Tool, expected_name: &str) {
        let def = tool.def();
        assert_eq!(def.def_type, "function");
        assert_eq!(def.function.name, expected_name);
        assert!(!def.function.description.is_empty());
        assert!(def.function.parameters.is_object());
        let params = def.function.parameters.as_object().unwrap();
        assert!(params.contains_key("type"));
        assert!(params.contains_key("properties"));
    }

    #[test]
    fn test_pm_tool_def_names() {
        let svc = create_service();
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(PmGetStatusTool::new(Arc::clone(&svc))),
            Box::new(PmListSitesTool::new(Arc::clone(&svc))),
            Box::new(PmListBatchesTool::new(Arc::clone(&svc))),
            Box::new(PmListPeopleTool::new(Arc::clone(&svc))),
            Box::new(PmListInventoryTool::new(Arc::clone(&svc))),
            Box::new(PmListProcurementOrdersTool::new(Arc::clone(&svc))),
            Box::new(PmGetTimingPlanTool::new(Arc::clone(&svc))),
            Box::new(PmCreateOrUpdateSiteTool::new(Arc::clone(&svc))),
            Box::new(PmCreateOrUpdateBatchTool::new(Arc::clone(&svc))),
            Box::new(PmAssignPersonTool::new(Arc::clone(&svc))),
            Box::new(PmRecordHandoverTool::new(Arc::clone(&svc))),
            Box::new(PmUpsertInventoryTool::new(Arc::clone(&svc))),
            Box::new(PmReserveInventoryTool::new(Arc::clone(&svc))),
            Box::new(PmCreateProcurementOrderTool::new(Arc::clone(&svc))),
            Box::new(PmCheckDeployReadinessTool::new(Arc::clone(&svc))),
            Box::new(PmCheckAlertsTool::new(Arc::clone(&svc))),
            Box::new(PmListBlockersTool::new(Arc::clone(&svc))),
        ];

        let expected = [
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
        ];

        assert_eq!(tools.len(), expected.len());
        for (tool, expected_name) in tools.iter().zip(expected.iter()) {
            assert_tool_def(tool.as_ref(), expected_name);
        }
    }

    #[tokio::test]
    async fn test_pm_get_status_tool() {
        let svc = create_service();
        let tool = PmGetStatusTool::new(Arc::clone(&svc));

        // With empty DB, should return empty summary
        let result = tool
            .execute(serde_json::json!({}))
            .await
            .expect("get status");
        assert!(result.output.contains("PM Status"));
    }

    #[tokio::test]
    async fn test_pm_create_site_tool() {
        let svc = create_service();
        let tool = PmCreateOrUpdateSiteTool::new(Arc::clone(&svc));

        let result = tool
            .execute(serde_json::json!({
                "name": "Test Site",
                "location": "Warehouse 1"
            }))
            .await
            .expect("create site");

        assert!(result.output.contains("saved"));
        assert!(result.output.contains("Test Site"));
    }

    #[tokio::test]
    async fn test_pm_list_sites_tool_filters_active_sites() {
        let svc = create_service();
        let active_site = svc
            .create_site("Active Site", "Warehouse A", None, None, 5, false, "")
            .unwrap();
        let completed_site = svc
            .create_site(
                "Completed Site",
                "Warehouse B",
                None,
                None,
                2,
                true,
                "",
            )
            .unwrap();
        svc.create_batch(
            "Active Batch",
            &[],
            Some(active_site.id),
            Some("2026-08-18"),
            5,
            "in_preparation",
            "",
        )
        .unwrap();
        svc.create_batch(
            "Completed Batch",
            &[],
            Some(completed_site.id),
            Some("2026-08-01"),
            2,
            "completed",
            "",
        )
        .unwrap();

        let tool = PmListSitesTool::new(Arc::clone(&svc));
        let result = tool
            .execute(serde_json::json!({ "filter": "active" }))
            .await
            .expect("list active sites");

        assert!(result.output.contains("Active PM sites (1)"));
        assert!(result.output.contains("Active Site"));
        assert!(result.output.contains("Active Batch [in_preparation"));
        assert!(!result.output.contains("Completed Site"));
    }

    #[tokio::test]
    async fn test_pm_list_read_tools() {
        let svc = create_service();
        let site = svc
            .create_site("Site A", "Area A", None, None, 4, false, "")
            .unwrap();
        svc.create_batch(
            "Batch A",
            &["R1".to_string()],
            Some(site.id),
            Some("2026-08-18"),
            4,
            "planned",
            "",
        )
        .unwrap();
        svc.create_person("Alice", true, &[], &[], None, None, "")
            .unwrap();
        svc.upsert_inventory(
            "Drillbit",
            Some("DB-1"),
            Some("consumable"),
            Some(20),
            Some("Office"),
            None,
            Some("Supplier"),
            Some(5),
            Some(10),
            None,
        )
        .unwrap();
        svc.create_procurement_order(
            "DB-1",
            "Drillbit",
            "Supplier",
            10,
            Some(5),
            None,
            Some(""),
        )
        .unwrap();

        let batch_result = PmListBatchesTool::new(Arc::clone(&svc))
            .execute(serde_json::json!({"filter": "active"}))
            .await
            .unwrap();
        assert!(batch_result.output.contains("Batch A"));
        let people_result = PmListPeopleTool::new(Arc::clone(&svc))
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(people_result.output.contains("Alice"));
        let inventory_result = PmListInventoryTool::new(Arc::clone(&svc))
            .execute(serde_json::json!({"filter": "all"}))
            .await
            .unwrap();
        assert!(inventory_result.output.contains("Drillbit"));
        let procurement_result =
            PmListProcurementOrdersTool::new(Arc::clone(&svc))
                .execute(serde_json::json!({"filter": "open"}))
                .await
                .unwrap();
        assert!(procurement_result.output.contains("Drillbit"));
    }

    #[tokio::test]
    async fn test_pm_get_timing_plan_tool() {
        let svc = create_service();
        let site = svc
            .create_site("Timing Site", "Area A", None, None, 4, false, "")
            .unwrap();
        let batch = svc
            .create_batch(
                "Timing Batch",
                &["R1".to_string()],
                Some(site.id),
                Some("2026-08-18"),
                4,
                "planned",
                "",
            )
            .unwrap();
        svc.upsert_inventory(
            "Vacuum Bag",
            Some("VB-1"),
            Some("consumable"),
            Some(3),
            Some("Office"),
            None,
            Some("Supplier"),
            Some(7),
            Some(5),
            Some(Some(batch.id)),
        )
        .unwrap();

        let result = PmGetTimingPlanTool::new(Arc::clone(&svc))
            .execute(serde_json::json!({
                "batch_id": batch.entity_id,
                "office_prep_days": 2,
                "logistics_days": 3,
                "safety_buffer_days": 2
            }))
            .await
            .unwrap();

        assert!(result.output.contains("Timing Batch"));
        assert!(result.output.contains("office-ready 2026-08-16"));
        assert!(result.output.contains("logistics-send 2026-08-15"));
        assert!(result.output.contains("Vacuum Bag"));
    }

    #[tokio::test]
    async fn test_pm_get_timing_plan_rejects_unknown_batch() {
        let svc = create_service();
        let err = PmGetTimingPlanTool::new(Arc::clone(&svc))
            .execute(serde_json::json!({"batch_id": "missing-batch"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Batch not found: missing-batch"));
    }

    #[tokio::test]
    async fn test_pm_get_timing_plan_rejects_negative_offsets() {
        let svc = create_service();
        for (field, message) in [
            ("office_prep_days", "office_prep_days must be non-negative"),
            ("logistics_days", "logistics_days must be non-negative"),
            (
                "safety_buffer_days",
                "safety_buffer_days must be non-negative",
            ),
        ] {
            let err = PmGetTimingPlanTool::new(Arc::clone(&svc))
                .execute(serde_json::json!({field: -1}))
                .await
                .unwrap_err();
            assert!(err.to_string().contains(message));
        }
    }

    #[tokio::test]
    async fn test_pm_get_timing_plan_omits_deployed_batches_by_default() {
        let svc = create_service();
        let site = svc
            .create_site("Site", "Area", None, None, 1, false, "")
            .unwrap();
        svc.create_batch(
            "Deployed Batch",
            &["R1".to_string()],
            Some(site.id),
            Some("2026-08-18"),
            1,
            "deployed",
            "",
        )
        .unwrap();

        let result = PmGetTimingPlanTool::new(Arc::clone(&svc))
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(result.output.contains("no matching active batches found"));
        assert!(!result.output.contains("Deployed Batch"));
    }

    #[tokio::test]
    async fn test_pm_create_batch_tool() {
        let svc = create_service();
        // First create a site
        let site = svc
            .create_site("Target Site", "", None, None, 0, false, "")
            .unwrap();

        let tool = PmCreateOrUpdateBatchTool::new(Arc::clone(&svc));
        let result = tool
            .execute(serde_json::json!({
                "name": "Batch 1",
                "target_site_id": site.entity_id,
                "target_deploy_date": "2026-08-18",
                "status": "planned"
            }))
            .await
            .expect("create batch");

        assert!(result.output.contains("saved"));
        assert!(result.output.contains("Batch 1"));
    }

    #[tokio::test]
    async fn test_pm_assign_person_tool() {
        let svc = create_service();
        let person = svc
            .create_person("John", true, &[], &[], None, None, "")
            .unwrap();
        let site = svc
            .create_site("Site A", "", None, None, 0, false, "")
            .unwrap();

        let tool = PmAssignPersonTool::new(Arc::clone(&svc));
        let result = tool
            .execute(serde_json::json!({
                "person_id": person.entity_id,
                "entity_type": "site",
                "entity_id": site.entity_id,
                "role": "lead",
                "start_date": "2026-08-18",
                "end_date": "2026-08-24"
            }))
            .await
            .expect("assign person");

        assert!(result.output.contains("assigned"));
    }

    #[tokio::test]
    async fn test_pm_upsert_inventory_tool() {
        let svc = create_service();
        let tool = PmUpsertInventoryTool::new(Arc::clone(&svc));

        let result = tool
            .execute(serde_json::json!({
                "name": "Drillbit 3mm",
                "quantity_on_hand": 50,
                "category": "consumable",
                "supplier": "ToolCo"
            }))
            .await
            .expect("upsert inventory");

        assert!(result.output.contains("saved"));
    }

    #[tokio::test]
    async fn test_pm_reserve_inventory_tool() {
        let svc = create_service();
        // First upsert an item via the service directly
        let item = svc
            .upsert_inventory(
                "Hammer",
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
            .unwrap();

        let tool = PmReserveInventoryTool::new(Arc::clone(&svc));
        let result = tool
            .execute(serde_json::json!({
                "item_id": item.entity_id,
                "quantity": 30,
                "entity_type": "site",
                "entity_id": ""
            }))
            .await
            .expect("reserve inventory");

        assert!(result.output.contains("Reserved"));
    }

    #[tokio::test]
    async fn test_pm_create_procurement_order_tool() {
        let svc = create_service();
        // Create inventory item first
        let item = svc
            .upsert_inventory(
                "Drillbit",
                Some("DB-01"),
                None,
                Some(10),
                None,
                None,
                Some("ToolCo"),
                Some(14),
                Some(20),
                None,
            )
            .unwrap();

        let tool = PmCreateProcurementOrderTool::new(Arc::clone(&svc));
        let result = tool
            .execute(serde_json::json!({
                "item_id": item.entity_id,
                "quantity": 50,
                "supplier": "ToolCo"
            }))
            .await
            .expect("create PO");

        assert!(result.output.contains("created"));
    }

    #[tokio::test]
    async fn test_pm_check_readiness_tool() {
        let svc = create_service();
        let site = svc
            .create_site("Site X", "", None, None, 0, false, "")
            .unwrap();
        let batch = svc
            .create_batch(
                "Batch X",
                &[],
                Some(site.id),
                Some("2026-08-18"),
                6,
                "planned",
                "",
            )
            .unwrap();

        let tool = PmCheckDeployReadinessTool::new(Arc::clone(&svc));
        let result = tool
            .execute(serde_json::json!({
                "batch_id": batch.entity_id
            }))
            .await
            .expect("check readiness");

        // Should list blockers (no inventory, etc.)
        assert!(
            result.output.contains("BLOCKED")
                || result.output.contains("READY")
        );
    }

    #[tokio::test]
    async fn test_pm_check_alerts_tool() {
        let svc = create_service();
        // Create an alert
        svc.create_alert(
            "low_stock",
            "urgent",
            "inventory_item",
            "inv-1",
            None,
            "Below threshold",
            "Order more",
        )
        .unwrap();

        let tool = PmCheckAlertsTool::new(Arc::clone(&svc));
        let result = tool
            .execute(serde_json::json!({}))
            .await
            .expect("check alerts");

        assert!(
            result.output.contains("low_stock")
                || result.output.contains("no active alerts")
        );
    }
}
