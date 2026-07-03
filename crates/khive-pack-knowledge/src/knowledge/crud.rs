//! CRUD handlers: upsert_atoms, upsert_domains, get, list, delete_atoms, stats.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::schema::{
    DeleteAtomsParams, GetParams, ListParams, UpsertAtomsParams, UpsertDomainsParams,
};
use super::sections::{section_from_row, section_to_json};
use super::util::{
    atom_from_row, atom_to_json, compute_embedding_coverage, deser, domain_from_row,
    domain_to_json, new_id, now_us, row_str, sql_err, status_sql_clause, status_values,
    tags_to_json, validate_atom_content,
};
use super::KnowledgeHandlers;

impl KnowledgeHandlers {
    pub(crate) async fn upsert_atoms(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
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
        let mut created = 0usize;
        let mut updated = 0usize;

        for atom_in in &p.atoms {
            let slug = atom_in.slug.trim().to_string();
            if slug.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "atom slug must not be empty".into(),
                ));
            }

            let content = atom_in.content.as_deref().unwrap_or("").trim().to_string();
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
            if let Some(ref uri) = atom_in.source_uri {
                khive_runtime::secret_gate::check(uri)?;
            }
            if let Some(ref st) = atom_in.source_type {
                khive_runtime::secret_gate::check(st)?;
            }

            let tags_json = tags_to_json(atom_in.tags.as_ref());
            let props_json = atom_in
                .properties
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let source_uri = atom_in
                .source_uri
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let source_type = atom_in
                .source_type
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());

            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("upsert_atoms reader", e))?;
            let existing = reader
                .query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(slug.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("upsert_atoms lookup", e))?;

            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("upsert_atoms writer", e))?;
            if let Some(row) = existing {
                let id = row_str(&row, "id").ok_or_else(|| {
                    RuntimeError::Internal("missing id in existing atom row".into())
                })?;
                writer
                    .execute(SqlStatement {
                        // Promote draft -> reviewed when this upsert finalizes the atom.
                        // Never demote an already reviewed row, and leave status
                        // untouched when not finalizing.
                        sql: "UPDATE knowledge_atoms SET name=?1, content=?2, tags=?3, properties=?4, source_uri=?5, source_type=?6, finalized=?7, status = CASE WHEN ?7 = 1 AND status = 'draft' THEN 'reviewed' ELSE status END, updated_at=?8 WHERE id=?9 AND namespace=?10".into(),
                        params: vec![
                            SqlValue::Text(atom_in.name.clone()),
                            SqlValue::Text(content.clone()),
                            SqlValue::Text(tags_json.clone()),
                            props_json.as_ref().map_or(SqlValue::Null, |p| SqlValue::Text(p.clone())),
                            source_uri.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                            source_type.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                            SqlValue::Integer(atom_in.finalized.unwrap_or(false) as i64),
                            SqlValue::Integer(now),
                            SqlValue::Text(id),
                            SqlValue::Text(ns.clone()),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("upsert_atoms update", e))?;
                updated += 1;
            } else {
                let id = new_id();
                writer
                    .execute(SqlStatement {
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
                            SqlValue::Text(if atom_in.finalized.unwrap_or(false) { "reviewed" } else { "draft" }.to_string()),
                            SqlValue::Integer(atom_in.finalized.unwrap_or(false) as i64),
                            SqlValue::Integer(now),
                            SqlValue::Integer(now),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("upsert_atoms insert", e))?;
                created += 1;
            }
        }

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
            let properties_json = serde_json::to_string(
                &serde_json::json!({ "members": domain_in.members.as_deref().unwrap_or(&[]) }),
            )
            .unwrap_or_else(|_| "{}".into());

            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("upsert_domains reader", e))?;
            let existing = reader
                .query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_domains WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(slug.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("upsert_domains lookup", e))?;

            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("upsert_domains writer", e))?;
            if let Some(row) = existing {
                let id = row_str(&row, "id").ok_or_else(|| {
                    RuntimeError::Internal("missing id in existing domain row".into())
                })?;
                writer
                    .execute(SqlStatement {
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
                    })
                    .await
                    .map_err(|e| sql_err("upsert_domains update", e))?;
                // Dual-write: sync the mirror atom in knowledge_atoms for FTS.
                // The domain's description text becomes the mirror atom's content.
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, properties, status, finalized, created_at, updated_at) \
                              VALUES (?1,?2,?3,?4,?5,?6,?7,'reviewed',1,?8,?9) \
                              ON CONFLICT(namespace, slug) DO UPDATE SET name=?4, content=?5, tags=?6, properties=?7, status='reviewed', updated_at=?9".into(),
                        params: vec![
                            SqlValue::Text(id),
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
                    })
                    .await
                    .map_err(|e| sql_err("upsert_domains atom mirror update", e))?;
                updated += 1;
            } else {
                let id = new_id();
                writer
                    .execute(SqlStatement {
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
                    })
                    .await
                    .map_err(|e| sql_err("upsert_domains insert", e))?;
                // Dual-write: mirror atom in knowledge_atoms for FTS indexing.
                // The domain's description text becomes the mirror atom's content.
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, properties, status, finalized, created_at, updated_at) \
                              VALUES (?1,?2,?3,?4,?5,?6,?7,'reviewed',1,?8,?9)".into(),
                        params: vec![
                            SqlValue::Text(id),
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
                    })
                    .await
                    .map_err(|e| sql_err("upsert_domains atom mirror insert", e))?;
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

        let is_uuid = id.parse::<Uuid>().is_ok();

        let mut reader = sql.reader().await.map_err(|e| sql_err("get reader", e))?;

        if is_uuid {
            // Domain-first: a domain's canonical row and its FTS mirror atom share
            // the same UUID, so the UUID branch must match the slug branch below
            // and prefer knowledge_domains — otherwise a domain UUID resolves to
            // its own mirror atom instead of the canonical domain record.
            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT * FROM knowledge_domains WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                    label: None,
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
                    sql: "SELECT * FROM knowledge_atoms WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                    params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("get atom by id", e))?;
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
        }

        // Slug lookup — domains first (authoritative for members), then atoms.
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_domains WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(id.clone())],
                label: None,
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
                label: None,
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
                        sql: "SELECT * FROM knowledge_domains WHERE namespace = ?1 AND deleted_at IS NULL ORDER BY created_at DESC LIMIT ?2 OFFSET ?3".into(),
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
                    "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND tags NOT LIKE '%type:domain%'{} ORDER BY created_at DESC LIMIT ?2 OFFSET ?3",
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
        _params: Value,
    ) -> Result<Value, RuntimeError> {
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
        let name = "sk-proj-FAKEKEY0000000000000000000000000000000000"; // gitleaks:allow
        assert!(
            check(name).is_err(),
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
