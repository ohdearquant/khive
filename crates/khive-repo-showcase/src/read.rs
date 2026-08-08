use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::ExportError;

#[derive(Debug, Clone)]
pub(crate) struct RawEntity {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) entity_type: Option<String>,
    pub(crate) name: String,
    pub(crate) properties: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct RawNote {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) content: String,
    pub(crate) properties: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct RawEdge {
    pub(crate) id: String,
    pub(crate) source_id: String,
    pub(crate) target_id: String,
    pub(crate) relation: String,
    pub(crate) weight: f64,
}

#[derive(Debug)]
pub(crate) struct HistoryData {
    pub(crate) project_id: String,
    pub(crate) notes: Vec<RawNote>,
    pub(crate) edges: Vec<RawEdge>,
}

#[derive(Debug)]
pub(crate) struct MapData {
    pub(crate) projects: Vec<RawEntity>,
    pub(crate) modules: Vec<RawEntity>,
    pub(crate) edges: Vec<RawEdge>,
}

fn open_read_only(path: &Path) -> Result<Connection, ExportError> {
    if !path.is_file() {
        return Err(ExportError::MissingDatabase(path.to_path_buf()));
    }
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| ExportError::Sqlite {
        path: path.to_path_buf(),
        source,
    })
}

fn parse_properties(path: &Path, raw: Option<String>, record: &str) -> Result<Value, ExportError> {
    match raw {
        Some(raw) => serde_json::from_str(&raw).map_err(|source| ExportError::InvalidJson {
            path: path.to_path_buf(),
            record: record.to_string(),
            source,
        }),
        None => Ok(Value::Object(Default::default())),
    }
}

pub(crate) fn read_history(
    path: &Path,
    repo_slug: &str,
    canonical_url: &str,
) -> Result<HistoryData, ExportError> {
    let conn = open_read_only(path)?;
    let exact = history_project_ids(
        &conn,
        path,
        "name=?1
         OR json_extract(properties,'$.repo_slug')=?1
         OR json_extract(properties,'$.repo_url')=?2
         OR json_extract(properties,'$.canonical_url')=?2
         OR json_extract(properties,'$.source_uri')=?2",
        &[repo_slug, canonical_url],
    )?;
    let candidates = if exact.is_empty() {
        let legacy_slug = repo_slug
            .split_once('/')
            .map(|(_, rest)| rest)
            .unwrap_or(repo_slug);
        history_project_ids(
            &conn,
            path,
            "name=?1 OR json_extract(properties,'$.repo_slug')=?1",
            &[legacy_slug],
        )?
    } else {
        exact
    };
    let project_id = match candidates.as_slice() {
        [project_id] => project_id.clone(),
        [] => {
            return Err(ExportError::HistoryProjectNotFound {
                repo_slug: repo_slug.to_string(),
                path: path.to_path_buf(),
            })
        }
        _ => {
            return Err(ExportError::AmbiguousHistoryProject {
                repo_slug: repo_slug.to_string(),
                count: candidates.len(),
                path: path.to_path_buf(),
            })
        }
    };

    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT n.id,n.kind,COALESCE(n.name,''),n.content,n.properties
             FROM notes n
             JOIN graph_edges e ON e.source_id=n.id
             WHERE n.deleted_at IS NULL AND e.deleted_at IS NULL
               AND e.relation='annotates' AND e.target_id=?1
               AND n.kind IN ('commit','issue','pull_request')
             ORDER BY n.kind,n.id",
        )
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let rows = stmt
        .query_map([&project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let mut notes = Vec::new();
    for row in rows {
        let (id, kind, name, content, properties): (
            String,
            String,
            String,
            String,
            Option<String>,
        ) = row.map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
        let properties = parse_properties(path, properties, &id)?;
        notes.push(RawNote {
            id,
            kind,
            name,
            content,
            properties,
        });
    }

    let note_ids = notes
        .iter()
        .map(|note| note.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut stmt = conn
        .prepare(
            "SELECT id,source_id,target_id,relation,weight FROM graph_edges
             WHERE deleted_at IS NULL
               AND relation IN ('precedes','annotates')
             ORDER BY relation,source_id,target_id,id",
        )
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let rows = stmt
        .query_map([], |row| {
            Ok(RawEdge {
                id: row.get(0)?,
                source_id: row.get(1)?,
                target_id: row.get(2)?,
                relation: row.get(3)?,
                weight: row.get(4)?,
            })
        })
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let mut edges = Vec::new();
    for row in rows {
        let edge = row.map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
        if note_ids.contains(edge.source_id.as_str())
            && (note_ids.contains(edge.target_id.as_str()) || edge.target_id == project_id)
        {
            edges.push(edge);
        }
    }
    Ok(HistoryData {
        project_id,
        notes,
        edges,
    })
}

fn history_project_ids(
    conn: &Connection,
    path: &Path,
    identity_predicate: &str,
    params: &[&str],
) -> Result<Vec<String>, ExportError> {
    let sql = format!(
        "SELECT id FROM entities
         WHERE kind='project' AND deleted_at IS NULL AND ({identity_predicate})
         ORDER BY id"
    );
    let mut stmt = conn.prepare(&sql).map_err(|source| ExportError::Sqlite {
        path: path.to_path_buf(),
        source,
    })?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row.map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?);
    }
    Ok(ids)
}

