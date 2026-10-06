use super::*;

// ---------------------------------------------------------------------------
// Transactional merge SQL helpers
// ---------------------------------------------------------------------------

/// Cheap SQL-side byte-length probe for one merge entity, evaluated BEFORE
/// [`read_merge_entity`] copies its columns into Rust `String`s and parses
/// `properties`/`tags` as JSON. `LENGTH()` still requires SQLite to touch the
/// stored bytes, but skips the Rust-side allocation and JSON parse — the
/// expensive part for an oversized record. Charging this probe against the
/// budget before the full read means an over-budget record is rejected
/// without ever being materialized or parsed inside the writer transaction.
/// Each column is wrapped in `CAST(... AS BLOB)` — plain `LENGTH(text)`
/// returns SQLite's *character* count for TEXT values, not the UTF-8 byte
/// count the budget is denominated in, so a multibyte (CJK/emoji) record
/// could under-report and pass a probe its true byte size exceeds. Casting
/// to BLOB forces `LENGTH()` to report octets instead.
/// A missing row probes as zero; `read_merge_entity`'s own "not found" error
/// fires on the subsequent full read and is unaffected by this probe.
fn probe_merge_entity_bytes(conn: &rusqlite::Connection, id: Uuid) -> Result<usize, SqliteError> {
    let id_str = id.to_string();
    let len: Option<i64> = conn
        .query_row(
            "SELECT LENGTH(CAST(name AS BLOB)) \
                    + COALESCE(LENGTH(CAST(description AS BLOB)), 0) \
                    + COALESCE(LENGTH(CAST(properties AS BLOB)), 0) \
                    + LENGTH(CAST(tags AS BLOB)) \
             FROM entities WHERE id = ?1 AND deleted_at IS NULL",
            rusqlite::params![id_str],
            |row| row.get(0),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?;
    Ok(128_usize.saturating_add(len.unwrap_or(0).max(0) as usize))
}

/// Read one entity row by ID within a namespace, returning `SqliteError` on missing/wrong-ns.
fn read_merge_entity(
    conn: &rusqlite::Connection,
    id: Uuid,
    namespace: &str,
) -> Result<Entity, SqliteError> {
    let id_str = id.to_string();
    let mut stmt = conn.prepare(
        "SELECT id, namespace, kind, entity_type, name, description, properties, tags, \
         created_at, updated_at, deleted_at, merged_into, merge_event_id, \
         (SELECT a.content_ref FROM attachments AS a \
          WHERE a.record_uuid = entities.id AND a.substrate = 'entity' \
            AND a.role = 'content') AS content_ref, entities.version \
         FROM entities WHERE id = ?1 AND deleted_at IS NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![id_str])?;
    let row = rows
        .next()?
        .ok_or_else(|| SqliteError::InvalidData(format!("entity {id} not found")))?;

    let id_s: String = row.get(0)?;
    let ns: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let entity_type: Option<String> = row.get(3)?;
    let name: String = row.get(4)?;
    let description: Option<String> = row.get(5)?;
    let properties_str: Option<String> = row.get(6)?;
    let tags_str: String = row.get(7)?;
    let created_at: i64 = row.get(8)?;
    let updated_at: i64 = row.get(9)?;
    let deleted_at: Option<i64> = row.get(10)?;
    let merged_into_str: Option<String> = row.get(11)?;
    let merge_event_id_str: Option<String> = row.get(12)?;
    let content_ref: Option<String> = row.get(13)?;
    let version: i64 = row.get(14)?;

    if ns != namespace {
        return Err(SqliteError::InvalidData(format!(
            "entity {id} belongs to namespace '{ns}', not '{namespace}'"
        )));
    }

    let entity_id = Uuid::parse_str(&id_s).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let properties: Option<Value> = properties_str
        .map(|s| {
            serde_json::from_str::<Value>(&s).map_err(|e| SqliteError::InvalidData(e.to_string()))
        })
        .transpose()?;
    let tags: Vec<String> =
        serde_json::from_str(&tags_str).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let merged_into = merged_into_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let merge_event_id = merge_event_id_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| SqliteError::InvalidData(e.to_string()))?;

    Ok(Entity {
        id: entity_id,
        namespace: ns,
        kind,
        entity_type,
        name,
        description,
        properties,
        tags,
        created_at,
        updated_at,
        version,
        deleted_at,
        merged_into,
        merge_event_id,
        content_ref,
    })
}

/// All merge SQL on one connection inside an already-open `BEGIN IMMEDIATE` transaction.
///
/// Reads both entities, rewires/drops incident edges, merges entity fields, updates FTS,
/// deletes the `from` vec entry (if `vec_table` is Some), and tombstones `from` with merge
/// provenance.  Returns the updated `into` entity so the caller can do the async vec re-insert.
///
/// When `dry_run` is true, all reads and computations are performed but no writes are issued.
// REASON: merge requires both entity IDs, the namespace, FTS and vec table names, merge
// policy, and dry-run flag — all are load-bearing; reducing to a struct would obscure
// the sync/async boundary split that keeps this function off the async runtime.
#[allow(clippy::too_many_arguments)]
pub(super) fn merge_entity_sql(
    conn: &rusqlite::Connection,
    namespace: String,
    fts_table: String,
    vec_tables: Vec<String>,
    into_id: Uuid,
    from_id: Uuid,
    strategy: EntityDedupMergePolicy,
    content_strategy: ContentMergeStrategy,
    dry_run: bool,
    pack_rules: Vec<EdgeEndpointRule>,
    validation: EntityMergeValidation,
    limits: MergeTxLimits,
    merge_event_id: Uuid,
    event_context: Option<MergeEventContext>,
) -> Result<(MergeSummary, Entity), MergeSqlError> {
    let mut budget = MergeTxBudget::new(limits);
    // Config-scaled fanout (one FTS/vector delete per table, one contract rule
    // set per pack) is charged in bytes only: it is bounded by configuration,
    // not by graph shape, but belongs in the same account it amortizes over.
    budget.charge(
        0,
        vec_tables.iter().map(String::len).sum::<usize>()
            + pack_rules.len() * std::mem::size_of::<EdgeEndpointRule>(),
        "preparing pack and vector fanout",
    )?;

    budget.charge(
        1,
        probe_merge_entity_bytes(conn, into_id)?,
        "reading merge records",
    )?;
    let into_entity = read_merge_entity(conn, into_id, &namespace)?;
    budget.charge(
        1,
        probe_merge_entity_bytes(conn, from_id)?,
        "reading merge records",
    )?;
    let from_entity = read_merge_entity(conn, from_id, &namespace)?;

    // ADR-115 A1: no production stamp is admitted yet. Check both guarded
    // preimages, even when the chosen fold would discard or replace a key.
    for properties in [&into_entity.properties, &from_entity.properties] {
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())
            .map_err(MergeSqlError::Refusal)?;
    }

    match validation {
        EntityMergeValidation::LegacyKind if into_entity.kind != from_entity.kind => {
            return Err(MergeSqlError::Refusal(
                EntityMergeRefusal::LegacyKind {
                    into_id,
                    into_kind: into_entity.kind,
                    from_id,
                    from_kind: from_entity.kind,
                }
                .into_runtime_error(),
            ));
        }
        EntityMergeValidation::SafetyFloor => {
            validate_entity_merge_floor(&into_entity, &from_entity).map_err(|guard| {
                MergeSqlError::Refusal(EntityMergeRefusal::SafetyFloor(guard).into_runtime_error())
            })?;
        }
        EntityMergeValidation::LegacyKind | EntityMergeValidation::Forced => {}
    }

    // --- Collect edges incident to from_id ---
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));

    let from_str = from_id.to_string();

    // Namespace-agnostic (khive#1236): edge endpoints resolve by-ID regardless of
    // namespace (ADR-007 Rev 6), and `link` stamps an edge with its *creator's*
    // namespace, not either endpoint's — so an edge incident to `from_id` can live
    // in any namespace. Scoping this collection to the merge's own namespace missed
    // those edges entirely. Each row's own `namespace` column is carried through
    // (`EdgeRow::namespace`) and used for every subsequent SQL op against that row.
    let mut outbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE source_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            outbound.push(edge);
        }
    }

    let mut inbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE target_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            inbound.push(edge);
        }
    }

    // Deduplicate by edge ID (a self-edge from_id→from_id appears in both lists).
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut all_edges: Vec<EdgeRow> = Vec::new();
    for edge in outbound.into_iter().chain(inbound) {
        if seen.insert(edge.id) {
            all_edges.push(edge);
        }
    }
    let original_edges: HashMap<Uuid, EdgeRow> = all_edges
        .iter()
        .map(|edge| (edge.id, edge.clone()))
        .collect();

    // --- Merge entity fields ---
    let (merged_props, properties_merged) =
        merge_properties(&into_entity.properties, &from_entity.properties, strategy);
    crate::secret_gate::reject_reserved_secret_gate_property(merged_props.as_ref())
        .map_err(MergeSqlError::Refusal)?;
    let merged_name = merge_string_field(&into_entity.name, &from_entity.name, strategy);
    let (merged_description, content_appended) = match content_strategy {
        ContentMergeStrategy::Append => {
            let into_desc = into_entity.description.as_deref().unwrap_or("");
            let from_desc = from_entity.description.as_deref().unwrap_or("");
            if from_desc.is_empty() {
                (into_entity.description.clone(), false)
            } else if into_desc.is_empty() {
                (from_entity.description.clone(), true)
            } else {
                (Some(format!("{}\n\n---\n\n{}", into_desc, from_desc)), true)
            }
        }
        // Description selection follows `content_strategy` directly — it is a
        // deliberate, independently-settable choice, not derived from the
        // entity-field `strategy` (properties/name/tags merge policy).
        ContentMergeStrategy::PreferInto => (into_entity.description.clone(), false),
        ContentMergeStrategy::PreferFrom => (from_entity.description.clone(), false),
    };
    let (merged_tags, tags_unioned) = union_tags(&into_entity.tags, &from_entity.tags);

    let now = chrono::Utc::now().timestamp_micros();
    let into_str = into_id.to_string();
    let props_str = merged_props
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let tags_json = serde_json::to_string(&merged_tags).unwrap_or_else(|_| "[]".to_string());

    // Writes are gated on `!dry_run` below, but the loop itself always runs so a
    // dry-run response reports a predictive `edges_rewired` count instead of zero.
    let mut rewired_edge_ids = HashSet::new();
    let mut edges_contract_skipped = 0usize;
    let mut edge_conflict_preimages = Vec::new();
    let mut edges_self_loop_dropped = 0usize;
    let mut self_loop_edge_preimages = Vec::new();
    let mut self_loop_incident_edge_preimages = Vec::new();
    let mut contract_drop_edge_preimages = Vec::new();
    let mut contract_drop_incident_edge_preimages = Vec::new();
    let mut planned_deleted_edge_ids = HashSet::new();
    for edge in all_edges {
        if planned_deleted_edge_ids.contains(&edge.id) {
            continue;
        }
        let raw_src = if edge.source_id == from_id {
            into_id
        } else {
            edge.source_id
        };
        let raw_tgt = if edge.target_id == from_id {
            into_id
        } else {
            edge.target_id
        };
        let relation_typed = edge.relation.parse::<EdgeRelation>().ok();
        // Symmetric relations must be stored with source_uuid < target_uuid.
        // Apply canonicalization so the conflict check and UPDATE both use the canonical form.
        let (new_src, new_tgt) = match relation_typed {
            Some(rel) => canonical_edge_endpoints(rel, raw_src, raw_tgt),
            None => (raw_src, raw_tgt),
        };

        if new_src == new_tgt {
            let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                conn,
                edge.id,
                &original_edges,
                &planned_deleted_edge_ids,
                &mut budget,
                "collecting self-loop cascade rows",
            )?;
            for incident in &incident_edge_preimages {
                planned_deleted_edge_ids.insert(incident.id);
                rewired_edge_ids.remove(&incident.id);
            }
            planned_deleted_edge_ids.insert(edge.id);
            self_loop_edge_preimages.push(edge_row_preimage(&edge)?);
            self_loop_incident_edge_preimages.extend(incident_edge_preimages.iter().cloned());
            edges_self_loop_dropped += 1;
            if !dry_run {
                delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
            }
            continue;
        }

        // Endpoint-contract check (khive#1216): the rewired triple must still pass
        // the same allowlist `link` enforces. `into_id` and `from_id` share `kind`
        // (enforced by the caller), but `entity_type` may differ between them, so a
        // pack rule scoped via `EntityOfType` can accept `from_id`'s edge yet reject
        // the post-rewrite pair against `into_id`. A violating edge is dropped and
        // counted, mirroring the existing dangling-endpoint skip behavior rather
        // than silently writing a contract-violating edge or aborting the merge.
        let contract_ok = match relation_typed {
            // `annotates` targets may be events or edges, which
            // `resolve_merge_edge_endpoint` cannot resolve — evaluate its
            // (unconditional) exemption before endpoint resolution so valid
            // annotates edges are not dropped as unresolvable.
            Some(EdgeRelation::Annotates) => true,
            Some(rel) => {
                let src_info = if new_src == into_id {
                    Some((
                        "entity",
                        into_entity.kind.clone(),
                        into_entity.entity_type.clone(),
                    ))
                } else {
                    resolve_merge_edge_endpoint_budgeted(conn, new_src, &mut budget)?
                };
                let tgt_info = if new_tgt == into_id {
                    Some((
                        "entity",
                        into_entity.kind.clone(),
                        into_entity.entity_type.clone(),
                    ))
                } else {
                    resolve_merge_edge_endpoint_budgeted(conn, new_tgt, &mut budget)?
                };
                match (src_info, tgt_info) {
                    (Some((src_sub, src_kind, src_type)), Some((tgt_sub, tgt_kind, tgt_type))) => {
                        merge_rewire_endpoint_contract_allows(
                            &pack_rules,
                            rel,
                            src_sub,
                            &src_kind,
                            src_type.as_deref(),
                            tgt_sub,
                            &tgt_kind,
                            tgt_type.as_deref(),
                        )
                    }
                    // An endpoint no longer resolves (e.g. concurrently hard-deleted)
                    // — cannot evaluate the contract, so drop rather than assume ok.
                    _ => false,
                }
            }
            // Relation string predates the closed EdgeRelation enum (pre-migration
            // data); leave existing behavior in place rather than guessing.
            None => true,
        };
        if !contract_ok {
            let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                conn,
                edge.id,
                &original_edges,
                &planned_deleted_edge_ids,
                &mut budget,
                "collecting contract-drop cascade rows",
            )?;
            for incident in &incident_edge_preimages {
                planned_deleted_edge_ids.insert(incident.id);
                rewired_edge_ids.remove(&incident.id);
            }
            planned_deleted_edge_ids.insert(edge.id);
            contract_drop_edge_preimages.push(edge_row_preimage(&edge)?);
            contract_drop_incident_edge_preimages.extend(incident_edge_preimages.iter().cloned());
            if !dry_run {
                delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
            }
            tracing::warn!(
                edge_id = %edge.id,
                source = %new_src,
                target = %new_tgt,
                relation = %edge.relation,
                "merge_entity: dropping rewired edge — endpoint contract violation post-merge"
            );
            edges_contract_skipped += 1;
            continue;
        }

        let now_ts = chrono::Utc::now().timestamp_micros();
        // Preserve the original edge ID where possible so callers can still get()
        // it by the ID returned from link(): update in-place when there's no
        // conflict; when into_id already owns this (source,target,relation), the
        // incoming (from-side) duplicate is dropped and the existing into-edge is
        // left untouched (ADR-039 `ON CONFLICT ... DO NOTHING` semantics).
        // Check for a conflict: does into_id already have this natural key?
        let conflict_id: Option<String> = {
            let conflict_src = new_src.to_string();
            let conflict_tgt = new_tgt.to_string();
            conn.query_row(
                khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL,
                rusqlite::params![
                    &edge.namespace,
                    &conflict_src,
                    &conflict_tgt,
                    &edge.relation,
                    edge.id.to_string(),
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(SqliteError::Rusqlite)?
        };

        if let Some(conflict_id) = conflict_id {
            // A live or soft-deleted row already owns this natural key: drop the
            // incoming duplicate. The surviving row's weight/metadata/deleted_at
            // are never mutated or resurrected. Capture the duplicate and the
            // complete hard-delete cascade before removing either, so the audit
            // event contains enough state to restore every destroyed row.
            let surviving_edge_id = Uuid::parse_str(&conflict_id)
                .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
            let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                conn,
                edge.id,
                &original_edges,
                &planned_deleted_edge_ids,
                &mut budget,
                "collecting conflict cascade rows",
            )?;
            for incident in &incident_edge_preimages {
                planned_deleted_edge_ids.insert(incident.id);
                rewired_edge_ids.remove(&incident.id);
            }
            planned_deleted_edge_ids.insert(edge.id);
            rewired_edge_ids.insert(edge.id);

            if !dry_run {
                delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
            }
            edge_conflict_preimages.push(MergeEdgeConflictPreimage {
                surviving_edge_id,
                dropped_edge: edge_row_preimage(&edge)?,
                incident_edge_preimages,
            });
        } else {
            if dry_run {
                rewired_edge_ids.insert(edge.id);
                continue;
            }
            let changed = conn.execute(
                "UPDATE graph_edges SET \
                     source_id = ?1, target_id = ?2, updated_at = ?3 \
                     WHERE namespace = ?4 AND id = ?5",
                rusqlite::params![
                    new_src.to_string(),
                    new_tgt.to_string(),
                    now_ts,
                    &edge.namespace,
                    edge.id.to_string(),
                ],
            )?;
            if changed > 0 {
                rewired_edge_ids.insert(edge.id);
            }
        }
    }
    let edges_rewired = rewired_edge_ids.len();

    if !dry_run {
        // UPDATE only the merged fields — a full-row INSERT OR REPLACE silently
        // nulls any column missing from its list (entity_type and the former
        // entity-owned content_ref were lost this way; khive#1214). Attachments
        // now live in their own table and this targeted UPDATE leaves them alone.
        conn.execute(
            "UPDATE entities SET version = version + 1, \
                 name = ?1, description = ?2, properties = ?3, tags = ?4, \
                 updated_at = ?5, merged_into = NULL, merge_event_id = NULL \
             WHERE namespace = ?6 AND id = ?7",
            rusqlite::params![
                &merged_name,
                &merged_description,
                &props_str,
                &tags_json,
                now,
                &namespace,
                &into_str,
            ],
        )?;

        // Body formula mirrors entity_fts_document (the canonical constructor):
        // this path is sync/spawn_blocking so it can't call it directly, but
        // must stay field-identical.
        let fts_body = match &merged_description {
            Some(d) if !d.is_empty() => format!("{} {}", merged_name, d),
            _ => merged_name.clone(),
        };
        let kind_str = SubstrateKind::Entity.to_string();
        let fts_map = khive_db::stores::text::rowid_map_table(&fts_table);

        // `into`'s old FTS row (via the map, not a namespace/subject_id
        // scan), then the new merged row, then the map upsert to the new
        // rowid. No separate map-row delete first: `INSERT OR REPLACE`
        // overwrites it in place (see `delete_document_statement`'s doc
        // comment in khive-db).
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2) \
                 AND namespace = ?1 AND subject_id = ?2"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;
        conn.execute(
            &format!(
                "INSERT INTO {} \
                (subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                fts_table
            ),
            rusqlite::params![
                &into_str,
                &kind_str,
                &merged_name,
                &fts_body,
                &tags_json,
                &namespace,
                &props_str,
                now,
                &into_entity.kind,
            ],
        )?;
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO {fts_map} (namespace, subject_id, rowid) \
                 VALUES (?1, ?2, last_insert_rowid())"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;

        // `from`'s FTS row is gone for good (merged away, not reinserted) —
        // its map row must be removed too, or it would keep pointing at a
        // rowid the DELETE above already reclaimed.
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2) \
                 AND namespace = ?1 AND subject_id = ?2"
            ),
            rusqlite::params![&namespace, &from_str],
        )?;
        conn.execute(
            &format!("DELETE FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2"),
            rusqlite::params![&namespace, &from_str],
        )?;

        khive_db::stores::vectors::delete_subject_from_vector_tables(
            conn,
            &vec_tables,
            from_id,
            &namespace,
        )?;

        conn.execute(
            "UPDATE entities \
             SET deleted_at = ?1, merged_into = ?2, merge_event_id = ?3, updated_at = ?1, version = version + 1 \
             WHERE namespace = ?4 AND id = ?5 AND deleted_at IS NULL",
            rusqlite::params![
                now,
                into_str,
                merge_event_id.to_string(),
                &namespace,
                &from_str,
            ],
        )?;
    }

    let updated_entity = Entity {
        id: into_id,
        namespace,
        kind: into_entity.kind,
        entity_type: into_entity.entity_type,
        name: merged_name,
        description: merged_description,
        properties: merged_props,
        tags: merged_tags,
        created_at: into_entity.created_at,
        updated_at: now,
        deleted_at: into_entity.deleted_at,
        merged_into: None,
        merge_event_id: None,
        version: if dry_run {
            into_entity.version
        } else {
            into_entity
                .version
                .checked_add(1)
                .ok_or_else(|| SqliteError::InvalidData("entity version overflow".into()))?
        },
        content_ref: into_entity.content_ref,
    };

    let summary = MergeSummary {
        kept_id: into_id,
        removed_id: from_id,
        edges_rewired,
        edges_self_loop_dropped,
        self_loop_edge_preimages,
        self_loop_incident_edge_preimages,
        edges_contract_skipped,
        contract_drop_edge_preimages,
        contract_drop_incident_edge_preimages,
        edge_conflict_preimages,
        properties_merged,
        tags_unioned,
        content_appended,
        dry_run,
        tx_budget: budget.report(),
        embedding_truncation: Default::default(),
        post_commit_reindex_error: None,
    };
    // The event is the only durable copy of destructive edge preimages. An
    // insertion failure must abort this transaction along with the merge.
    if !dry_run {
        if let Some(context) = event_context {
            append_merge_event_in_transaction(conn, context, &summary, &updated_entity.namespace)?;
        }
    }
    Ok((summary, updated_entity))
}

