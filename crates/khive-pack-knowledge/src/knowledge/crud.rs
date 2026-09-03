//! CRUD handlers: upsert_atoms, upsert_domains, get, list, delete_atoms, stats.

use std::collections::HashMap;

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{uuid_prefix_bounds, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::schema::{
    DeleteAtomsParams, GetParams, ListParams, StatsParams, UpsertAtomsParams, UpsertDomainsParams,
};
use super::sections::{section_from_row, section_to_json};
use super::util::{
    atom_from_row, atom_to_json, compute_embedding_coverage, deser, domain_from_row,
    domain_to_json, new_id, now_us, row_i64, row_str, sql_err, status_sql_clause, status_values,
    tags_to_json, validate_atom_content,
};
use super::KnowledgeHandlers;

fn knowledge_get_prefix_statement(prefix: &str) -> Option<SqlStatement> {
    let (lower, upper) = uuid_prefix_bounds(prefix)?;
    Some(SqlStatement {
        // A domain's FTS mirror atom has the same UUID. UNION (rather than
        // UNION ALL) deduplicates that legitimate cross-table duplicate so
        // one domain is not reported as an ambiguous prefix.
        sql: "SELECT id FROM knowledge_domains \
              WHERE id >= ?1 AND id < ?2 AND deleted_at IS NULL \
              UNION \
              SELECT id FROM knowledge_atoms \
              WHERE id >= ?1 AND id < ?2 AND deleted_at IS NULL \
              ORDER BY id LIMIT 2"
            .into(),
        params: vec![SqlValue::Text(lower), SqlValue::Text(upper)],
        label: Some("knowledge.get.resolve_prefix".into()),
    })
}

impl KnowledgeHandlers {
    pub(crate) async fn upsert_atoms(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        Self::upsert_atoms_with_content_policy(runtime, token, params, false).await
    }

    /// Import has already validated the complete source document and must retain its
    /// boundary whitespace. The public upsert verb keeps its established trim behavior.
    pub(super) async fn upsert_import_atoms(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        Self::upsert_atoms_with_content_policy(runtime, token, params, true).await
    }

    async fn upsert_atoms_with_content_policy(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        preserve_content_whitespace: bool,
    ) -> Result<Value, RuntimeError> {
        let p: UpsertAtomsParams = deser(params)?;
        if p.chunk_size.is_some() {
            tracing::warn!(
                chunk_size = ?p.chunk_size,
                "upsert_atoms: chunk_size is accepted but not yet implemented; \
                 server-side chunking is not performed"
            );
        }
        if p.atoms.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "atoms list must not be empty".into(),
            ));
        }
        if p.atoms.len() > 5000 {
            return Err(RuntimeError::InvalidInput(
                "max 5000 atoms per request".into(),
            ));
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let now = now_us();

        for atom_in in &p.atoms {
            let slug = atom_in.slug.trim().to_string();
            if slug.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "atom slug must not be empty".into(),
                ));
            }

            let raw_content = atom_in.content.as_deref().unwrap_or("");
            let content = if preserve_content_whitespace {
                raw_content.to_string()
            } else {
                raw_content.trim().to_string()
            };
            validate_atom_content(&content)?;
            // Secret gate: scan all caller-supplied text and structured fields
            // before any reader/writer is acquired.
            khive_runtime::secret_gate::check(&slug)?;
            khive_runtime::secret_gate::check(&atom_in.name)?;
            khive_runtime::secret_gate::check(&content)?;
            if let Some(ref tags_vec) = atom_in.tags {
                khive_runtime::secret_gate::check_tags(tags_vec)?;
            }
            if let Some(ref props) = atom_in.properties {
                khive_runtime::secret_gate::check_json(props)?;
            }
            khive_runtime::secret_gate::reject_reserved_secret_gate_property(
                atom_in.properties.as_ref(),
            )?;
            if let Some(Some(uri)) = &atom_in.source_uri {
                khive_runtime::secret_gate::check(uri)?;
            }
            if let Some(Some(st)) = &atom_in.source_type {
                khive_runtime::secret_gate::check(st)?;
            }
        }

        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("upsert_atoms reader", e))?;
        let mut ids_by_slug: HashMap<String, String> = HashMap::new();
        let mut operations = Vec::with_capacity(p.atoms.len());
        for atom_in in &p.atoms {
            let slug = atom_in.slug.trim().to_string();
            if let Some(id) = ids_by_slug.get(&slug) {
                operations.push((id.clone(), false));
                continue;
            }

            // Look up by slug WITHOUT the deleted_at filter so a tombstoned row that
            // still owns the (namespace, slug) unique index is detected before the
            // insert path runs — otherwise SQLite raises a raw unique-constraint
            // error instead of a defined lifecycle error.
            let existing = reader
                .query_row(SqlStatement {
                    sql: "SELECT id, deleted_at, tags FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(slug.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("upsert_atoms lookup", e))?;
            if let Some(row) = &existing {
                // A domain's mirror atom shares the (namespace, slug) index with
                // ordinary atoms. Reject here (mirroring the delete_atoms guard)
                // so a plain upsert_atoms call can never blind-overwrite the
                // mirror's tags/content and desynchronize it from its domain.
                let existing_tags = row_str(row, "tags").unwrap_or_default();
                if existing_tags.contains("type:domain") {
                    return Err(RuntimeError::InvalidInput(format!(
                        "atom slug {slug:?} collides with a domain mirror; use upsert_domains instead"
                    )));
                }
                if row_i64(row, "deleted_at").is_some() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "atom slug {slug:?} was previously deleted; choose a new slug"
                    )));
                }
            }

            let (id, insert) = if let Some(row) = existing {
                (
                    row_str(&row, "id").ok_or_else(|| {
                        RuntimeError::Internal("missing id in existing atom row".into())
                    })?,
                    false,
                )
            } else {
                (new_id(), true)
            };
            ids_by_slug.insert(slug, id.clone());
            operations.push((id, insert));
        }
        drop(reader);

        let mut created = 0usize;
        let mut updated = 0usize;
        let mut statements = Vec::with_capacity(p.atoms.len());
        for (atom_in, (id, insert)) in p.atoms.iter().zip(operations) {
            let slug = atom_in.slug.trim().to_string();
            let raw_content = atom_in.content.as_deref().unwrap_or("");
            let content = if preserve_content_whitespace {
                raw_content.to_string()
            } else {
                raw_content.trim().to_string()
            };
            let tags_json = tags_to_json(atom_in.tags.as_ref());
            let props_json = atom_in
                .properties
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let source_uri = atom_in
                .source_uri
                .as_ref()
                .and_then(Option::as_ref)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            // Blank strings keep their established update behavior (preserve),
            // while JSON null is now an explicit clear.
            let source_uri_present =
                matches!(&atom_in.source_uri, Some(None)) || source_uri.is_some();
            let source_type = atom_in
                .source_type
                .as_ref()
                .and_then(Option::as_ref)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let source_type_present =
                matches!(&atom_in.source_type, Some(None)) || source_type.is_some();
            let finalized_present = atom_in.finalized.is_some();
            let finalized = atom_in.finalized.flatten().unwrap_or(false);

            if insert {
                statements.push(SqlStatement {
                        sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, properties, source_uri, source_type, status, finalized, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)".into(),
                        params: vec![
                            SqlValue::Text(id),
                            SqlValue::Text(ns.clone()),
                            SqlValue::Text(slug.clone()),
                            SqlValue::Text(atom_in.name.clone()),
                            SqlValue::Text(content.clone()),
                            SqlValue::Text(tags_json.clone()),
                            props_json.as_ref().map_or(SqlValue::Null, |p| SqlValue::Text(p.clone())),
                            source_uri.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                            source_type.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                            // status mirrors the lifecycle backfill (finalized => reviewed) so a
                            // freshly-finalized atom is never left at the 'draft' default.
                            SqlValue::Text(if finalized { "reviewed" } else { "draft" }.to_string()),
                            SqlValue::Integer(finalized as i64),
                            SqlValue::Integer(now),
                            SqlValue::Integer(now),
                        ],
                        label: None,
                    });
                created += 1;
            } else {
                statements.push(SqlStatement {
                    // Presence/value pairs distinguish omission from explicit JSON
                    // null. Finalized is non-nullable, so its null value is bound as
                    // false. Only true promotes draft -> reviewed; clearing the flag
                    // does not demote an independent lifecycle status.
                    sql: "UPDATE knowledge_atoms SET name=?1, content=?2, tags=?3, properties=?4, source_uri=CASE WHEN ?5 = 1 THEN ?6 ELSE source_uri END, source_type=CASE WHEN ?7 = 1 THEN ?8 ELSE source_type END, finalized=CASE WHEN ?9 = 1 THEN ?10 ELSE finalized END, status=CASE WHEN ?9 = 1 AND ?10 = 1 AND status = 'draft' THEN 'reviewed' ELSE status END, updated_at=?11 WHERE id=?12 AND namespace=?13".into(),
                    params: vec![
                        SqlValue::Text(atom_in.name.clone()),
                        SqlValue::Text(content),
                        SqlValue::Text(tags_json),
                        props_json.map_or(SqlValue::Null, SqlValue::Text),
                        SqlValue::Integer(source_uri_present as i64),
                        source_uri.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                        SqlValue::Integer(source_type_present as i64),
                        source_type.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                        SqlValue::Integer(finalized_present as i64),
                        SqlValue::Integer(finalized as i64),
                        SqlValue::Integer(now),
                        SqlValue::Text(id),
                        SqlValue::Text(ns.clone()),
                    ],
                    label: None,
                });
                updated += 1;
            }
        }

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| sql_err("upsert_atoms writer", e))?;
        writer
            .execute_batch(statements)
            .await
            .map_err(|e| sql_err("upsert_atoms batch", e))?;

        Ok(json!({
            "created": created,
            "updated": updated,
            "total": p.atoms.len(),
        }))
    }

    pub(crate) async fn upsert_domains(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: UpsertDomainsParams = deser(params)?;
        if p.domains.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "domains list must not be empty".into(),
            ));
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let now = now_us();
        let mut created = 0usize;
        let mut updated = 0usize;

        for domain_in in &p.domains {
            let slug = domain_in.slug.trim().to_string();
            let name = domain_in.name.trim().to_string();
            if slug.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "domain slug must not be empty".into(),
                ));
            }
            if name.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "domain name must not be empty".into(),
                ));
            }
            // Secret gate: scan slug and name first (before content-length validation)
            // so security violations short-circuit before business logic errors.
            khive_runtime::secret_gate::check(&slug)?;
            khive_runtime::secret_gate::check(&name)?;
            // Domain mirror atoms are written to knowledge_atoms with the description
            // as content. Enforce the same 20-word minimum that normal atoms must satisfy
            // so the FTS and embedding surfaces receive adequate content.
            let mirror_content = domain_in.description.as_deref().unwrap_or("").trim();
            validate_atom_content(mirror_content).map_err(|e| {
                RuntimeError::InvalidInput(format!("domain {slug:?}: description {e}"))
            })?;
            // Secret gate: scan remaining caller-supplied text.
            khive_runtime::secret_gate::check(mirror_content)?;
            if let Some(ref tags_vec) = domain_in.tags {
                khive_runtime::secret_gate::check_tags(tags_vec)?;
            }
            if let Some(ref members_vec) = domain_in.members {
                khive_runtime::secret_gate::check_tags(members_vec)?;
            }

            let mut tags: Vec<String> = domain_in.tags.clone().unwrap_or_default();
            if !tags.iter().any(|t| t == "type:domain") {
                tags.push("type:domain".to_string());
            }
            let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into());
            let members_json = match &domain_in.members {
                Some(m) => serde_json::to_string(m).unwrap_or_else(|_| "[]".into()),
                None => "[]".to_string(),
            };
            // The mirror's properties are synthesized entirely from `members`
            // (a `Vec<String>`, no arbitrary-key input); `DomainInput` carries no
            // `properties` field, so the top-level reserved key is unreachable here.
            let properties_json = serde_json::to_string(
                &serde_json::json!({ "members": domain_in.members.as_deref().unwrap_or(&[]) }),
            )
            .unwrap_or_else(|_| "{}".into());

            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("upsert_domains reader", e))?;
            // Look up by slug WITHOUT the deleted_at filter so a tombstoned domain
            // that still owns the (namespace, slug) unique index is detected before
            // the insert path runs, instead of leaking a raw unique-constraint error.
            let existing = reader
                .query_row(SqlStatement {
                    sql: "SELECT id, deleted_at FROM knowledge_domains WHERE namespace = ?1 AND slug = ?2 LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(slug.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("upsert_domains lookup", e))?;
            if let Some(row) = &existing {
                if row_i64(row, "deleted_at").is_some() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "domain slug {slug:?} was previously deleted; choose a new slug"
                    )));
                }
            }

            let id = match &existing {
                Some(row) => row_str(row, "id").ok_or_else(|| {
                    RuntimeError::Internal("missing id in existing domain row".into())
                })?,
                None => new_id(),
            };

            // Preflight: a normal atom (or a tombstoned atom that still owns the
            // unique (namespace, slug) index) must never share this domain's slug.
            // The mirror atom for THIS domain shares its id with the domain, so the
            // collision is only real when the colliding row belongs to a different
            // id. This must run BEFORE any write so a collision never leaves a
            // partially-committed domain row behind.
            let atom_collision = reader
                .query_row(SqlStatement {
                    sql:
                        "SELECT id FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 LIMIT 1"
                            .into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(slug.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("upsert_domains atom collision check", e))?;
            if let Some(row) = &atom_collision {
                let collision_id = row_str(row, "id").ok_or_else(|| {
                    RuntimeError::Internal("missing id in colliding atom row".into())
                })?;
                if collision_id != id {
                    return Err(RuntimeError::InvalidInput(format!(
                        "domain slug {slug:?} collides with an existing atom of the same slug"
                    )));
                }
            }

            // Mirror write is keyed by id (shared with the domain), never by slug —
            // ON CONFLICT(namespace, slug) would blind-overwrite an unrelated atom
            // that merely happens to share this slug.
            let mirror_stmt = SqlStatement {
                sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, properties, status, finalized, created_at, updated_at) \
                      VALUES (?1,?2,?3,?4,?5,?6,?7,'reviewed',1,?8,?9) \
                      ON CONFLICT(id) DO UPDATE SET slug=?3, name=?4, content=?5, tags=?6, properties=?7, status='reviewed', finalized=1, updated_at=?9".into(),
                params: vec![
                    SqlValue::Text(id.clone()),
                    SqlValue::Text(ns.clone()),
                    SqlValue::Text(slug.clone()),
                    SqlValue::Text(name.clone()),
                    SqlValue::Text(domain_in.description.clone().unwrap_or_default()),
                    SqlValue::Text(tags_json.clone()),
                    SqlValue::Text(properties_json.clone()),
                    SqlValue::Integer(now),
                    SqlValue::Integer(now),
                ],
                label: None,
            };

            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("upsert_domains writer", e))?;
            if existing.is_some() {
                let domain_stmt = SqlStatement {
                    sql: "UPDATE knowledge_domains SET name=?1, description=?2, tags=?3, members=?4, updated_at=?5 WHERE id=?6 AND namespace=?7".into(),
                    params: vec![
                        SqlValue::Text(name.clone()),
                        domain_in.description.as_ref().map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                        SqlValue::Text(tags_json.clone()),
                        SqlValue::Text(members_json.clone()),
                        SqlValue::Integer(now),
                        SqlValue::Text(id.clone()),
                        SqlValue::Text(ns.clone()),
                    ],
                    label: None,
                };
                // Atomic: the canonical domain row and its FTS mirror atom are one
                // logical record; a mirror-write failure must roll back the domain
                // update too.
                writer
                    .execute_batch(vec![domain_stmt, mirror_stmt])
                    .await
                    .map_err(|e| sql_err("upsert_domains update batch", e))?;
                updated += 1;
            } else {
                let domain_stmt = SqlStatement {
                    sql: "INSERT INTO knowledge_domains (id, namespace, slug, name, description, tags, members, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)".into(),
                    params: vec![
                        SqlValue::Text(id.clone()),
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(slug.clone()),
                        SqlValue::Text(name.clone()),
                        domain_in.description.as_ref().map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                        SqlValue::Text(tags_json.clone()),
                        SqlValue::Text(members_json.clone()),
                        SqlValue::Integer(now),
                        SqlValue::Integer(now),
                    ],
                    label: None,
                };
                // Atomic: a mirror-insert failure must roll back the domain insert
                // too, so no domain row is ever left committed without its mirror.
                writer
                    .execute_batch(vec![domain_stmt, mirror_stmt])
                    .await
                    .map_err(|e| sql_err("upsert_domains insert batch", e))?;
                created += 1;
            }
        }

        Ok(json!({
            "created": created,
            "updated": updated,
            "total": p.domains.len(),
        }))
    }

    pub(crate) async fn get(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: GetParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let id = p.id.trim().to_string();
        let with_sections = p.include_sections.unwrap_or(false);

        let mut reader = sql.reader().await.map_err(|e| sql_err("get reader", e))?;

        // ADR-007 Rule 2: UUID and short-prefix forms are by-ID reads, so they
        // are namespace-agnostic. Parse a complete UUID first so 32-character
        // compact UUIDs are complete identifiers, not prefixes. For non-UUID
        // input, preserve the pack's registered-slug contract: an exact slug in
        // the caller namespace wins before an all-hex value is interpreted as a
        // UUID prefix.
        let resolved_id = if let Ok(uuid) = id.parse::<Uuid>() {
            Some(uuid.to_string())
        } else {
            // Exact slug lookup precedes prefix interpretation so a registered
            // all-hex slug remains addressable. Domains are authoritative over
            // their same-slug mirror atoms.
            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT * FROM knowledge_domains WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(id.clone())],
                    label: Some("knowledge.get.domain_by_slug".into()),
                })
                .await
                .map_err(|e| sql_err("get domain by slug", e))?;
            if let Some(r) = row {
                return domain_from_row(&r)
                    .map(|d| domain_to_json(&d))
                    .ok_or_else(|| RuntimeError::Internal("domain row parse failed".into()));
            }

            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(id.clone())],
                    label: Some("knowledge.get.atom_by_slug".into()),
                })
                .await
                .map_err(|e| sql_err("get atom by slug", e))?;
            if let Some(r) = row {
                let atom = atom_from_row(&r)
                    .ok_or_else(|| RuntimeError::Internal("atom row parse failed".into()))?;
                let atom_id = atom.id.to_string();
                let mut out = atom_to_json(&atom);
                if with_sections {
                    out["sections"] = fetch_sections(runtime, &ns, &atom_id).await?;
                }
                return Ok(out);
            }

            if id.len() >= 8 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                let rows = if let Some(statement) = knowledge_get_prefix_statement(&id) {
                    reader
                        .query_all(statement)
                        .await
                        .map_err(|e| sql_err("get by prefix", e))?
                } else {
                    Vec::new()
                };
                let matches: Vec<String> =
                    rows.iter().filter_map(|row| row_str(row, "id")).collect();
                match matches.as_slice() {
                    [] => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "no knowledge record matches prefix: {id:?}"
                        )))
                    }
                    [only] => Some(only.clone()),
                    _ => {
                        return Err(RuntimeError::AmbiguousPrefix {
                            prefix: id.clone(),
                            matches: matches
                                .iter()
                                .filter_map(|matched| matched.parse::<Uuid>().ok())
                                .collect(),
                        })
                    }
                }
            } else {
                None
            }
        };

        if let Some(resolved_id) = resolved_id {
            // Domain-first: a domain's canonical row and its FTS mirror atom share
            // the same UUID, so the UUID branch must match the slug branch below
            // and prefer knowledge_domains — otherwise a domain UUID resolves to
            // its own mirror atom instead of the canonical domain record.
            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT * FROM knowledge_domains WHERE id = ?1 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(resolved_id.clone())],
                    label: Some("knowledge.get.domain_by_id".into()),
                })
                .await
                .map_err(|e| sql_err("get domain by id", e))?;
            if let Some(r) = row {
                return domain_from_row(&r)
                    .map(|d| domain_to_json(&d))
                    .ok_or_else(|| RuntimeError::Internal("domain row parse failed".into()));
            }
            let row = reader
                .query_row(SqlStatement {
                    sql:
                        "SELECT * FROM knowledge_atoms WHERE id = ?1 AND deleted_at IS NULL LIMIT 1"
                            .into(),
                    params: vec![SqlValue::Text(resolved_id)],
                    label: Some("knowledge.get.atom_by_id".into()),
                })
                .await
                .map_err(|e| sql_err("get atom by id", e))?;
            if let Some(r) = row {
                let atom = atom_from_row(&r)
                    .ok_or_else(|| RuntimeError::Internal("atom row parse failed".into()))?;
                let atom_id = atom.id.to_string();
                let atom_namespace = atom.namespace.clone();
                let mut out = atom_to_json(&atom);
                if with_sections {
                    out["sections"] = fetch_sections(runtime, &atom_namespace, &atom_id).await?;
                }
                return Ok(out);
            }

            // An ID-shaped input names one record or misses. It must not fall
            // through and accidentally resolve a slug with the same spelling.
            return Err(RuntimeError::NotFound(format!(
                "atom or domain not found: {id:?}"
            )));
        }

        Err(RuntimeError::NotFound(format!(
            "atom or domain not found: {id:?}"
        )))
    }

    pub(crate) async fn list(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ListParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let limit = p.limit.unwrap_or(20).clamp(1, 500) as i64;
        let offset = p.offset.unwrap_or(0) as i64;

        let mut reader = sql.reader().await.map_err(|e| sql_err("list reader", e))?;

        match p.kind.as_deref() {
            Some("domain") => {
                let rows = reader
                    .query_all(SqlStatement {
                        // #1671: `id` tiebreak — deterministic total order for offset pages.
                        sql: "SELECT * FROM knowledge_domains WHERE namespace = ?1 AND deleted_at IS NULL ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3".into(),
                        params: vec![
                            SqlValue::Text(ns.clone()),
                            SqlValue::Integer(limit),
                            SqlValue::Integer(offset),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("list domains", e))?;

                let total_row = reader
                    .query_scalar(SqlStatement {
                        sql: "SELECT COUNT(*) FROM knowledge_domains WHERE namespace = ?1 AND deleted_at IS NULL".into(),
                        params: vec![SqlValue::Text(ns)],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("list domains count", e))?;
                let total = match total_row {
                    Some(SqlValue::Integer(n)) => n,
                    _ => 0,
                };

                let items: Vec<Value> = rows
                    .iter()
                    .filter_map(|r| domain_from_row(r).map(|d| domain_to_json(&d)))
                    .collect();

                Ok(json!({ "results": items, "total": total, "limit": limit, "offset": offset }))
            }
            Some("atom") | None => {
                let requested_statuses = status_values(p.status.as_ref());
                let exclude_buf: Vec<&str> = p
                    .exclude_status
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .into_iter()
                    .collect();
                let (data_status_clause, data_status_params) =
                    status_sql_clause(&requested_statuses, &exclude_buf, 4);
                let (count_status_clause, count_status_params) =
                    status_sql_clause(&requested_statuses, &exclude_buf, 2);

                let sql_str = format!(
                    "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'{} ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3",
                    data_status_clause
                );
                let count_sql = format!(
                    "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'{}",
                    count_status_clause
                );

                let mut row_params = vec![
                    SqlValue::Text(ns.clone()),
                    SqlValue::Integer(limit),
                    SqlValue::Integer(offset),
                ];
                row_params.extend(data_status_params);

                let rows = reader
                    .query_all(SqlStatement {
                        sql: sql_str,
                        params: row_params,
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("list atoms", e))?;

                let mut count_params = vec![SqlValue::Text(ns)];
                count_params.extend(count_status_params);
                let total_row = reader
                    .query_scalar(SqlStatement {
                        sql: count_sql,
                        params: count_params,
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("list atoms count", e))?;
                let total = match total_row {
                    Some(SqlValue::Integer(n)) => n,
                    _ => 0,
                };

                let items: Vec<Value> = rows
                    .iter()
                    .filter_map(|r| atom_from_row(r).map(|a| atom_to_json(&a)))
                    .collect();

                Ok(json!({ "results": items, "total": total, "limit": limit, "offset": offset }))
            }
            Some(other) => Err(RuntimeError::InvalidInput(format!(
                "unknown type {other:?}; valid: atom | domain"
            ))),
        }
    }

    pub(crate) async fn delete_atoms(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: DeleteAtomsParams = deser(params)?;
        if p.cascade.is_some() {
            tracing::warn!(
                cascade = ?p.cascade,
                "delete_atoms: cascade is accepted but not yet implemented; \
                 sections are not cascade-deleted when atoms are soft-deleted"
            );
        }
        if p.ids.is_empty() {
            return Err(RuntimeError::InvalidInput("ids must not be empty".into()));
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let now = now_us();
        let mut deleted = 0usize;

        // Preflight the full request before mutating anything: knowledge.delete_atoms
        // is documented as atom-only. A domain's canonical row and its FTS mirror
        // atom share an id/slug, so deleting the mirror here would tombstone one
        // half of the domain while leaving knowledge_domains live — a split-brain
        // where direct lookup and search/suggest disagree. Domains must go through
        // the generic delete verb, which deletes both halves together.
        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("delete_atoms reader", e))?;
        for id_or_slug in &p.ids {
            let key = id_or_slug.trim().to_string();
            let domain = reader
                .query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_domains WHERE namespace = ?1 AND (id = ?2 OR slug = ?2) AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(key.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("delete_atoms domain preflight", e))?;
            if domain.is_some() {
                return Err(RuntimeError::InvalidInput(format!(
                    "knowledge.delete_atoms cannot delete domain {key:?}; use the generic delete verb by domain UUID"
                )));
            }

            let mirror = reader
                .query_row(SqlStatement {
                    sql: "SELECT tags FROM knowledge_atoms WHERE namespace = ?1 AND (id = ?2 OR slug = ?2) AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(key.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("delete_atoms mirror preflight", e))?;
            if let Some(row) = mirror {
                let tags = row_str(&row, "tags").unwrap_or_default();
                if tags.contains("type:domain") {
                    return Err(RuntimeError::InvalidInput(format!(
                        "knowledge.delete_atoms cannot delete domain mirror {key:?}; use the generic delete verb by domain UUID"
                    )));
                }
            }
        }

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| sql_err("delete_atoms writer", e))?;
        for id_or_slug in &p.ids {
            let id_or_slug = id_or_slug.trim().to_string();
            let affected = writer
                .execute(SqlStatement {
                    sql: "UPDATE knowledge_atoms SET deleted_at = ?1 WHERE namespace = ?2 AND (id = ?3 OR slug = ?3) AND deleted_at IS NULL".into(),
                    params: vec![
                        SqlValue::Integer(now),
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(id_or_slug),
                    ],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("delete_atoms update", e))?;
            deleted += affected as usize;
        }

        Ok(json!({
            "deleted": deleted,
            "requested": p.ids.len(),
        }))
    }

    pub(crate) async fn stats(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let _: StatsParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let mut reader = sql.reader().await.map_err(|e| sql_err("stats reader", e))?;

        let atom_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'".into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("stats atoms", e))?;

        let domain_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM knowledge_domains WHERE namespace = ?1 AND deleted_at IS NULL".into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("stats domains", e))?;

        let finalized_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND finalized = 1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'".into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("stats finalized", e))?;

        let event_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM events WHERE namespace = ?1 AND verb LIKE 'knowledge.%'"
                    .into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("stats events", e))?;

        let retrieval_eval_run_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM knowledge_eval_runs WHERE namespace = ?1".into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: Some("knowledge.stats.eval_run_count".into()),
            })
            .await
            .map_err(|e| sql_err("stats eval run count", e))?;

        let latest_retrieval_eval = reader
            .query_row(SqlStatement {
                sql: "SELECT run_at, precision_at_5, mrr FROM knowledge_eval_runs \
                      WHERE namespace = ?1 ORDER BY run_at DESC, rowid DESC LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: Some("knowledge.stats.latest_eval_run".into()),
            })
            .await
            .map_err(|e| sql_err("stats latest eval run", e))?;

        let total_atoms = match atom_count {
            Some(SqlValue::Integer(n)) => n,
            _ => 0,
        };
        let total_domains = match domain_count {
            Some(SqlValue::Integer(n)) => n,
            _ => 0,
        };
        let finalized = match finalized_count {
            Some(SqlValue::Integer(n)) => n,
            _ => 0,
        };
        let total_events = match event_count {
            Some(SqlValue::Integer(n)) => n,
            _ => 0,
        };
        let retrieval_eval_run_count = match retrieval_eval_run_count {
            Some(SqlValue::Integer(n)) => n,
            _ => 0,
        };
        let retrieval_eval_coverage = latest_retrieval_eval
            .as_ref()
            .and_then(|row| match row.get("precision_at_5") {
                Some(SqlValue::Float(value)) => Some(*value),
                Some(SqlValue::Integer(value)) => Some(*value as f64),
                _ => None,
            })
            .unwrap_or(0.0);
        let retrieval_eval_last_run_at =
            latest_retrieval_eval
                .as_ref()
                .and_then(|row| match row.get("run_at") {
                    Some(SqlValue::Integer(value)) => Some(*value),
                    _ => None,
                });
        let retrieval_eval_last_mrr =
            latest_retrieval_eval
                .as_ref()
                .and_then(|row| match row.get("mrr") {
                    Some(SqlValue::Float(value)) => Some(*value),
                    Some(SqlValue::Integer(value)) => Some(*value as f64),
                    _ => None,
                });

        let eval_coverage = if total_atoms > 0 {
            finalized as f64 / total_atoms as f64
        } else {
            0.0
        };

        let embedding_coverage =
            compute_embedding_coverage(runtime, token, &ns, total_atoms).await?;

        Ok(json!({
            "total_atoms": total_atoms,
            "total_domains": total_domains,
            "total_events": total_events,
            "eval_coverage": eval_coverage,
            "embedding_coverage": embedding_coverage,
            "retrieval_eval_coverage": retrieval_eval_coverage,
            "retrieval_eval_run_count": retrieval_eval_run_count,
            "retrieval_eval_last_run_at": retrieval_eval_last_run_at,
            "retrieval_eval_last_mrr": retrieval_eval_last_mrr,
            "namespace": ns,
        }))
    }
}

/// Fetch all sections for `atom_id` scoped to `ns`, ordered by `sort_order`.
/// Namespace isolation is preserved: `atom_id` was resolved under `ns` by the
/// caller, and we additionally filter `knowledge_sections.namespace = ns`.
async fn fetch_sections(
    runtime: &KhiveRuntime,
    ns: &str,
    atom_id: &str,
) -> Result<Value, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("get sections reader", e))?;

    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM knowledge_sections \
                  WHERE atom_id = ?1 AND namespace = ?2 \
                  ORDER BY sort_order ASC, created_at ASC, id ASC"
                .into(),
            params: vec![
                SqlValue::Text(atom_id.to_owned()),
                SqlValue::Text(ns.to_owned()),
            ],
            label: None,
        })
        .await
        .map_err(|e| sql_err("get sections query", e))?;

    let mut sections: Vec<Value> = Vec::with_capacity(rows.len());
    for r in &rows {
        match section_from_row(r) {
            Some(s) => sections.push(section_to_json(&s)),
            None => {
                return Err(RuntimeError::Internal(
                    "knowledge_sections row is malformed (invalid UUID or section_type); \
                     data integrity check required"
                        .into(),
                ));
            }
        }
    }

    Ok(Value::Array(sections))
}