pub(crate) fn read_map(path: &Path, revision: &str) -> Result<MapData, ExportError> {
    let conn = open_read_only(path)?;
    let mut projects = read_entities(&conn, path, "kind='project' AND deleted_at IS NULL", &[])?;
    let modules = read_entities(
        &conn,
        path,
        "kind='concept' AND entity_type='module' AND deleted_at IS NULL
         AND json_extract(properties,'$.source_revision')=?1",
        &[revision],
    )?;
    let source_projects = modules
        .iter()
        .filter_map(|module| {
            module
                .properties
                .get("source_project")
                .and_then(Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    projects.retain(|project| {
        let identity = project
            .properties
            .get("source_project")
            .and_then(Value::as_str)
            .unwrap_or(&project.name);
        source_projects.contains(identity)
    });
    let ids = projects
        .iter()
        .chain(modules.iter())
        .map(|entity| entity.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut stmt = conn
        .prepare(
            "SELECT id,source_id,target_id,relation,weight FROM graph_edges
             WHERE deleted_at IS NULL AND relation IN ('contains','depends_on')
             ORDER BY relation,source_id,target_id,id",
        )
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let rows = stmt
        .query_map([], |row| {
            Ok(RawEdge {
                id: row.get(0)?,
                source_id: row.get(1)?,
                target_id: row.get(2)?,
                relation: row.get(3)?,
                weight: row.get(4)?,
            })
        })
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let mut edges = Vec::new();
    for row in rows {
        let edge = row.map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
        if ids.contains(edge.source_id.as_str()) && ids.contains(edge.target_id.as_str()) {
            edges.push(edge);
        }
    }
    Ok(MapData {
        projects,
        modules,
        edges,
    })
}

fn read_entities(
    conn: &Connection,
    path: &Path,
    predicate: &str,
    params: &[&str],
) -> Result<Vec<RawEntity>, ExportError> {
    let sql = format!(
        "SELECT id,kind,entity_type,name,properties FROM entities WHERE {predicate} ORDER BY id"
    );
    let mut stmt = conn.prepare(&sql).map_err(|source| ExportError::Sqlite {
        path: path.to_path_buf(),
        source,
    })?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let mut entities = Vec::new();
    for row in rows {
        let (id, kind, entity_type, name, properties): (
            String,
            String,
            Option<String>,
            String,
            Option<String>,
        ) = row.map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
        entities.push(RawEntity {
            properties: parse_properties(path, properties, &id)?,
            id,
            kind,
            entity_type,
            name,
        });
    }
    Ok(entities)
}