// ---------------------------------------------------------------------------
// Note merge SQL helpers
// ---------------------------------------------------------------------------

/// Cheap SQL-side byte-length probe for one merge note — see
/// [`probe_merge_entity_bytes`] for why this runs before
/// [`read_merge_note`]'s full column copy and JSON parse, and why each
/// column is cast to BLOB before `LENGTH()`.
fn probe_merge_note_bytes(conn: &rusqlite::Connection, id: Uuid) -> Result<usize, SqliteError> {
    let id_str = id.to_string();
    let len: Option<i64> = conn
        .query_row(
            "SELECT COALESCE(LENGTH(CAST(name AS BLOB)), 0) \
                    + LENGTH(CAST(content AS BLOB)) \
                    + COALESCE(LENGTH(CAST(properties AS BLOB)), 0) \
             FROM notes WHERE id = ?1 AND deleted_at IS NULL",
            rusqlite::params![id_str],
            |row| row.get(0),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?;
    Ok(128_usize.saturating_add(len.unwrap_or(0).max(0) as usize))
}

/// Read one note row by ID within a namespace, returning `SqliteError` on missing/wrong-ns.
fn read_merge_note(
    conn: &rusqlite::Connection,
    id: Uuid,
    namespace: &str,
) -> Result<khive_storage::note::Note, SqliteError> {
    use khive_storage::note::Note;
    let id_str = id.to_string();
    let mut stmt = conn.prepare(
        "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
         expires_at, properties, created_at, updated_at, deleted_at, key, version \
         FROM notes WHERE id = ?1 AND deleted_at IS NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![id_str])?;
    let row = rows
        .next()?
        .ok_or_else(|| SqliteError::InvalidData(format!("note {id} not found")))?;

    let id_s: String = row.get(0)?;
    let ns: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let status: String = row.get(3)?;
    let name: Option<String> = row.get(4)?;
    let content: String = row.get(5)?;
    let salience: Option<f64> = row.get(6)?;
    let decay_factor: Option<f64> = row.get(7)?;
    let expires_at: Option<i64> = row.get(8)?;
    let properties_str: Option<String> = row.get(9)?;
    let created_at: i64 = row.get(10)?;
    let updated_at: i64 = row.get(11)?;
    let deleted_at: Option<i64> = row.get(12)?;
    let key: Option<String> = row.get(13)?;
    let version: i64 = row.get(14)?;

    if ns != namespace {
        return Err(SqliteError::InvalidData(format!(
            "note {id} belongs to namespace '{ns}', not '{namespace}'"
        )));
    }

    let note_id = Uuid::parse_str(&id_s).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let properties: Option<serde_json::Value> = properties_str
        .map(|s| serde_json::from_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string())))
        .transpose()?;

    Ok(Note {
        id: note_id,
        namespace: ns,
        kind,
        status,
        name,
        content,
        salience,
        decay_factor,
        expires_at,
        properties,
        created_at,
        updated_at,
        deleted_at,
        key,
        version,
    })
}