#[cfg(test)]
mod tests {
    // Gate wiring tests: confirm that the secret check integrated into
    // upsert_atoms fires on credential-shaped atom content and passes on
    // allowlisted content (sha256 hex, UUIDs).  Tests call
    // `khive_runtime::secret_gate::check` directly with the same inputs
    // that the handler would pass — this proves the gate is reachable without
    // requiring a live DB connection.

    use khive_runtime::secret_gate::check;
    use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
    use khive_storage::SqlValue;

    #[tokio::test]
    async fn get_prefix_query_plan_uses_primary_key_range_seeks() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(crate::KnowledgePack::new(runtime.clone()));
        let registry = builder.build().expect("registry builds");
        registry.apply_schema_plans(runtime.backend());

        let mut reader = runtime.sql().reader().await.expect("prefix plan reader");
        let rows = reader
            .explain(
                super::knowledge_get_prefix_statement("0b6cf134")
                    .expect("valid compact UUID prefix"),
            )
            .await
            .expect("explain knowledge prefix query");
        let details: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("detail") {
                Some(SqlValue::Text(detail)) => Some(detail.clone()),
                _ => None,
            })
            .collect();

        assert!(
            ["knowledge_domains", "knowledge_atoms"]
                .iter()
                .all(|table| {
                    details.iter().any(|detail| {
                        detail.contains(&format!("SEARCH {table}"))
                            && detail.contains("id>?")
                            && detail.contains("id<?")
                    }) && !details
                        .iter()
                        .any(|detail| detail.contains(&format!("SCAN {table}")))
                }),
            "knowledge.get short-id tables must use primary-key range seeks: {details:?}"
        );
    }

    #[test]
    fn atom_body_with_fake_aws_key_is_blocked() {
        // Fake AWS access key ID in an atom body — must be blocked.
        let body = "provider: aws\naccess_key_id: AKIAFAKE000000000000\nregion: us-east-1";
        assert!(
            check(body).is_err(),
            "atom body containing fake AWS key must be blocked"
        );
    }

    #[test]
    fn atom_body_with_sha256_hash_passes() {
        // A manifest-style line containing a sha256 digest — must pass the allowlist.
        let body =
            "checksum = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\"";
        assert!(
            check(body).is_ok(),
            "atom body with sha256 manifest hash must pass; fired: {:?}",
            check(body).err()
        );
    }

    #[test]
    fn atom_name_with_fake_openai_key_is_blocked() {
        // A credential accidentally used as an atom name — must be blocked.
        let name = format!("sk-proj-{}", "A".repeat(80));
        assert!(
            check(&name).is_err(),
            "atom name containing fake OpenAI key must be blocked"
        );
    }

    #[test]
    fn normal_atom_name_passes() {
        let name = "FlashAttention-2: efficient transformer self-attention";
        assert!(
            check(name).is_ok(),
            "normal atom name must pass; fired: {:?}",
            check(name).err()
        );
    }

    // Ignored-param warning coverage: verify that chunk_size and cascade are still
    // accepted by the param structs (no deserialization error) and that the fields
    // are Some when supplied, confirming the warning branch precondition is satisfiable.

    #[test]
    fn upsert_atoms_chunk_size_accepted_and_detectable() {
        use crate::knowledge::schema::UpsertAtomsParams;
        let p: UpsertAtomsParams = serde_json::from_value(serde_json::json!({
            "atoms": [{"slug": "s", "name": "n", "content": "placeholder content for test"}],
            "chunk_size": 100,
        }))
        .expect("upsert_atoms params with chunk_size must deserialize without error");
        assert!(
            p.chunk_size.is_some(),
            "chunk_size must be Some when supplied so the warning branch precondition is satisfiable"
        );
    }

    #[test]
    fn delete_atoms_cascade_accepted_and_detectable() {
        use crate::knowledge::schema::DeleteAtomsParams;
        let p: DeleteAtomsParams = serde_json::from_value(serde_json::json!({
            "ids": ["some-atom-id"],
            "cascade": true,
        }))
        .expect("delete_atoms params with cascade must deserialize without error");
        assert!(
            p.cascade.is_some(),
            "cascade must be Some when supplied so the warning branch precondition is satisfiable"
        );
    }
}
