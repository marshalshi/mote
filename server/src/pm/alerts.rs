//! Lead-time and blocker alert engine.
//!
//! Provides:
//! - `latest_safe_start` formula calculation
//! - `classify_alert_severity` for overdue/urgent/upcoming/ok
//! - Inventory and procurement alert generation (idempotent upsert)
//! - Blocked-batch detection from alerts and readiness signals
//!
//! # Idempotency rule
//!
//! `generate_inventory_alerts` and `generate_procurement_alerts` look for
//! existing active alerts by type + related_entity_id. If one exists, they
//! update its severity/timestamp instead of inserting a duplicate.

use anyhow::Result;
use chrono::NaiveDate;

use crate::pm::store::Store;

use marshaling_protocol::pm::{AlertGenerationSummary, BlockerInfo};

// ── Pure formula helpers ──────────────────────────────────────

/// Compute the latest safe start date given lead times and buffer.
///
/// `latest_safe = needed_by - vendor_lead_time - internal_processing - shipping - buffer`
#[allow(dead_code)]
pub fn latest_safe_start(
    needed_by: NaiveDate,
    vendor_lead_time_days: i64,
    internal_processing_days: i64,
    shipping_days: i64,
    buffer_days: i64,
) -> NaiveDate {
    let total = vendor_lead_time_days
        + internal_processing_days
        + shipping_days
        + buffer_days;
    needed_by - chrono::Duration::days(total)
}

/// Classify alert severity based on how close `today` is to `latest_safe`.
///
/// Returns one of `"overdue"`, `"urgent"`, `"upcoming"`, or `"ok"`.
///
/// - `overdue`: today is past the latest safe start date
/// - `urgent`: within `urgent_threshold_days` of the deadline (default 7)
/// - `upcoming`: within `upcoming_threshold_days` of the deadline (default 14)
/// - `ok`: otherwise
#[allow(dead_code)]
pub fn classify_alert_severity(
    today: NaiveDate,
    latest_safe: NaiveDate,
    urgent_threshold_days: i64,
    upcoming_threshold_days: i64,
) -> &'static str {
    let days_until = (latest_safe - today).num_days();
    if days_until < 0 {
        "overdue"
    } else if days_until <= urgent_threshold_days {
        "urgent"
    } else if days_until <= upcoming_threshold_days {
        "upcoming"
    } else {
        "ok"
    }
}

// ── Inventory alert generation ───────────────────────────────

/// Generate (or update) low-stock alerts for all inventory items.
///
/// For each item where `qty_on_hand <= reorder_threshold` and status is not
/// `"discontinued"`:
/// - severity `"urgent"` if qty is 0, otherwise `"upcoming"`
/// - upserts an alert of type `"low_stock"` keyed by SKU (or name if SKU empty)
///
/// Returns the number of alerts created or updated.
pub fn generate_inventory_alerts(store: &Store) -> Result<usize> {
    let items = store.list_inventory()?;
    let mut count = 0;

    for item in &items {
        if item.status == "discontinued"
            || item.qty_on_hand > item.reorder_threshold
        {
            continue;
        }

        let severity = if item.qty_on_hand == 0 {
            "urgent"
        } else {
            "upcoming"
        };
        let lookup_key = if item.sku.is_empty() {
            &item.name
        } else {
            &item.sku
        };

        // Idempotent upsert: find existing active alert for this item
        let existing = store
            .find_active_alert_by_type_and_entity("low_stock", lookup_key)?;

        if let Some(alert) = existing {
            store.update_alert(
                &alert.entity_id,
                None,
                Some(severity),
                None,
                None,
                None,
            )?;
        } else {
            store.insert_alert(
                "low_stock",
                severity,
                "inventory_item",
                lookup_key,
                None,
                &format!(
                    "Low stock: {} ({} on hand, threshold {})",
                    item.name, item.qty_on_hand, item.reorder_threshold
                ),
                &format!("Order more '{}' from {}", item.name, item.supplier),
            )?;
        }
        count += 1;
    }

    Ok(count)
}

// ── Procurement alert generation ─────────────────────────────

