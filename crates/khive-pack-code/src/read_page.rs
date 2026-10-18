//! Bounded list/search over a single registered code map.

use khive_runtime::RuntimeError;
use rusqlite::{functions::FunctionFlags, params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::read_common::{bounded, db_error, invalid, required_text};
use crate::read_cursor::{Binding, Cursor, Position};
use crate::CodePack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    map: String,
    query: Option<String>,
    kind: Option<String>,
    project_id: Option<String>,
    cursor: Option<String>,
    limit: Option<u64>,
}

impl CodePack {
    pub(crate) async fn handle_list(&self, params: Value) -> Result<Value, RuntimeError> {
        self.read_page(params, false).await
    }

    pub(crate) async fn handle_search(&self, params: Value) -> Result<Value, RuntimeError> {
        self.read_page(params, true).await
    }

    async fn read_page(&self, params: Value, search: bool) -> Result<Value, RuntimeError> {
        let maximum = if search { 200 } else { 500 };
        let default = if search { 20 } else { 100 };
        validate_shapes(&params, search, maximum)?;
        let params: Params = serde_json::from_value(params)
            .map_err(|_| invalid("unsupported code map read parameter"))?;
        required_text(&params.map, "map")?;
        if let Some(kind) = &params.kind {
            if !["project", "module", "function", "datatype", "interface"].contains(&kind.as_str())
            {
                return Err(invalid(
                    "kind must be project, module, function, datatype, or interface",
                ));
            }
        }
        if let Some(project) = &params.project_id {
            required_text(project, "project_id")?;
        }
        if search {
            required_text(params.query.as_deref().unwrap_or_default(), "query")?;
        }
        let limit = bounded(params.limit, "limit", default, maximum)?;
        // Decode shape and check all bindings before with_read opens the map.
        let cursor = params
            .cursor
            .as_deref()
            .map(|s| Cursor::decode(s, search))
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.binding.map != params.map)
        {
            return Err(invalid(
                "cursor is stale for this map registration or request",
            ));
        }
        let registered = self.runtime.code_map_registry().resolve(&params.map)?;
        let binding = Binding {
            map: registered.name().to_owned(),
            generation: registered.generation().to_string(),
            query: params.query.clone(),
            kind: params.kind.clone(),
            project_id: params.project_id.clone(),
        };
        if let Some(cursor) = &cursor {
            cursor.validate(&binding)?;
        }
        let runtime = self.runtime.clone();
        tokio::task::spawn_blocking(move || {
            runtime.code_map_registry().with_read(&params.map, |connection| {
                if search {
                    register_name_score(connection)?;
                }
                if let Some(cursor) = &cursor {
                    if query_page(connection, &params, Some(&cursor.position), true, 1)?.is_empty() {
                        return Err(invalid("cursor is stale: its position is absent from this map result"));
                    }
                }
                let mut items = query_page(connection, &params, cursor.as_ref().map(|c| &c.position), false, limit + 1)?;
                let more = items.len() > limit;
                items.truncate(limit);
                let next_cursor = if more {
                    let last = items.last().expect("positive page limit");
                    let position = if search {
                        Position::Search { score: last.score, id: last.id.clone() }
                    } else {
                        Position::List { name: last.name.clone(), id: last.id.clone() }
                    };
                    Some(Cursor { binding, position }.encode()?)
                } else { None };
                let items: Vec<_> = items.into_iter().map(|item| {
                    let mut value = json!({"id": item.id, "kind": item.kind, "name": item.name, "project_id": item.project_id});
                    if search { value["score"] = json!(f64::from(item.score)); }
                    value
                }).collect();
                Ok(json!({"map": params.map, "items": items, "next_cursor": next_cursor}))
            })
        }).await.map_err(|_| RuntimeError::Internal("code map read task failed".into()))?
    }
}

fn validate_shapes(params: &Value, search: bool, maximum: u64) -> Result<(), RuntimeError> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid("parameters must be an object"))?;
    for (name, value) in object {
        match name.as_str() {
            "map" | "kind" | "project_id" | "cursor" | "query" if name != "query" || search => {
                if !value.is_string() {
                    return Err(invalid(format!("{name} must be a string")));
                }
            }
            "limit" => {
                if value.as_u64().is_none() {
                    return Err(invalid(format!(
                        "limit must be an integer in 1..={maximum}"
                    )));
                }
            }
            _ => return Err(invalid("unsupported code map read parameter")),
        }
    }
    if !object.contains_key("map") {
        return Err(invalid("map must be a non-empty string"));
    }
    if search && !object.contains_key("query") {
        return Err(invalid("query must be a non-empty string"));
    }
    Ok(())
}

struct Item {
    id: String,
    kind: String,
    name: String,
    project_id: Option<String>,
    score: u8,
}

fn query_page(
    connection: &Connection,
    params: &Params,
    position: Option<&Position>,
    check: bool,
    limit: usize,
) -> Result<Vec<Item>, RuntimeError> {
    let (name, score, id) = match position {
        Some(Position::List { name, id }) => (Some(name.as_str()), None, Some(id.as_str())),
        Some(Position::Search { score, id }) => (None, Some(*score), Some(id.as_str())),
        None => (None, None, None),
    };
    let search = params.query.is_some();
    let sql = if search {
        include_str!("../sql/read-search.sql")
    } else {
        include_str!("../sql/read-list.sql")
    };
    let mut statement = connection.prepare(sql).map_err(db_error)?;
    let read = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Item> {
        Ok(Item {
            id: row.get(0)?,
            kind: row.get(1)?,
            name: row.get(2)?,
            project_id: row.get(3)?,
            score: if search { row.get(4)? } else { 0 },
        })
    };
    let rows = if search {
        statement.query_map(
            params![
                params.kind,
                params.project_id,
                params.query,
                if check { None } else { score },
                if check { None } else { id },
                if check { score } else { None },
                if check { id } else { None },
                limit as i64,
            ],
            read,
        )
    } else {
        statement.query_map(
            params![
                params.kind,
                params.project_id,
                if check { None } else { name },
                if check { None } else { id },
                if check { name } else { None },
                if check { id } else { None },
                limit as i64,
            ],
            read,
        )
    }
    .map_err(db_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)
}

fn register_name_score(connection: &Connection) -> Result<(), RuntimeError> {
    connection
        .create_scalar_function(
            "code_name_score",
            2,
            FunctionFlags::SQLITE_UTF8
                | FunctionFlags::SQLITE_DETERMINISTIC
                | FunctionFlags::SQLITE_INNOCUOUS,
            |context| {
                let name = context.get::<String>(0)?.to_lowercase();
                let query = context.get::<String>(1)?.to_lowercase();
                Ok(if name == query {
                    3
                } else if name.starts_with(&query) {
                    2
                } else if name.contains(&query) {
                    1
                } else {
                    0
                })
            },
        )
        .map_err(db_error)
}

#[cfg(test)]
#[path = "read_page_tests.rs"]
mod tests;
