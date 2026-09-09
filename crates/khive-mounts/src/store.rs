use std::{any::Any, sync::Arc};

use khive_runtime::{mounted_verb::MountedVerb, RuntimeError};
use khive_storage::{AtomicUnitOp, Event, SqlAccess, SqlStatement, SqlValue};

use crate::error::Failure;

fn statement(sql: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.into(),
        params,
        label: Some("tool_source_mount".into()),
    }
}

pub(crate) async fn load(
    sql: &Arc<dyn SqlAccess>,
    name: &str,
) -> Result<Option<(i64, Vec<MountedVerb>)>, RuntimeError> {
    let row = sql
        .reader()
        .await?
        .query_row(statement(
            "SELECT generation, tools FROM tool_source_mounts WHERE name = ?1",
            vec![SqlValue::Text(name.into())],
        ))
        .await?;
    let Some(row) = row else { return Ok(None) };
    let (Some(SqlValue::Integer(generation)), Some(SqlValue::Text(tools))) =
        (row.get("generation"), row.get("tools"))
    else {
        return Err(Failure::error("catalog_unavailable").wire(name));
    };
    let mut tools: Vec<MountedVerb> = serde_json::from_str(tools)
        .map_err(|_| Failure::error("catalog_unavailable").wire(name))?;
    for tool in &mut tools {
        tool.generation = *generation;
    }
    Ok(Some((*generation, tools)))
}

pub(crate) async fn initialize(
    sql: &Arc<dyn SqlAccess>,
    name: &str,
    tools: &[MountedVerb],
) -> Result<(), RuntimeError> {
    let tools = serde_json::to_string(tools)
        .map_err(|_| Failure::error("catalog_unavailable").wire(name))?;
    sql.writer().await?.execute(statement(
        "INSERT INTO tool_source_mounts(name, generation, tools) VALUES (?1, 1, ?2) ON CONFLICT(name) DO NOTHING",
        vec![SqlValue::Text(name.into()), SqlValue::Text(tools)],
    )).await?;
    Ok(())
}

pub(crate) async fn replace(
    sql: &Arc<dyn SqlAccess>,
    name: &str,
    generation: i64,
    tools: &[MountedVerb],
    audit: &Event,
) -> Result<(), RuntimeError> {
    let tools = serde_json::to_string(tools)
        .map_err(|_| Failure::error("catalog_unavailable").wire(name))?;
    let update = statement(
        "UPDATE tool_source_mounts SET tools = ?1, generation = generation + 1 WHERE name = ?2 AND generation = ?3 AND generation < 9223372036854775807",
        vec![SqlValue::Text(tools), SqlValue::Text(name.into()), SqlValue::Integer(generation)],
    );
    let audit = khive_db::stores::event::event_insert_statements(audit)
        .map_err(|_| Failure::error("audit_unavailable").wire(name))?;
    // Serialization, catalog discovery and audit preparation precede the writer.
    // The closure performs only bounded SQL; a lost CAS writes no audit row.
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let changed = writer.execute(update).await? == 1;
            if changed {
                for statement in audit {
                    writer.execute(statement).await?;
                }
            }
            Ok(Box::new(changed) as Box<dyn Any + Send>)
        })
    });
    let changed = sql
        .atomic_unit(op)
        .await?
        .downcast::<bool>()
        .map_err(|_| Failure::error("catalog_unavailable").wire(name))?;
    if !*changed {
        return Err(Failure::error("generation_changed").wire(name));
    }
    Ok(())
}