/// Generate (or update) alerts for open procurement orders.
///
/// For each open order with a parseable ETA:
/// - ETA has passed → `"overdue"` with type `"procurement_delay"`
/// - ETA within 7 days → `"urgent"` with type `"procurement_delay"`
/// - ETA within 14 days → `"upcoming"` with type `"procurement_upcoming"`
/// - ETA beyond 14 days → skipped (no alert needed yet)
///
/// Returns the number of alerts created or updated.
pub fn generate_procurement_alerts(store: &Store) -> Result<usize> {
    let orders = store.list_open_orders()?;
    let today = chrono::Utc::now().date_naive();
    let mut count = 0;

    for order in &orders {
        let eta = match &order.eta {
            Some(e) => match NaiveDate::parse_from_str(e, "%Y-%m-%d") {
                Ok(d) => d,
                Err(_) => continue,
            },
            None => continue,
        };

        let days_until = (eta - today).num_days();
        let (alert_type, severity): (&str, &str) = if days_until < 0 {
            ("procurement_delay", "overdue")
        } else if days_until <= 7 {
            ("procurement_delay", "urgent")
        } else if days_until <= 14 {
            ("procurement_upcoming", "upcoming")
        } else {
            continue;
        };

        let lookup_key = if order.order_ref.is_empty() {
            &order.entity_id
        } else {
            &order.order_ref
        };

        // Idempotent upsert
        let existing = store
            .find_active_alert_by_type_and_entity(alert_type, lookup_key)?;

        let reason = format!(
            "{}: '{}' from {} - ETA {}",
            if days_until < 0 {
                "Overdue"
            } else {
                "Upcoming"
            },
            order.item_name,
            order.supplier,
            order.eta.as_deref().unwrap_or("unknown"),
        );

        if let Some(alert) = existing {
            store.update_alert(
                &alert.entity_id,
                None,
                Some(severity),
                Some(&reason),
                None,
                None,
            )?;
        } else {
            store.insert_alert(
                alert_type,
                severity,
                "procurement_order",
                lookup_key,
                Some(&eta.to_string()),
                &reason,
                &format!(
                    "Follow up on order for '{}' from {}",
                    order.item_name, order.supplier
                ),
            )?;
        }
        count += 1;
    }

    Ok(count)
}

// ── Stale alert resolution ───────────────────────────────────

