//! Audit log helpers for PM operations.
//!
//! Provides convenience functions for logging events to the `event_logs`
//! table. Every important write operation should call one of these helpers
//! to maintain a durable audit trail.

use anyhow::Result;
use chrono::Utc;

use crate::pm::Store;

/// Log a write event with the given parameters.
///
/// This is the primary helper for event logging. It records the event with
/// the current UTC timestamp and returns the internal event ID.
///
/// # Arguments
///
/// * `store` - The PM store
/// * `source` - Event source: "user", "sync", or "system"
/// * `action_type` - Action performed (e.g., "created", "updated", "reserved", "resolved")
/// * `entity_type` - Type of entity (e.g., "site", "batch", "inventory_item")
/// * `entity_id` - Stable entity ID (UUID) of the affected entity
/// * `summary` - Human-readable summary of what changed
///
/// # Returns
///
/// The internal database ID of the event log row.
pub fn log_write_event(
    store: &Store,
    source: &str,
    action_type: &str,
    entity_type: &str,
    entity_id: &str,
    summary: &str,
) -> Result<i64> {
    let timestamp = Utc::now().to_rfc3339();
    store.log_event(
        &timestamp,
        source,
        action_type,
        entity_type,
        entity_id,
        summary,
    )
}

/// Log an event with a formatted summary.
///
/// Convenience wrapper that formats a summary from a format string.
pub fn log_write_event_fmt(
    store: &Store,
    source: &str,
    action_type: &str,
    entity_type: &str,
    entity_id: &str,
    summary_fmt: std::fmt::Arguments<'_>,
) -> Result<i64> {
    let summary = std::fmt::format(summary_fmt);
    log_write_event(
        store,
        source,
        action_type,
        entity_type,
        entity_id,
        &summary,
    )
}
