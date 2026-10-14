//! Domain loading, compose sizing, and knowledge-graph candidate blending.

#[cfg(test)]
use super::{atom_from_row, Atom};
use super::{
    domain_from_row, embed_cosine_scores, estimate_compose_item_tokens, hex_prefix_to_uuid_pattern,
    is_timeout, row_i64, row_str, sql_err, Domain, EntityFilter, HashMap, HashSet, KhiveRuntime,
    NamespaceToken, PageRequest, QueryEmbeddingCache, RuntimeError, SqlStatement, SqlValue, Uuid,
    SERVABLE_SECTION,
};

// ─── compose helpers ──────────────────────────────────────────────────────────

pub(super) struct ScoredTextItem {
    pub(super) id: String,
    pub(super) slug: String,
    pub(super) name: String,
    pub(super) text: String,
    pub(super) score: f32,
}

fn compare_scored_text_items(a: &ScoredTextItem, b: &ScoredTextItem) -> std::cmp::Ordering {
    khive_score::cmp_desc_then_id_f32(a.score, &a.slug, b.score, &b.slug)
}

pub(super) async fn load_domain_by_id_or_slug(
    runtime: &KhiveRuntime,
    ns: &str,
    id_or_slug: &str,
) -> Result<Domain, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("compose domain reader", e))?;
    let id = id_or_slug.trim().to_string();
    let row = if id.parse::<Uuid>().is_ok() {
        reader
            .query_row(SqlStatement {
                sql: khive_runtime::sql!("knowledge_compose_domain_id").into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose domain by id", e))?
    } else {
        let by_slug = reader
            .query_row(SqlStatement {
                sql: khive_runtime::sql!("knowledge_compose_domain_slug").into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose domain by slug", e))?;
        if by_slug.is_some() {
            by_slug
        } else {
            let is_hex = id.len() >= 8
                && id.len() <= 36
                && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
            if is_hex {
                let pattern = format!("{}%", hex_prefix_to_uuid_pattern(&id));
                let rows = reader
                    .query_all(SqlStatement {
                        sql: khive_runtime::sql!("knowledge_compose_domain_prefix").into(),
                        params: vec![SqlValue::Text(pattern), SqlValue::Text(ns.to_owned())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("compose domain by prefix", e))?;
                if rows.len() > 1 {
                    return Err(RuntimeError::InvalidInput(format!(
                        "ambiguous domain prefix {id:?} matches multiple domains"
                    )));
                }
                rows.into_iter().next()
            } else {
                None
            }
        }
    };
    row.and_then(|r| domain_from_row(&r))
        .ok_or_else(|| RuntimeError::NotFound(format!("domain not found: {id:?}")))
}

#[cfg(test)]
pub(super) async fn load_atom_by_id_or_slug(
    runtime: &KhiveRuntime,
    ns: &str,
    id_or_slug: &str,
) -> Result<Atom, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("compose atom reader", e))?;
    let id = id_or_slug.trim().to_string();
    let row = if id.parse::<Uuid>().is_ok() {
        reader
            .query_row(SqlStatement {
                sql: khive_runtime::sql!("knowledge_compose_atom_id").into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose atom by id", e))?
    } else {
        let by_slug = reader
            .query_row(SqlStatement {
                sql: khive_runtime::sql!("knowledge_compose_atom_slug").into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose atom by slug", e))?;
        if by_slug.is_some() {
            by_slug
        } else {
            let is_hex = id.len() >= 8
                && id.len() <= 36
                && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
            if is_hex {
                let pattern = format!("{}%", hex_prefix_to_uuid_pattern(&id));
                let rows = reader
                    .query_all(SqlStatement {
                        sql: khive_runtime::sql!("knowledge_compose_atom_prefix").into(),
                        params: vec![SqlValue::Text(pattern), SqlValue::Text(ns.to_owned())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("compose atom by prefix", e))?;
                if rows.len() > 1 {
                    return Err(RuntimeError::InvalidInput(format!(
                        "ambiguous atom prefix {id:?} matches multiple atoms"
                    )));
                }
                rows.into_iter().next()
            } else {
                None
            }
        }
    };
    row.and_then(|r| atom_from_row(&r))
        .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {id:?}")))
}

pub(super) fn parse_domain_members(domain: &Domain) -> Result<Vec<String>, RuntimeError> {
    if domain.members.is_empty() || domain.members == "[]" {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<String>>(&domain.members).map_err(|e| {
        RuntimeError::Internal(format!(
            "domain {:?} has invalid members JSON: {e}",
            domain.slug
        ))
    })
}

#[derive(Debug, Default)]
pub(super) struct DomainMemberSizing {
    pub(super) tokens: usize,
    pub(super) live_members: usize,
}

/// Member-token sizing is best-effort: a request read-deadline timeout on
/// reader checkout or the query returns an empty map and `true`, not `Err`.
/// One `query_all` has no partial-completion state, so the flag covers the batch.
/// Every live canonical domain enters the map, with zero size and members
/// when no live member joins. Only absent domains stay unmeasured.
pub(super) async fn load_domain_member_token_sizes(
    runtime: &KhiveRuntime,
    ns: &str,
    domain_ids: &[String],
) -> Result<(HashMap<String, DomainMemberSizing>, bool), RuntimeError> {
    let mut sizes: HashMap<String, DomainMemberSizing> = HashMap::new();
    if domain_ids.is_empty() {
        return Ok((sizes, false));
    }

    let placeholders = domain_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(domain_ids.iter().cloned().map(SqlValue::Text));

    let sql = runtime.sql();
    let mut reader = match sql.reader().await {
        Ok(reader) => reader,
        Err(e) if is_timeout(&e) => return Ok((sizes, true)),
        Err(e) => return Err(sql_err("suggest member size reader", e)),
    };
    let rows = match reader
        .query_all(SqlStatement {
            // Keep atom identity in DISTINCT: repeated members count once, while
            // different atoms with identical names and content still count separately.
            sql: format!(
                "SELECT DISTINCT d.id AS domain_id, a.id AS atom_id, a.name, a.content \
                 FROM knowledge_domains AS d \
                 LEFT JOIN json_each(d.members) AS member ON 1 = 1 \
                 LEFT JOIN knowledge_atoms AS a \
                   ON a.namespace = d.namespace \
                  AND a.slug = member.value \
                  AND a.deleted_at IS NULL \
                 WHERE d.namespace = ?1 \
                   AND d.id IN ({placeholders}) \
                   AND d.deleted_at IS NULL"
            ),
            params,
            label: None,
        })
        .await
    {
        Ok(rows) => rows,
        Err(e) if is_timeout(&e) => return Ok((sizes, true)),
        Err(e) => return Err(sql_err("suggest member size query", e)),
    };

    for row in rows {
        let Some(domain_id) = row_str(&row, "domain_id") else {
            continue;
        };
        let sizing = sizes.entry(domain_id).or_default();
        let Some(content) = row_str(&row, "content") else {
            continue;
        };
        let name = row_str(&row, "name").unwrap_or_default();
        sizing.tokens = sizing
            .tokens
            .saturating_add(estimate_compose_item_tokens(&name, &content));
        sizing.live_members = sizing.live_members.saturating_add(1);
    }

    Ok((sizes, false))
}

/// Body-line metadata is best-effort: a request read-deadline timeout on
/// either the reader checkout or the aggregate query returns `Ok(None)` —
/// the already-ranked hits report `body_lines: null` with a degradation
/// flag instead of the whole search failing. Non-timeout storage errors
/// still propagate.
///
/// The line count follows `str::lines()` semantics: a terminal newline does
/// not add a line, blank interior lines count, and empty content is 0.
pub(super) async fn load_atom_body_line_counts(
    runtime: &KhiveRuntime,
    ns: &str,
    atom_ids: &[String],
) -> Result<Option<HashMap<String, usize>>, RuntimeError> {
    let mut counts: HashMap<String, usize> = atom_ids.iter().map(|id| (id.clone(), 0)).collect();
    if atom_ids.is_empty() {
        return Ok(Some(counts));
    }

    let placeholders = atom_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(atom_ids.iter().cloned().map(SqlValue::Text));

    let sql = runtime.sql();
    let mut reader = match sql.reader().await {
        Ok(reader) => reader,
        Err(e) if is_timeout(&e) => return Ok(None),
        Err(e) => return Err(sql_err("search body line count reader", e)),
    };
    let rows = match reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT atom_id, \
                        SUM(CASE WHEN content = '' THEN 0 \
                                 ELSE length(content) \
                                      - length(replace(content, char(10), '')) \
                                      + (CASE WHEN substr(content, -1) = char(10) \
                                              THEN 0 ELSE 1 END) \
                            END) AS body_lines \
                 FROM knowledge_sections \
                 WHERE namespace = ?1 AND atom_id IN ({placeholders}) AND {SERVABLE_SECTION} \
                 GROUP BY atom_id"
            ),
            params,
            label: None,
        })
        .await
    {
        Ok(rows) => rows,
        Err(e) if is_timeout(&e) => return Ok(None),
        Err(e) => return Err(sql_err("search body line count query", e)),
    };

    for row in rows {
        let Some(atom_id) = row_str(&row, "atom_id") else {
            continue;
        };
        let Some(body_lines) = row_i64(&row, "body_lines") else {
            continue;
        };
        if let Ok(body_lines) = usize::try_from(body_lines) {
            counts.insert(atom_id, body_lines);
        }
    }

    Ok(Some(counts))
}

pub(super) async fn rerank_text_items(
    runtime: &KhiveRuntime,
    query: &str,
    query_embedding: &mut QueryEmbeddingCache,
    items: &mut [ScoredTextItem],
) -> Result<(), RuntimeError> {
    if items.is_empty() {
        return Ok(());
    }
    let texts: Vec<String> = items.iter().map(|item| item.text.clone()).collect();
    if let Some(cosines) = embed_cosine_scores(runtime, query, query_embedding, &texts).await? {
        for (item, cos) in items.iter_mut().zip(cosines.iter()) {
            item.score = cos.max(0.0);
        }
        items.sort_by(compare_scored_text_items);
    }
    Ok(())
}

// ─── KG entity blending (ADR-051 Amendment 1) ─────────────────────────────────

/// The KG entity kinds `knowledge.compose` blends into a briefing. Concepts
/// and documents are the kinds that carry the measured, expert-curated
/// content (algorithms, papers, ADRs) that outranks generic lore atoms on
/// sharply technical queries — see ADR-051 Amendment 1.
const KG_BLEND_ENTITY_KINDS: [&str; 2] = ["concept", "document"];

/// Cap on blended KG entities per briefing, so entities stay a supplementary
/// "Knowledge graph" section and atoms remain the body of the briefing.
pub(super) const KG_BLEND_CAP: usize = 5;

/// A `concept`/`document` KG entity blended into a compose briefing.
pub(super) struct KgEntityHit {
    pub(super) id: String,
    pub(super) kind: String,
    pub(super) name: String,
    pub(super) description: String,
    pub(super) score: f32,
}

fn compare_kg_entity_hits(a: &KgEntityHit, b: &KgEntityHit) -> std::cmp::Ordering {
    khive_score::cmp_desc_then_id_f32(a.score, &a.id, b.score, &b.id)
}

/// Finds `concept`/`document` KG entities relevant to `query`, reranked with
/// the same embedding-cosine signal `rerank_text_items` uses for atom bodies
/// (embed `name + description`, cosine against the query embedding). Because
/// both pools are scored with the identical metric against the identical
/// query embedding, the resulting scores land on the same 0..1 scale as
/// atom/section scores — direct comparison, not a separate rank-fusion step,
/// is what makes them a valid blended candidate pool.
///
/// Candidate discovery itself reuses `KhiveRuntime::hybrid_search` — the same
/// FTS+ANN RRF-fused retrieval path `kg.search(kind="entity")` dispatches to
/// (`khive-pack-kg`'s `handle_search` calls the identical method) — so this
/// does not stand up a parallel retrieval stack; only the final relevance
/// score is recomputed, to land on the atom-comparable scale.
/// `hybrid_search_each_kind` hands each blend kind the list `hybrid_search`
/// returns for it while issuing the vector query once for all kinds.
///
/// `min_score` is the self-calibrating inclusion floor (ADR-051 Amendment
/// 1): only hits scoring at or above it survive, applied after rerank and
/// before the `cap` truncation. Callers derive it from the minimum rerank
/// score among the atoms that made the final compose body.
pub(super) async fn search_kg_entities(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    ns: &str,
    query: &str,
    query_embedding: &mut QueryEmbeddingCache,
    cap: usize,
    min_score: f32,
) -> Result<Vec<KgEntityHit>, RuntimeError> {
    // KG discovery has two kind-specific hybrid searches that share one vector query.
    // Both are gated on `role_specific` specifically, never `generic`: a vector produced by
    // the rerank's combined batch (`embed_batch`) lands in a different
    // embedding space than `embed_query` for asymmetric-prompt models, so it
    // must never stand in for a role-specific vector here — doing so would
    // mask a real `embed_query` failure behind a same-shaped wrong-space
    // vector instead of degrading to the atom-only briefing (#2307). No
    // role-specific vector (never attempted, or attempted and failed) means
    // the dense blend is unavailable and the caller safely keeps its
    // already-complete atom-only briefing.
    let Some(query_vector) = query_embedding.role_specific.as_deref() else {
        return Ok(Vec::new());
    };
    let candidate_k = ((cap * 4) as u32).max(20);
    let mut candidate_ids: Vec<Uuid> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    let hits_by_kind = runtime
        .hybrid_search_each_kind(
            token,
            query,
            Some(query_vector.to_vec()),
            candidate_k,
            &KG_BLEND_ENTITY_KINDS,
        )
        .await?;
    for hit in hits_by_kind.into_iter().flatten() {
        if seen.insert(hit.entity_id) {
            candidate_ids.push(hit.entity_id);
        }
    }
    if candidate_ids.is_empty() {
        return Ok(Vec::new());
    }

    let visible_ns: Vec<String> = token
        .visible_namespace_strs()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let entities_page = runtime
        .entities(token)?
        .query_entities(
            ns,
            EntityFilter {
                ids: candidate_ids.clone(),
                kinds: KG_BLEND_ENTITY_KINDS
                    .iter()
                    .map(|k| k.to_string())
                    .collect(),
                namespaces: visible_ns,
                ..EntityFilter::default()
            },
            PageRequest {
                offset: 0,
                limit: candidate_ids.len() as u32,
            },
        )
        .await?;

    if entities_page.items.is_empty() {
        return Ok(Vec::new());
    }

    let texts: Vec<String> = entities_page
        .items
        .iter()
        .map(|e| format!("{} {}", e.name, e.description.as_deref().unwrap_or("")))
        .collect();
    let cosines = match embed_cosine_scores(runtime, query, query_embedding, &texts).await? {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };

    let mut hits: Vec<KgEntityHit> = entities_page
        .items
        .iter()
        .zip(cosines.iter())
        .map(|(e, &score)| KgEntityHit {
            id: e.id.to_string(),
            kind: e.kind.clone(),
            name: e.name.clone(),
            description: e.description.clone().unwrap_or_default(),
            score: score.max(0.0),
        })
        .collect();

    hits.sort_by(compare_kg_entity_hits);
    // Self-calibrating inclusion floor (ADR-051 Amendment 1): an entity only
    // blends in if it outranks the weakest atom that made the final body —
    // no fixed score constant. Applied after rerank, before the cap, so a
    // query with many strong entities doesn't starve out a marginal one that
    // still clears the floor.
    hits.retain(|h| h.score >= min_score);
    hits.truncate(cap);
    Ok(hits)
}

#[cfg(test)]
#[path = "compose_ordering_tests.rs"]
mod ordering_tests;