/// Resolve alerts whose conditions no longer apply.
///
/// Called as a pre-step before `generate_all_alerts` so that alerts that
/// have cleared are marked resolved instead of lingering indefinitely.
///
/// Resolution rules:
/// - Inventory `low_stock` alerts: resolved when `qty_on_hand > reorder_threshold`
/// - Procurement alerts: resolved when order status is `"delivered"` or `"cancelled"`
///
/// Returns the number of alerts resolved.
pub fn resolve_stale_alerts(store: &Store) -> Result<usize> {
    let active = store.list_active_alerts()?;
    let mut resolved_count = 0;

    for alert in &active {
        match alert.alert_type.as_str() {
            "low_stock" => {
                // Try SKU first, then name
                let item = store
                    .get_inventory_by_sku(&alert.related_entity_id)
                    .ok()
                    .flatten()
                    .or_else(|| {
                        store
                            .get_inventory_by_name(&alert.related_entity_id)
                            .ok()
                            .flatten()
                    });
                if let Some(item) = item {
                    if item.qty_on_hand > item.reorder_threshold {
                        if store.resolve_alert(&alert.entity_id).is_ok() {
                            resolved_count += 1;
                        }
                    }
                }
            }
            "procurement_delay" | "procurement_upcoming" => {
                // Try entity_id first, then order_ref as fallback
                let order = store
                    .get_procurement_order(&alert.related_entity_id)
                    .ok()
                    .flatten()
                    .or_else(|| {
                        // Fallback: alerts keyed by order_ref
                        store
                            .get_procurement_order_by_ref(
                                &alert.related_entity_id,
                            )
                            .ok()
                            .flatten()
                    });
                if let Some(order) = order {
                    if order.status == "delivered"
                        || order.status == "cancelled"
                    {
                        if store.resolve_alert(&alert.entity_id).is_ok() {
                            resolved_count += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(resolved_count)
}

// ── Combined generation ─────────────────────────────────────

/// Run all alert generators and return a summary.
///
/// Stale alerts (those whose conditions have cleared) are resolved first
/// so that the report reflects current state.
pub fn generate_all_alerts(store: &Store) -> Result<AlertGenerationSummary> {
    // Pre-step: resolve stale alerts
    let _resolved = resolve_stale_alerts(store)?;

    let inventory_alerts = generate_inventory_alerts(store)?;
    let procurement_alerts = generate_procurement_alerts(store)?;
    Ok(AlertGenerationSummary {
        inventory_alerts,
        procurement_alerts,
        total: inventory_alerts + procurement_alerts,
    })
}

// ── Blocked-batch detection ─────────────────────────────────

/// Detect blocked batches from current alert and readiness state.
///
/// Scans all non-terminal batches and checks:
/// 1. Active `overdue` or `urgent` alert targeting this batch
/// 2. Linked inventory with zero reservation and no in-transit procurement
/// 3. Missing target site
/// 4. Missing deploy date
///
/// Does **not** run alert generation — callers should run
/// `generate_all_alerts` first if alerts need to be up to date.
pub fn detect_blocked_batches(store: &Store) -> Result<Vec<BlockerInfo>> {
    let batches = store.list_batches()?;
    let alerts = store.list_active_alerts()?;
    let inventory = store.list_inventory()?;
    let open_orders = store.list_open_orders()?;

    let terminal_statuses = ["completed", "cancelled", "deployed"];
    let mut blockers: Vec<BlockerInfo> = Vec::new();

    for batch in &batches {
        if terminal_statuses.contains(&batch.status.as_str()) {
            continue;
        }

        // Check 1: Active overdue or urgent alerts
        for alert in &alerts {
            if (alert.severity == "overdue" || alert.severity == "urgent")
                && (alert.related_entity_id == batch.entity_id
                    || alert.related_entity_id == batch.name)
            {
                blockers.push(BlockerInfo {
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

        // Check 2: Linked inventory not reserved with no in-transit procurement
        for item in &inventory {
            if item.linked_batch_id == Some(batch.id) && item.qty_reserved == 0
            {
                let has_in_transit = open_orders.iter().any(|po| {
                    po.linked_batch_id == Some(batch.id)
                        && (po.item_sku == item.sku
                            || po.item_name == item.name)
                        && po.status != "delivered"
                        && po.status != "cancelled"
                });

                if !has_in_transit {
                    blockers.push(BlockerInfo {
                        batch_entity_id: batch.entity_id.clone(),
                        batch_name: batch.name.clone(),
                        reason: format!(
                            "Item '{}' linked to batch but not reserved (no in-transit order)",
                            item.name
                        ),
                        severity: "urgent".to_string(),
                    });
                }
            }
        }

        // Check 3: Missing target site
        if batch.target_site_id.is_none() {
            blockers.push(BlockerInfo {
                batch_entity_id: batch.entity_id.clone(),
                batch_name: batch.name.clone(),
                reason: "No target site assigned".to_string(),
                severity: "urgent".to_string(),
            });
        }

        // Check 4: Missing deploy date
        if batch.target_deploy_date.is_none() {
            blockers.push(BlockerInfo {
                batch_entity_id: batch.entity_id.clone(),
                batch_name: batch.name.clone(),
                reason: "No target deploy date set".to_string(),
                severity: "upcoming".to_string(),
            });
        }
    }

    Ok(blockers)
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm::schema;
    use crate::pm::store::Store;
    use chrono::NaiveDate;
    use rusqlite::Connection;

    fn create_store() -> Store {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        schema::bootstrap(&conn).expect("bootstrap");
        Store::new(conn)
    }

    // ── latest_safe_start tests ──

    #[test]
    fn test_latest_safe_start_basic() {
        let needed_by =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        let result = latest_safe_start(needed_by, 14, 3, 5, 2);
        // 14 + 3 + 5 + 2 = 24 days before Aug 18 = Jul 25
        let expected =
            NaiveDate::parse_from_str("2026-07-25", "%Y-%m-%d").unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_latest_safe_start_zero_buffer() {
        let needed_by =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        let result = latest_safe_start(needed_by, 10, 0, 0, 0);
        let expected =
            NaiveDate::parse_from_str("2026-08-08", "%Y-%m-%d").unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_latest_safe_start_same_day() {
        let d = NaiveDate::parse_from_str("2026-07-01", "%Y-%m-%d").unwrap();
        let result = latest_safe_start(d, 0, 0, 0, 0);
        assert_eq!(result, d);
    }

    // ── classify_alert_severity tests ──

    #[test]
    fn test_severity_overdue() {
        let today =
            NaiveDate::parse_from_str("2026-08-19", "%Y-%m-%d").unwrap();
        let latest_safe =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        assert_eq!(
            classify_alert_severity(today, latest_safe, 7, 14),
            "overdue"
        );
    }

    #[test]
    fn test_severity_urgent() {
        let today =
            NaiveDate::parse_from_str("2026-08-12", "%Y-%m-%d").unwrap();
        let latest_safe =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        // 6 days until latest_safe, within 7-day urgent threshold
        assert_eq!(
            classify_alert_severity(today, latest_safe, 7, 14),
            "urgent"
        );
    }

    #[test]
    fn test_severity_upcoming() {
        let today =
            NaiveDate::parse_from_str("2026-08-08", "%Y-%m-%d").unwrap();
        let latest_safe =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        // 10 days until latest_safe: within 14-day but outside 7-day threshold
        assert_eq!(
            classify_alert_severity(today, latest_safe, 7, 14),
            "upcoming"
        );
    }

    #[test]
    fn test_severity_ok() {
        let today =
            NaiveDate::parse_from_str("2026-08-01", "%Y-%m-%d").unwrap();
        let latest_safe =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        // 17 days away, outside 14-day threshold
        assert_eq!(classify_alert_severity(today, latest_safe, 7, 14), "ok");
    }

    #[test]
    fn test_severity_exactly_at_deadline() {
        let today =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        let latest_safe =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        // 0 days until, should be "urgent" (within 7-day threshold)
        assert_eq!(
            classify_alert_severity(today, latest_safe, 7, 14),
            "urgent"
        );
    }

    #[test]
    fn test_severity_exactly_at_urgent_boundary() {
        let today =
            NaiveDate::parse_from_str("2026-08-11", "%Y-%m-%d").unwrap();
        let latest_safe =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        // 7 days until — exactly at boundary, should be urgent
        assert_eq!(
            classify_alert_severity(today, latest_safe, 7, 14),
            "urgent"
        );
    }

    #[test]
    fn test_severity_exactly_at_upcoming_boundary() {
        let today =
            NaiveDate::parse_from_str("2026-08-04", "%Y-%m-%d").unwrap();
        let latest_safe =
            NaiveDate::parse_from_str("2026-08-18", "%Y-%m-%d").unwrap();
        // 14 days until — exactly at boundary, should be upcoming
        assert_eq!(
            classify_alert_severity(today, latest_safe, 7, 14),
            "upcoming"
        );
    }

    // ── generate_inventory_alerts tests ──

    #[test]
    fn test_generate_inventory_alerts_creates_low_stock() {
        let store = create_store();
        store
            .upsert_inventory(
                "Hammer",
                Some("HAM-01"),
                Some("tool"),
                Some(3),
                None,
                None,
                Some("Acme"),
                Some(5),
                Some(10),
                None,
            )
            .expect("create item");
        // qty_on_hand=3 <= reorder_threshold=10 → should trigger

        let count = generate_inventory_alerts(&store).expect("gen alerts");
        assert_eq!(count, 1, "should create one alert");

        let alerts = store.list_active_alerts().expect("list alerts");
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].alert_type, "low_stock");
        assert_eq!(alerts[0].severity, "upcoming"); // 3 > 0, so upcoming
        assert_eq!(alerts[0].related_entity_id, "HAM-01");
    }

    #[test]
    fn test_generate_inventory_alerts_out_of_stock_is_urgent() {
        let store = create_store();
        store
            .upsert_inventory(
                "Drillbit",
                Some("DB-3MM"),
                Some("consumable"),
                Some(0),
                None,
                None,
                Some("ToolCo"),
                Some(14),
                Some(5),
                None,
            )
            .expect("create item");

        let count = generate_inventory_alerts(&store).expect("gen alerts");
        assert_eq!(count, 1);

        let alerts = store.list_active_alerts().expect("list alerts");
        assert_eq!(alerts[0].severity, "urgent");
    }

    #[test]
    fn test_generate_inventory_alerts_skips_ok_items() {
        let store = create_store();
        store
            .upsert_inventory(
                "WellStocked",
                Some("WS-01"),
                None,
                Some(100),
                None,
                None,
                None,
                None,
                Some(10),
                None,
            )
            .expect("create item");
        // qty_on_hand=100 > reorder_threshold=10 → no alert

        let count = generate_inventory_alerts(&store).expect("gen alerts");
        assert_eq!(count, 0, "no alert for well-stocked item");
    }

    #[test]
    fn test_generate_inventory_alerts_idempotent_update() {
        let store = create_store();
        store
            .upsert_inventory(
                "Hammer",
                Some("HAM-01"),
                None,
                Some(3),
                None,
                None,
                None,
                None,
                Some(10),
                None,
            )
            .expect("create");

        let count1 = generate_inventory_alerts(&store).expect("first run");
        assert_eq!(count1, 1);

        // Second run: should update, not duplicate
        let count2 = generate_inventory_alerts(&store).expect("second run");
        assert_eq!(count2, 1, "should still find and touch one alert");

        let alerts = store.list_active_alerts().expect("list alerts");
        assert_eq!(alerts.len(), 1, "no duplicate alerts");
    }

    // ── generate_procurement_alerts tests ──

    fn set_procurement_eta(store: &Store, entity_id: &str, eta: &str) {
        store
            .update_procurement_order(
                entity_id,
                None,
                Some(eta),
                None,
                None,
                None,
                None,
            )
            .expect("set ETA");
    }

    #[test]
    fn test_generate_procurement_alerts_overdue() {
        let store = create_store();
        let po = store
            .insert_procurement_order("SKU", "Item", "Sup", 10, 7, None, "")
            .expect("create order");
        let eta = (chrono::Utc::now().date_naive() - chrono::Duration::days(5))
            .format("%Y-%m-%d")
            .to_string();
        set_procurement_eta(&store, &po.entity_id, &eta);

        let count = generate_procurement_alerts(&store).expect("gen alerts");
        assert_eq!(count, 1);

        let alerts = store.list_active_alerts().expect("list alerts");
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].alert_type, "procurement_delay");
        assert_eq!(alerts[0].severity, "overdue");
    }

    #[test]
    fn test_generate_procurement_alerts_urgent() {
        let store = create_store();
        let po = store
            .insert_procurement_order("SKU", "Item", "Sup", 10, 7, None, "")
            .expect("create order");
        let eta = (chrono::Utc::now().date_naive() + chrono::Duration::days(3))
            .format("%Y-%m-%d")
            .to_string();
        set_procurement_eta(&store, &po.entity_id, &eta);

        let count = generate_procurement_alerts(&store).expect("gen alerts");
        assert_eq!(count, 1);
        let alerts = store.list_active_alerts().expect("list alerts");
        assert_eq!(alerts[0].severity, "urgent");
    }

    #[test]
    fn test_generate_procurement_alerts_upcoming() {
        let store = create_store();
        let po = store
            .insert_procurement_order("SKU", "Item", "Sup", 10, 7, None, "")
            .expect("create order");
        let eta = (chrono::Utc::now().date_naive()
            + chrono::Duration::days(10))
        .format("%Y-%m-%d")
        .to_string();
        set_procurement_eta(&store, &po.entity_id, &eta);

        let count = generate_procurement_alerts(&store).expect("gen alerts");
        assert_eq!(count, 1);
        let alerts = store.list_active_alerts().expect("list alerts");
        assert_eq!(alerts[0].alert_type, "procurement_upcoming");
        assert_eq!(alerts[0].severity, "upcoming");
    }

    #[test]
    fn test_generate_procurement_alerts_skips_far_future() {
        let store = create_store();
        let po = store
            .insert_procurement_order("SKU", "Item", "Sup", 10, 7, None, "")
            .expect("create order");
        let eta = (chrono::Utc::now().date_naive()
            + chrono::Duration::days(30))
        .format("%Y-%m-%d")
        .to_string();
        set_procurement_eta(&store, &po.entity_id, &eta);

        let count = generate_procurement_alerts(&store).expect("gen alerts");
        assert_eq!(count, 0, "no alert for far-future ETA");
    }

    #[test]
    fn test_generate_procurement_alerts_skips_no_eta() {
        let store = create_store();
        store
            .insert_procurement_order("SKU", "Item", "Sup", 10, 7, None, "")
            .expect("create order");
        // No ETA set

        let count = generate_procurement_alerts(&store).expect("gen alerts");
        assert_eq!(count, 0);
    }

    // ── generate_all_alerts tests ──

    #[test]
    fn test_generate_all_alerts_combined() {
        let store = create_store();
        // Inventory item below threshold
        store
            .upsert_inventory(
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
            .expect("create");

        // Procurement order with overdue ETA
        let po = store
            .insert_procurement_order("BOLT", "Bolt", "Sup", 50, 7, None, "")
            .expect("create PO");
        let eta = (chrono::Utc::now().date_naive() - chrono::Duration::days(2))
            .format("%Y-%m-%d")
            .to_string();
        set_procurement_eta(&store, &po.entity_id, &eta);

        let summary = generate_all_alerts(&store).expect("generate all");
        assert_eq!(summary.inventory_alerts, 1);
        assert_eq!(summary.procurement_alerts, 1);
        assert_eq!(summary.total, 2);
    }

    // ── detect_blocked_batches tests ──

    #[test]
    fn test_detect_blocked_batches_no_blockers() {
        let store = create_store();
        // Create a clean batch with site and deploy date
        let site = store
            .insert_site("Site A", "", None, None, 0, false, "")
            .expect("create site");
        store
            .insert_batch(
                "Clean Batch",
                &[],
                Some(site.id),
                Some("2026-08-18"),
                6,
                "ready",
                "",
            )
            .expect("create batch");

        let blockers = detect_blocked_batches(&store).expect("detect");
        // No blockers for ready batch (terminal)
        let relevant: Vec<_> = blockers
            .iter()
            .filter(|b| b.batch_name == "Clean Batch")
            .collect();
        assert_eq!(relevant.len(), 0);
    }

    #[test]
    fn test_detect_blocked_batches_missing_site_and_date() {
        let store = create_store();
        store
            .insert_batch("Orphan", &[], None, None, 0, "planned", "")
            .expect("create batch");

        let blockers = detect_blocked_batches(&store).expect("detect");
        let batch_blockers: Vec<_> = blockers
            .iter()
            .filter(|b| b.batch_name == "Orphan")
            .collect();
        assert!(
            batch_blockers.iter().any(|b| b.reason.contains("site")),
            "should block for missing site"
        );
        assert!(
            batch_blockers
                .iter()
                .any(|b| b.reason.contains("deploy date")),
            "should block for missing deploy date"
        );
    }

    #[test]
    fn test_detect_blocked_batches_alert_blocks() {
        let store = create_store();
        let batch = store
            .insert_batch("Alerted Batch", &[], None, None, 0, "planned", "")
            .expect("create batch");

        // Create an active urgent alert for this batch
        store
            .insert_alert(
                "procurement_delay",
                "urgent",
                "batch",
                &batch.entity_id,
                None,
                "Overdue procurement",
                "Follow up",
            )
            .expect("create alert");

        let blockers = detect_blocked_batches(&store).expect("detect");
        let batch_blockers: Vec<_> = blockers
            .iter()
            .filter(|b| b.batch_name == "Alerted Batch")
            .collect();
        assert!(
            batch_blockers
                .iter()
                .any(|b| b.reason.contains("procurement")),
            "should block for urgent alert"
        );
    }

    #[test]
    fn test_detect_blocked_batches_unreserved_inventory() {
        let store = create_store();
        let site = store
            .insert_site("Site X", "", None, None, 0, false, "")
            .expect("create site");
        let batch = store
            .insert_batch(
                "Inventory Batch",
                &[],
                Some(site.id),
                Some("2026-08-18"),
                6,
                "planned",
                "",
            )
            .expect("create batch");

        // Create inventory linked to this batch with 0 reserved
        store
            .upsert_inventory(
                "Part",
                Some("PART-01"),
                None,
                Some(100),
                None,
                None,
                None,
                None,
                None,
                Some(Some(batch.id)),
            )
            .expect("create item");

        let blockers = detect_blocked_batches(&store).expect("detect");
        let batch_blockers: Vec<_> = blockers
            .iter()
            .filter(|b| b.batch_name == "Inventory Batch")
            .collect();
        assert!(
            batch_blockers
                .iter()
                .any(|b| b.reason.contains("not reserved")),
            "should block for unreserved inventory"
        );
    }

    #[test]
    fn test_detect_blocked_batches_skips_terminal() {
        let store = create_store();
        store
            .insert_batch("Done Batch", &[], None, None, 0, "completed", "")
            .expect("create batch");
        store
            .insert_batch("Deployed Batch", &[], None, None, 0, "deployed", "")
            .expect("create batch");

        let blockers = detect_blocked_batches(&store).expect("detect");
        assert!(
            !blockers.iter().any(|b| b.batch_name == "Done Batch"),
            "completed should be skipped"
        );
        assert!(
            !blockers.iter().any(|b| b.batch_name == "Deployed Batch"),
            "deployed should be skipped"
        );
    }
}
