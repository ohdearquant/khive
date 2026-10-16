//! SQL helpers for the proposals projection worker.

use crate::sql::sql;
use khive_runtime::{event_insert_statements, RuntimeError};
use khive_storage::{event::Event, types::SqlStatement};

/// Reuse canonical event/observation bindings, with the proposal CAS guard
/// on every insert. Each successful insert affects one row, so the guard
/// chains from the CAS through the event and observations; a missed CAS
/// suppresses the whole chain.
pub(crate) fn build_conditional_event_inserts(
    event: &Event,
) -> Result<Vec<SqlStatement>, RuntimeError> {
    let mut statements = event_insert_statements(event)
        .map_err(|error| RuntimeError::InvalidInput(format!("proposal event: {error}")))?;
    for (index, statement) in statements.iter_mut().enumerate() {
        statement.sql = if index == 0 {
            sql!("events_insert_if_changed")
        } else {
            sql!("event_observations_insert_if_changed")
        }
        .into();
        statement.label = Some(
            if index == 0 {
                "projection_worker.conditional_event_insert"
            } else {
                "projection_worker.conditional_event_observation_insert"
            }
            .into(),
        );
    }
    Ok(statements)
}
