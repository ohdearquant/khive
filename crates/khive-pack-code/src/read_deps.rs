use khive_runtime::RuntimeError;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::read_common::{bounded, db_error, invalid, required_text};
use crate::CodePack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DepsParams {
    map: String,
    id: String,
    #[serde(default = "default_direction")]
    direction: String,
    #[serde(default = "default_depth")]
    max_depth: u64,
    #[serde(default = "default_limit")]
    limit: u64,
}

fn default_direction() -> String {
    "both".into()
}

fn default_depth() -> u64 {
    2
}

fn default_limit() -> u64 {
    200
}

#[derive(Serialize)]
struct Node {
    id: String,
    kind: String,
    name: String,
}

fn node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
    })
}

fn ids_json(ids: &[String]) -> Result<String, RuntimeError> {
    serde_json::to_string(ids).map_err(|_| {
        RuntimeError::Internal("code dependency identifiers could not be encoded".into())
    })
}

impl CodePack {
    pub(crate) async fn handle_deps(&self, params: Value) -> Result<Value, RuntimeError> {
        let params: DepsParams = serde_json::from_value(params).map_err(|_| {
            invalid("invalid code.deps arguments: map and id must be non-empty strings; direction must be outgoing, incoming or both; max_depth must be an integer in 1..=10; limit must be an integer in 1..=1000; unknown fields are not accepted")
        })?;
        required_text(&params.map, "map")?;
        required_text(&params.id, "id")?;
        let depth = bounded(Some(params.max_depth), "max_depth", 2, 10)?;
        let limit = bounded(Some(params.limit), "limit", 200, 1000)?;
        if !matches!(params.direction.as_str(), "outgoing" | "incoming" | "both") {
            return Err(invalid("direction must be outgoing, incoming or both"));
        }
        self.runtime.code_map_registry().resolve(&params.map)?;
        let runtime = self.runtime.clone();
        tokio::task::spawn_blocking(move || {
            runtime
                .code_map_registry()
                .with_read(&params.map, |connection| {
                    traverse(connection, &params, depth, limit)
                })
        })
        .await
        .map_err(|_| RuntimeError::Internal("code dependency read could not finish".into()))?
    }
}

fn traverse(
    connection: &Connection,
    params: &DepsParams,
    depth: usize,
    limit: usize,
) -> Result<Value, RuntimeError> {
    let root = connection
        .query_row(
            include_str!("../sql/read-deps-root.sql"),
            [&params.id],
            node,
        )
        .optional()
        .map_err(db_error)?
        .ok_or_else(|| invalid("id was not found in the selected code map"))?;
    let mut visited = vec![root.id.clone()];
    let mut frontier = visited.clone();
    let mut nodes = vec![root];
    let mut truncated = false;
    let mut neighbors = connection
        .prepare(include_str!("../sql/read-deps-neighbors.sql"))
        .map_err(db_error)?;
    for _ in 0..depth {
        let remaining = limit - nodes.len();
        let candidates = neighbors
            .query_map(
                params![
                    ids_json(&frontier)?,
                    ids_json(&visited)?,
                    &params.direction,
                    (remaining + 1) as i64,
                ],
                node,
            )
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        if candidates.is_empty() {
            break;
        }
        truncated = candidates.len() > remaining;
        frontier.clear();
        for candidate in candidates.into_iter().take(remaining) {
            frontier.push(candidate.id.clone());
            visited.push(candidate.id.clone());
            nodes.push(candidate);
        }
        if truncated {
            break;
        }
    }
    // Traversal may stop halfway through a frontier; retain every edge between admitted nodes.
    let mut edge_query = connection
        .prepare(include_str!("../sql/read-deps-edges.sql"))
        .map_err(db_error)?;
    let edges = edge_query
        .query_map([ids_json(&visited)?], |row| {
            Ok(json!({"source": row.get::<_, String>(0)?, "target": row.get::<_, String>(1)?}))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    Ok(json!({
        "map": params.map,
        "root": nodes[0].id,
        "nodes": nodes,
        "edges": edges,
        "truncated": truncated,
    }))
}

#[cfg(test)]
#[path = "read_deps_tests.rs"]
mod tests;