fn max_option_f64(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn append_merge_history(props: Option<Value>, entry: Value) -> Result<Option<Value>, SqliteError> {
    use serde_json::{json, Map};
    let mut obj: Map<String, Value> = match props {
        Some(Value::Object(m)) => m,
        Some(other) => {
            let mut m = Map::new();
            m.insert("_value".into(), other);
            m
        }
        None => Map::new(),
    };
    let history = obj
        .entry("_merge_history".to_string())
        .or_insert_with(|| json!([]));
    if let Value::Array(arr) = history {
        arr.push(entry);
    }
    Ok(Some(Value::Object(obj)))
}

/// All note merge SQL on one connection inside a `BEGIN IMMEDIATE` transaction.
///
/// Reads both notes (must have same `kind`), rewires/drops incident edges, merges content
/// per `content_strategy`, tombstones `from`. Returns the updated `into` note for async
/// re-embedding.
///
/// When `dry_run` is true, all reads and computations are performed but no writes are issued.
// REASON: note merge additionally requires a content_strategy parameter versus entity merge;
// same sync/async boundary rationale as merge_entity_sql applies here.
#[allow(clippy::too_many_arguments)]
pub(super) fn merge_note_sql(
    conn: &rusqlite::Connection,
    namespace: String,
    fts_table: String,
    vec_tables: Vec<String>,
    into_id: Uuid,
    from_id: Uuid,
    strategy: EntityDedupMergePolicy,
    content_strategy: ContentMergeStrategy,
    dry_run: bool,
    pack_rules: Vec<EdgeEndpointRule>,
    preserve_owner_established: bool,
    limits: MergeTxLimits,
    event_context: Option<MergeEventContext>,
    guard: Option<NoteMergeGuard>,
) -> Result<(MergeSummary, khive_storage::note::Note), MergeSqlError> {
    let mut budget = MergeTxBudget::new(limits);
    // Same accounting as `merge_entity_sql`: config-scaled fanout in bytes only.
    budget.charge(
        0,
        vec_tables.iter().map(String::len).sum::<usize>()
            + pack_rules.len() * std::mem::size_of::<EdgeEndpointRule>(),
        "preparing pack and vector fanout",
    )?;

    budget.charge(
        1,
        probe_merge_note_bytes(conn, into_id)?,
        "reading merge records",
    )?;
    let into_note = read_merge_note(conn, into_id, &namespace)?;
    budget.charge(
        1,
        probe_merge_note_bytes(conn, from_id)?,
        "reading merge records",
    )?;
    let from_note = read_merge_note(conn, from_id, &namespace)?;
    if let Some(guard) = guard.as_ref() {
        guard.enforce(conn, &namespace, &into_note, &from_note, &mut budget)?;
    }

    // Preimages are read in the same guarded unit as the eventual mutation.
    // Checking only the fold would allow a stamp to be discarded by a merge.
    for properties in [&into_note.properties, &from_note.properties] {
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())
            .map_err(MergeSqlError::Refusal)?;
    }

    if into_note.kind != from_note.kind {
        return Err(SqliteError::InvalidData(format!(
            "cannot merge notes of different kinds: {} vs {}",
            into_note.kind, from_note.kind
        ))
        .into());
    }

    // A quarantined message participates in no merges, in either role. Folding
    // its content into an ordinary message would retain the body while the
    // marker restoration below drops the `quarantined` disposition — laundering
    // quarantined transport content into an unmarked record. Release is the
    // channel-ingest path's decision, never a side effect of curation.
    if into_note.kind == "message"
        && (message_is_quarantined(&into_note) || message_is_quarantined(&from_note))
    {
        return Err(SqliteError::InvalidData(
            "cannot merge a quarantined message: quarantine disposition is              transport-owned and must be released by the channel-ingest path              before the content can be folded into another record"
                .to_string(),
        ).into());
    }

    let now = chrono::Utc::now().timestamp_micros();
    let into_str = into_id.to_string();
    let from_str = from_id.to_string();

    // Collect edges incident to from_id.
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));

    // Namespace-agnostic (khive#1236): see the equivalent comment in
    // `merge_entity_sql` — edge endpoints resolve by-ID regardless of namespace.
    let mut outbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE source_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            outbound.push(edge);
        }
    }
    let mut inbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE target_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            inbound.push(edge);
        }
    }
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut all_edges: Vec<EdgeRow> = Vec::new();
    for edge in outbound.into_iter().chain(inbound) {
        if seen.insert(edge.id) {
            all_edges.push(edge);
        }
    }
    let original_edges: HashMap<Uuid, EdgeRow> = all_edges
        .iter()
        .map(|edge| (edge.id, edge.clone()))
        .collect();

    // Merge note fields.
    let (merged_content, content_appended) = match content_strategy {
        ContentMergeStrategy::Append => {
            if from_note.content.is_empty() {
                (into_note.content.clone(), false)
            } else {
                (
                    format!("{}\n\n---\n\n{}", into_note.content, from_note.content),
                    true,
                )
            }
        }
        ContentMergeStrategy::PreferInto => (into_note.content.clone(), false),
        ContentMergeStrategy::PreferFrom => (from_note.content.clone(), false),
    };

    let merged_name = match strategy {
        EntityDedupMergePolicy::PreferFrom => from_note.name.clone().or(into_note.name.clone()),
        _ => into_note.name.clone().or(from_note.name.clone()),
    };

    let (mut merged_props, _) =
        merge_properties(&into_note.properties, &from_note.properties, strategy);

    // A merge folds two records together; it does not transfer attribution.
    // On a pack-owned note kind the into-note's owned identity properties are
    // restored after the fold, under every strategy including `PreferFrom`, so
    // the surviving row still says who wrote it.
    if preserve_owner_established {
        preserve_owner_established_properties(&into_note.properties, &mut merged_props);
    }
    preserve_property_keys(
        kind_owned_properties(&into_note.kind),
        &into_note.properties,
        &mut merged_props,
    );

    // Recomputed from the final retained properties rather than carried
    // forward from the fold's own count. The fold's count and post-
    // restoration reality diverge whenever an owner-established key holds a
    // nested object: `union` recurses into it and counts the absorbed
    // note's leaf as merged, but restoration then reverts the whole key,
    // and the fold's flat "keys contributed" number cannot express a
    // partial reversal of a nested contribution. Diffing the final object
    // against the into-note's pre-merge properties sidesteps that fold/
    // restoration coupling entirely.
    let properties_merged = count_new_property_keys(
        into_note.properties.as_ref(),
        merged_props.as_ref(),
        strategy,
    );

    let mut merge_history_entry = serde_json::json!({
        "merged_from": from_id.to_string(),
        "merged_at": now,
        "strategy": format!("{:?}", strategy),
        "content_strategy": format!("{:?}", content_strategy),
    });
    if let Some(guard) = guard.as_ref() {
        guard.apply_to_survivor(
            &into_note.kind,
            preserve_owner_established,
            &mut merged_props,
            &mut merge_history_entry,
        )?;
    }
    let merged_props = append_merge_history(merged_props, merge_history_entry)?;
    crate::secret_gate::reject_reserved_secret_gate_property(merged_props.as_ref())
        .map_err(MergeSqlError::Refusal)?;

    let merged_salience = max_option_f64(into_note.salience, from_note.salience);
    let merged_expires_at = match (into_note.expires_at, from_note.expires_at) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let props_str = merged_props
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let (due_key, due_source) = khive_db::stores::note::note_due_key_values(&merged_props);

    // The loop always runs so a dry-run reports a predictive `edges_rewired`
    // count instead of zero (mirrors the entity merge path).
    let mut rewired_edge_ids = HashSet::new();
    let mut edges_contract_skipped = 0usize;
    let mut edge_conflict_preimages = Vec::new();
    let mut edges_self_loop_dropped = 0usize;
    let mut self_loop_edge_preimages = Vec::new();
    let mut self_loop_incident_edge_preimages = Vec::new();
    let mut contract_drop_edge_preimages = Vec::new();
    let mut contract_drop_incident_edge_preimages = Vec::new();
    let mut planned_deleted_edge_ids = HashSet::new();
    {
        for edge in all_edges {
            if planned_deleted_edge_ids.contains(&edge.id) {
                continue;
            }
            let raw_src = if edge.source_id == from_id {
                into_id
            } else {
                edge.source_id
            };
            let raw_tgt = if edge.target_id == from_id {
                into_id
            } else {
                edge.target_id
            };
            let relation_typed = edge.relation.parse::<EdgeRelation>().ok();
            // Canonicalize symmetric relations before conflict check + UPDATE.
            let (new_src, new_tgt) = match relation_typed {
                Some(rel) => canonical_edge_endpoints(rel, raw_src, raw_tgt),
                None => (raw_src, raw_tgt),
            };
            if new_src == new_tgt {
                let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                    conn,
                    edge.id,
                    &original_edges,
                    &planned_deleted_edge_ids,
                    &mut budget,
                    "collecting self-loop cascade rows",
                )?;
                for incident in &incident_edge_preimages {
                    planned_deleted_edge_ids.insert(incident.id);
                    rewired_edge_ids.remove(&incident.id);
                }
                planned_deleted_edge_ids.insert(edge.id);
                self_loop_edge_preimages.push(edge_row_preimage(&edge)?);
                self_loop_incident_edge_preimages.extend(incident_edge_preimages.iter().cloned());
                edges_self_loop_dropped += 1;
                if !dry_run {
                    delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
                }
                continue;
            }

            // Endpoint-contract check (khive#1216/#1236): see the equivalent
            // block in `merge_entity_sql` for the full rationale. Here the
            // rewiring endpoint is a note (`into_id`'s kind, substrate "note"),
            // not an entity.
            let contract_ok = match relation_typed {
                // Same rationale as the entity-merge path: annotates targets may
                // be events or edges, unresolvable by substrate lookup — the
                // exemption must precede endpoint resolution.
                Some(EdgeRelation::Annotates) => true,
                Some(rel) => {
                    let src_info = if new_src == into_id {
                        Some(("note", into_note.kind.clone(), None))
                    } else {
                        resolve_merge_edge_endpoint_budgeted(conn, new_src, &mut budget)?
                    };
                    let tgt_info = if new_tgt == into_id {
                        Some(("note", into_note.kind.clone(), None))
                    } else {
                        resolve_merge_edge_endpoint_budgeted(conn, new_tgt, &mut budget)?
                    };
                    match (src_info, tgt_info) {
                        (
                            Some((src_sub, src_kind, src_type)),
                            Some((tgt_sub, tgt_kind, tgt_type)),
                        ) => merge_rewire_endpoint_contract_allows(
                            &pack_rules,
                            rel,
                            src_sub,
                            &src_kind,
                            src_type.as_deref(),
                            tgt_sub,
                            &tgt_kind,
                            tgt_type.as_deref(),
                        ),
                        _ => false,
                    }
                }
                None => true,
            };
            if !contract_ok {
                let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                    conn,
                    edge.id,
                    &original_edges,
                    &planned_deleted_edge_ids,
                    &mut budget,
                    "collecting contract-drop cascade rows",
                )?;
                for incident in &incident_edge_preimages {
                    planned_deleted_edge_ids.insert(incident.id);
                    rewired_edge_ids.remove(&incident.id);
                }
                planned_deleted_edge_ids.insert(edge.id);
                contract_drop_edge_preimages.push(edge_row_preimage(&edge)?);
                contract_drop_incident_edge_preimages
                    .extend(incident_edge_preimages.iter().cloned());
                if !dry_run {
                    delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
                }
                tracing::warn!(
                    edge_id = %edge.id,
                    source = %new_src,
                    target = %new_tgt,
                    relation = %edge.relation,
                    "merge_note: dropping rewired edge — endpoint contract violation post-merge"
                );
                edges_contract_skipped += 1;
                continue;
            }

            let now_ts = chrono::Utc::now().timestamp_micros();
            let conflict_id: Option<String> = {
                let conflict_src = new_src.to_string();
                let conflict_tgt = new_tgt.to_string();
                conn.query_row(
                    khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL,
                    rusqlite::params![
                        &edge.namespace,
                        &conflict_src,
                        &conflict_tgt,
                        &edge.relation,
                        edge.id.to_string(),
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(SqliteError::Rusqlite)?
            };

            if let Some(conflict_id) = conflict_id {
                // A live or soft-deleted row already owns this natural key: drop
                // the incoming duplicate (ADR-039 `ON CONFLICT ... DO NOTHING`).
                // The surviving row's weight/metadata/deleted_at are never
                // mutated or resurrected. Match hard `delete_edge`: cascade
                // incident annotations, and preserve every removed row first.
                let surviving_edge_id = Uuid::parse_str(&conflict_id)
                    .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
                let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                    conn,
                    edge.id,
                    &original_edges,
                    &planned_deleted_edge_ids,
                    &mut budget,
                    "collecting conflict cascade rows",
                )?;
                for incident in &incident_edge_preimages {
                    planned_deleted_edge_ids.insert(incident.id);
                    rewired_edge_ids.remove(&incident.id);
                }
                planned_deleted_edge_ids.insert(edge.id);
                rewired_edge_ids.insert(edge.id);

                if !dry_run {
                    delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
                }
                edge_conflict_preimages.push(MergeEdgeConflictPreimage {
                    surviving_edge_id,
                    dropped_edge: edge_row_preimage(&edge)?,
                    incident_edge_preimages,
                });
            } else {
                if dry_run {
                    rewired_edge_ids.insert(edge.id);
                    continue;
                }
                let changed = conn.execute(
                    "UPDATE graph_edges SET \
                     source_id = ?1, target_id = ?2, updated_at = ?3 \
                     WHERE namespace = ?4 AND id = ?5",
                    rusqlite::params![
                        new_src.to_string(),
                        new_tgt.to_string(),
                        now_ts,
                        &edge.namespace,
                        edge.id.to_string(),
                    ],
                )?;
                if changed > 0 {
                    rewired_edge_ids.insert(edge.id);
                }
            }
        }
    }
    let edges_rewired = rewired_edge_ids.len();

    if !dry_run {
        conn.prepare_cached(khive_db::stores::note::NOTE_UPSERT_SQL)?
            .execute(rusqlite::params![
                &into_str,
                &namespace,
                &into_note.kind,
                &into_note.status,
                &merged_name,
                &merged_content,
                merged_salience,
                into_note.decay_factor,
                merged_expires_at,
                &props_str,
                into_note.created_at,
                now,
                into_note.deleted_at,
                &into_note.key,
                &due_key,
                &due_source,
            ])?;

        let fts_map = khive_db::stores::text::rowid_map_table(&fts_table);

        // `into`'s old FTS row (via the map), then the new merged row, then
        // the map upsert to the new rowid — see `merge_entity_sql`'s matching
        // comment for why no separate map-row delete is needed here.
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2) \
                 AND namespace = ?1 AND subject_id = ?2"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;
        // Derive FTS scalars through the shared constructor so this raw SQL path
        // is field-identical to TextSearch::upsert_document: critically, `title`
        // is an empty string (not SQL NULL) for nameless notes, so get_document
        // round-trips None <-> "" correctly.
        let fts_merged = {
            let mut merged_note = Note::new(&namespace, &*into_note.kind, &*merged_content);
            merged_note.id = into_id;
            merged_note.name = merged_name.clone();
            merged_note.properties = merged_props.clone();
            merged_note.updated_at = now;
            note_fts_scalars(&merged_note)
        };
        conn.execute(
            &format!(
                "INSERT INTO {} \
                (subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                fts_table
            ),
            rusqlite::params![
                &into_str,
                SubstrateKind::Note.to_string(),
                &fts_merged.title,
                &fts_merged.body,
                &fts_merged.tags,
                &namespace,
                &fts_merged.metadata,
                fts_merged.updated_at_micros,
                &fts_merged.record_kind,
            ],
        )?;
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO {fts_map} (namespace, subject_id, rowid) \
                 VALUES (?1, ?2, last_insert_rowid())"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;

        // `from`'s FTS row is gone for good — remove its map row too.
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2) \
                 AND namespace = ?1 AND subject_id = ?2"
            ),
            rusqlite::params![&namespace, &from_str],
        )?;
        conn.execute(
            &format!("DELETE FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2"),
            rusqlite::params![&namespace, &from_str],
        )?;

        khive_db::stores::vectors::delete_subject_from_vector_tables(
            conn,
            &vec_tables,
            from_id,
            &namespace,
        )?;

        conn.execute(
            "UPDATE notes SET status = 'deleted', deleted_at = ?1, updated_at = ?1 \
             WHERE namespace = ?2 AND id = ?3 AND deleted_at IS NULL",
            rusqlite::params![now, &namespace, &from_str],
        )?;
    }

    let updated_note = khive_storage::note::Note {
        id: into_id,
        namespace: namespace.clone(),
        kind: into_note.kind.clone(),
        status: into_note.status.clone(),
        name: merged_name,
        content: merged_content,
        salience: merged_salience,
        decay_factor: into_note.decay_factor,
        expires_at: merged_expires_at,
        properties: merged_props,
        created_at: into_note.created_at,
        updated_at: now,
        deleted_at: into_note.deleted_at,
        key: into_note.key.clone(),
        version: conn.query_row(
            "SELECT version FROM notes WHERE id = ?1",
            [&into_str],
            |row| row.get(0),
        )?,
    };

    let summary = MergeSummary {
        kept_id: into_id,
        removed_id: from_id,
        edges_rewired,
        edges_self_loop_dropped,
        self_loop_edge_preimages,
        self_loop_incident_edge_preimages,
        edges_contract_skipped,
        contract_drop_edge_preimages,
        contract_drop_incident_edge_preimages,
        edge_conflict_preimages,
        properties_merged,
        tags_unioned: 0,
        content_appended,
        dry_run,
        tx_budget: budget.report(),
        embedding_truncation: Default::default(),
        post_commit_reindex_error: None,
    };
    if !dry_run {
        if let Some(context) = event_context {
            append_merge_event_in_transaction(conn, context, &summary, &updated_note.namespace)?;
        }
    }
    Ok((summary, updated_note))
}
