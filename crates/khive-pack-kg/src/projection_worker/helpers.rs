//! SQL helpers for the proposals projection worker.

use crate::sql::sql;
use khive_storage::{
    event::Event,
    types::{SqlStatement, SqlValue},
};

/// Build a conditional event INSERT `SqlStatement` for use in `execute_batch`.
pub(crate) fn build_conditional_event_insert(event: &Event) -> SqlStatement {
    let substrate_str = event.substrate.name().to_string();
    let kind_str = event.kind.name().to_string();
    let outcome_str = event.outcome.name().to_string();
    let payload_str = event.payload.to_string();

    let params = vec![
        SqlValue::Text(event.id.to_string()),
        SqlValue::Text(event.namespace.clone()),
        SqlValue::Text(event.verb.clone()),
        SqlValue::Text(substrate_str),
        SqlValue::Text(event.actor.clone()),
        SqlValue::Text(kind_str),
        SqlValue::Text(outcome_str),
        SqlValue::Text(payload_str),
        SqlValue::Integer(event.payload_schema_version as i64),
        match event.profile_state_version {
            Some(v) => SqlValue::Integer(v as i64),
            None => SqlValue::Null,
        },
        SqlValue::Integer(event.duration_us),
        match event.target_id {
            Some(u) => SqlValue::Text(u.to_string()),
            None => SqlValue::Null,
        },
        match event.session_id {
            Some(u) => SqlValue::Text(u.to_string()),
            None => SqlValue::Null,
        },
        match &event.aggregate_kind {
            Some(s) => SqlValue::Text(s.clone()),
            None => SqlValue::Null,
        },
        match event.aggregate_id {
            Some(u) => SqlValue::Text(u.to_string()),
            None => SqlValue::Null,
        },
        SqlValue::Integer(event.created_at),
    ];
    SqlStatement {
        sql: sql!("events_insert_if_changed").to_string(),
        params,
        label: Some("projection_worker.conditional_event_insert".into()),
    }
}
