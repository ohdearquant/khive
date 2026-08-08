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

#[derive(Debug, Clone)]
pub(crate) struct RawOwnedDeclaration {
    pub(crate) owner_module_id: String,
    pub(crate) declaration_index: i64,
    pub(crate) declaration_id_type: String,
    pub(crate) declaration_id: String,
    pub(crate) entity: Option<RawEntity>,
}

#[derive(Debug, Clone)]
pub(crate) struct RawSymbolEdge {
    pub(crate) id: String,
    pub(crate) source_id: String,
    pub(crate) target_id: String,
    pub(crate) relation: String,
    pub(crate) weight: f64,
    pub(crate) last_seen_at: Option<String>,
    pub(crate) language: Option<String>,
    pub(crate) evidence_type: Option<String>,
    pub(crate) has_call: bool,
    pub(crate) has_type_reference: bool,
    pub(crate) has_unknown_evidence: bool,
}

#[derive(Debug)]
pub(crate) struct SymbolSnapshot {
    pub(crate) declarations: Vec<RawOwnedDeclaration>,
    pub(crate) edges: Vec<RawSymbolEdge>,
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
    let mut projects = read_entities(
        &conn,
        path,
        "namespace='local' AND kind='project' AND deleted_at IS NULL",
        &[],
    )?;
    let modules = read_entities(
        &conn,
        path,
        "namespace='local' AND kind='concept' AND entity_type='module' AND deleted_at IS NULL
         AND json_extract(properties,'$.source_revision')=?1
         AND json_type(properties,'$.import_scan_status')='text'",
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
             WHERE namespace='local' AND deleted_at IS NULL
               AND relation IN ('contains','depends_on')
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

pub(crate) fn read_l2_snapshot(path: &Path, revision: &str) -> Result<SymbolSnapshot, ExportError> {
    let conn = open_read_only(path)?;
    let declarations = read_owned_declarations(&conn, path, revision)?;
    let edges = read_symbol_edges(&conn, path)?;
    Ok(SymbolSnapshot {
        declarations,
        edges,
    })
}

fn read_owned_declarations(
    conn: &Connection,
    path: &Path,
    revision: &str,
) -> Result<Vec<RawOwnedDeclaration>, ExportError> {
    let mut stmt = conn
        .prepare(
            "WITH current_rust_file_modules AS (
                 SELECT id,properties
                 FROM entities
                 WHERE namespace='local'
                   AND kind='concept'
                   AND entity_type='module'
                   AND deleted_at IS NULL
                   AND json_extract(properties,'$.source_revision')=?1
                   AND json_extract(properties,'$.language')='rust'
                   AND json_type(properties,'$.import_scan_status')='text'
                   AND json_type(properties,'$.declaration_ids')='array'
             )
             SELECT m.id,d.key,d.type,COALESCE(CAST(d.value AS TEXT),''),
                    s.id,s.kind,s.entity_type,s.name,s.properties
             FROM current_rust_file_modules m
             JOIN json_each(m.properties,'$.declaration_ids') d ON TRUE
             LEFT JOIN entities s
               ON s.namespace='local'
              AND s.id=CAST(d.value AS TEXT)
              AND s.deleted_at IS NULL
             ORDER BY m.id,d.key,CAST(d.value AS TEXT)",
        )
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let rows = stmt
        .query_map([revision], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let mut declarations = Vec::new();
    for row in rows {
        let (
            owner_module_id,
            declaration_index,
            declaration_id_type,
            declaration_id,
            entity_id,
            kind,
            entity_type,
            name,
            properties,
        ) = row.map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
        let entity = match entity_id {
            Some(id) => Some(RawEntity {
                kind: kind.unwrap_or_default(),
                entity_type,
                name: name.unwrap_or_default(),
                properties: parse_properties(path, properties, &id)?,
                id,
            }),
            None => None,
        };
        declarations.push(RawOwnedDeclaration {
            owner_module_id,
            declaration_index,
            declaration_id_type,
            declaration_id,
            entity,
        });
    }
    Ok(declarations)
}

fn read_symbol_edges(conn: &Connection, path: &Path) -> Result<Vec<RawSymbolEdge>, ExportError> {
    let mut stmt = conn
        .prepare(
            "SELECT e.id,e.source_id,e.target_id,e.relation,e.weight,
                    json_extract(e.metadata,'$.last_seen_at'),
                    json_extract(e.metadata,'$.language'),
                    json_type(e.metadata,'$.l2_evidence'),
                    EXISTS (
                        SELECT 1
                        FROM json_each(COALESCE(e.metadata,'{}'),'$.l2_evidence')
                        WHERE value='call'
                    ),
                    EXISTS (
                        SELECT 1
                        FROM json_each(COALESCE(e.metadata,'{}'),'$.l2_evidence')
                        WHERE value='type_reference'
                    ),
                    EXISTS (
                        SELECT 1
                        FROM json_each(COALESCE(e.metadata,'{}'),'$.l2_evidence')
                        WHERE type<>'text' OR value NOT IN ('call','type_reference')
                    )
             FROM graph_edges e
             WHERE e.namespace='local'
               AND e.deleted_at IS NULL
               AND e.relation IN ('depends_on','implements')
               AND json_extract(e.metadata,'$.l2_derived')=1
             ORDER BY e.relation,e.source_id,e.target_id,e.id",
        )
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let rows = stmt
        .query_map([], |row| {
            Ok(RawSymbolEdge {
                id: row.get(0)?,
                source_id: row.get(1)?,
                target_id: row.get(2)?,
                relation: row.get(3)?,
                weight: row.get(4)?,
                last_seen_at: row.get(5)?,
                language: row.get(6)?,
                evidence_type: row.get(7)?,
                has_call: row.get::<_, i64>(8)? != 0,
                has_type_reference: row.get::<_, i64>(9)? != 0,
                has_unknown_evidence: row.get::<_, i64>(10)? != 0,
            })
        })
        .map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?;
    let mut edges = Vec::new();
    for row in rows {
        edges.push(row.map_err(|source| ExportError::Sqlite {
            path: path.to_path_buf(),
            source,
        })?);
    }
    Ok(edges)
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
