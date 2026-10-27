//! Canonical hit hydration and coordinated candidate acquisition.

use super::{
    atom_embed_text_fields, filter_hits_by_status, filter_hits_by_type, has_domain_mirror_tag,
    is_read_timeout, is_timeout, json, row_bool, row_str, search_ann_with_warm_wait,
    status_sql_clause, vamana, AnnAvailability, AnnSearchState, HashMap, HashSet, KhiveRuntime,
    LexicalBound, LexicalTimeout, NamespaceToken, QueryEmbeddingCache, RoleSpecificEmbedding,
    RuntimeError, ScoreProvenance, ScoredHit, SearchCoreOutcome, SearchCtx, SqlStatement, SqlValue,
    Value, HYDRATION_ID_CHUNK,
};

/// Operator-only accounting for one public request, including repeated ANN prefixes.
pub(super) struct StaleAnnDiagnostics {
    request_class: &'static str,
    dropped_ids: HashSet<String>,
}

impl StaleAnnDiagnostics {
    pub(super) fn new(request_class: &'static str) -> Self {
        Self {
            request_class,
            dropped_ids: HashSet::new(),
        }
    }
}

impl Drop for StaleAnnDiagnostics {
    fn drop(&mut self) {
        tracing::info!(
            target: "khive_pack_knowledge::knowledge::search",
            request_class = self.request_class,
            stale_ann_dropped = self.dropped_ids.len() as u64,
            "stale ANN candidates dropped"
        );
    }
}

// ─── hit hydration ────────────────────────────────────────────────────────────

/// Build the atom hydration statement for one id chunk.
///
/// The `IN (...)` list runs off `knowledge_atoms`' primary key, so the
/// namespace predicate is deliberately kept out of index selection with
/// SQLite's unary `+` (`+namespace`): a scratch or freshly vacuumed store has
/// no `sqlite_stat1`, and without statistics the planner's default guess for
/// an indexed namespace equality (~10 rows) beats a 250+ id primary-key
/// probe, so it picks `idx_knowledge_atoms_ns_created` and walks the whole
/// namespace once per chunk instead of doing 250 point lookups. `+namespace`
/// removes the column from consideration as an index term so the primary key
/// wins regardless of table size or missing statistics. Do not remove it.
pub(super) fn hydrate_atoms_statement(ns: &str, ids: &[String]) -> SqlStatement {
    let placeholders = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(ids.iter().cloned().map(SqlValue::Text));
    SqlStatement {
        sql: format!(
            "SELECT id, slug, name, content, tags, finalized, status FROM knowledge_atoms \
             WHERE id IN ({placeholders}) AND +namespace = ?1 AND deleted_at IS NULL"
        ),
        params,
        label: None,
    }
}

/// Build the domain hydration statement for one id chunk.
///
/// Same primary-key-first shape as [`hydrate_atoms_statement`], for the same
/// reason: `knowledge_domains` also carries a namespace index
/// (`idx_knowledge_domains_ns`) that the no-statistics planner would
/// otherwise prefer over the primary key on a large id list.
pub(super) fn hydrate_domains_statement(ns: &str, ids: &[String]) -> SqlStatement {
    let placeholders = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(ids.iter().cloned().map(SqlValue::Text));
    SqlStatement {
        sql: format!(
            "SELECT id, slug, name, description, tags, status FROM knowledge_domains \
             WHERE id IN ({placeholders}) AND +namespace = ?1 AND deleted_at IS NULL"
        ),
        params,
        label: None,
    }
}

/// Presence only: no foreign content is read or admitted into caller-visible hits.
fn candidate_presence_statement(ids: &[String]) -> SqlStatement {
    let placeholders = ids
        .iter()
        .enumerate()
        .map(|(index, _)| format!("?{}", index + 1))
        .collect::<Vec<_>>()
        .join(",");
    SqlStatement {
        sql: format!(
            "SELECT id FROM knowledge_atoms WHERE id IN ({placeholders}) \
             UNION SELECT id FROM knowledge_domains WHERE id IN ({placeholders})"
        ),
        // Both UNION arms reuse these numbered binds, preserving the chunk ceiling.
        params: ids.iter().cloned().map(SqlValue::Text).collect(),
        label: None,
    }
}

/// Build the phase-B hydration statement for one chunk of phase-A rowids
/// (issue #2396 fix 5).
///
/// `+a.namespace = ?1` (not a bare equality) is deliberate — same reason and
/// shape as [`hydrate_atoms_statement`]/[`hydrate_domains_statement`]:
/// without table statistics (this codebase never runs `ANALYZE` on
/// `knowledge_atoms`), SQLite's planner prefers `idx_knowledge_atoms_ns` over
/// the integer primary key for a large `rowid IN (...)` list, turning an
/// O(chunk) rowid seek into an O(matching-namespace-rows) index scan per
/// chunk. Verified with `EXPLAIN QUERY PLAN` against a freshly loaded,
/// unanalyzed `knowledge_atoms` table at a 900-row chunk (this statement's
/// own `HYDRATION_ID_CHUNK`): without the unary plus the planner chose
/// `SEARCH a USING INDEX idx_knowledge_atoms_ns (namespace=? AND rowid=?)`;
/// with it, `SEARCH a USING INTEGER PRIMARY KEY (rowid=?)`. The unary plus
/// defeats the index without changing the predicate's meaning.
pub(super) fn phase_b_hydration_statement(
    ns: &str,
    rowids: &[i64],
    statuses: &[String],
    exclude_statuses: &[&str],
    type_clause: &str,
) -> SqlStatement {
    let rowid_placeholders = rowids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let (status_clause, status_params) =
        status_sql_clause(statuses, exclude_statuses, 2 + rowids.len());
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(rowids.iter().map(|rowid| SqlValue::Integer(*rowid)));
    params.extend(status_params);
    SqlStatement {
        sql: format!(
            "SELECT a.*, a.rowid AS rowid FROM knowledge_atoms AS a \
             WHERE a.rowid IN ({rowid_placeholders}) \
               AND +a.namespace = ?1 \
               AND a.deleted_at IS NULL{status_clause}{type_clause}"
        ),
        params,
        label: None,
    }
}

/// Hydrate ANN-only hit shells from the canonical corpus tables.
///
/// Proven-absent IDs are index misses. Existing or unreadable rows that cannot
/// hydrate remain failures. Every unresolved shell is removed.
#[cfg(test)]
pub(super) async fn hydrate_empty_hits(
    runtime: &KhiveRuntime,
    ns: &str,
    hits: &mut Vec<ScoredHit>,
) -> usize {
    hydrate_empty_hits_recording(runtime, ns, hits, &mut HashSet::new()).await
}

async fn hydrate_empty_hits_recording(
    runtime: &KhiveRuntime,
    ns: &str,
    hits: &mut Vec<ScoredHit>,
    stale_ids: &mut HashSet<String>,
) -> usize {
    let ids: Vec<String> = hits
        .iter()
        .filter(|hit| hit.slug.is_empty())
        .map(|hit| hit.id.clone())
        .collect();
    if ids.is_empty() {
        return 0;
    }

    let sql = runtime.sql();
    let mut reader = match sql.reader().await {
        Ok(r) => r,
        Err(error) => {
            tracing::warn!(
                target: "khive_pack_knowledge::knowledge::search",
                namespace = ns,
                requested = ids.len(),
                error = %error,
                "knowledge candidate hydration could not acquire a reader"
            );
            hits.retain(|hit| !hit.slug.is_empty());
            return ids.len();
        }
    };

    let mut uncertain_ids = HashSet::new();
    let mut atom_rows = Vec::new();
    for chunk in ids.chunks(HYDRATION_ID_CHUNK) {
        match reader.query_all(hydrate_atoms_statement(ns, chunk)).await {
            Ok(rows) => {
                if rows.iter().any(|row| row_str(row, "id").is_none()) {
                    uncertain_ids.extend(chunk.iter().cloned());
                }
                atom_rows.extend(rows);
            }
            Err(error) => {
                tracing::warn!(
                    target: "khive_pack_knowledge::knowledge::search",
                    namespace = ns,
                    requested = chunk.len(),
                    error = %error,
                    "knowledge atom candidate hydration chunk degraded"
                );
                uncertain_ids.extend(chunk.iter().cloned());
            }
        }
    }

    let mut atom_rows_by_id: HashMap<String, khive_storage::types::SqlRow> = HashMap::new();
    for row in atom_rows {
        if let Some(id) = row_str(&row, "id") {
            atom_rows_by_id.insert(id, row);
        }
    }

    for hit in hits.iter_mut().filter(|hit| hit.slug.is_empty()) {
        if let Some(row) = atom_rows_by_id.get(&hit.id) {
            hit.slug = row_str(row, "slug").unwrap_or_default();
            hit.name = row_str(row, "name").unwrap_or_default();
            hit.content = row_str(row, "content");
            hit.tags = row_str(row, "tags");
            hit.atom_embed_text = Some(atom_embed_text_fields(
                &hit.name,
                hit.content.as_deref().unwrap_or(""),
                hit.tags.as_deref().unwrap_or("[]"),
            ));
            hit.finalized = row_bool(row, "finalized");
            hit.status = row_str(row, "status");
            hit.is_domain = has_domain_mirror_tag(hit.tags.as_deref().unwrap_or(""));
        }
    }

    let missing_ids: Vec<String> = hits
        .iter()
        .filter(|hit| hit.slug.is_empty())
        .map(|hit| hit.id.clone())
        .collect();
    if missing_ids.is_empty() {
        return 0;
    }

    let mut domain_rows = Vec::new();
    for chunk in missing_ids.chunks(HYDRATION_ID_CHUNK) {
        match reader.query_all(hydrate_domains_statement(ns, chunk)).await {
            Ok(rows) => {
                if rows.iter().any(|row| row_str(row, "id").is_none()) {
                    uncertain_ids.extend(chunk.iter().cloned());
                }
                domain_rows.extend(rows);
            }
            Err(error) => {
                tracing::warn!(
                    target: "khive_pack_knowledge::knowledge::search",
                    namespace = ns,
                    requested = chunk.len(),
                    error = %error,
                    "knowledge domain candidate hydration chunk degraded"
                );
                uncertain_ids.extend(chunk.iter().cloned());
            }
        }
    }

    let mut domain_rows_by_id: HashMap<String, khive_storage::types::SqlRow> = HashMap::new();
    for row in domain_rows {
        if let Some(id) = row_str(&row, "id") {
            domain_rows_by_id.insert(id, row);
        }
    }

    for hit in hits.iter_mut().filter(|hit| hit.slug.is_empty()) {
        if let Some(row) = domain_rows_by_id.get(&hit.id) {
            hit.slug = row_str(row, "slug").unwrap_or_default();
            hit.name = row_str(row, "name").unwrap_or_default();
            hit.content = row_str(row, "description");
            hit.tags = row_str(row, "tags");
            hit.atom_embed_text = None;
            hit.finalized = false;
            hit.is_domain = true;
            hit.status = row_str(row, "status");
        }
    }

    let mut seen = HashSet::new();
    let unresolved: Vec<String> = hits
        .iter()
        .filter(|hit| hit.slug.is_empty() && !uncertain_ids.contains(&hit.id))
        .filter(|hit| seen.insert(hit.id.clone()))
        .map(|hit| hit.id.clone())
        .collect();
    let mut absent = HashSet::new();
    for chunk in unresolved.chunks(HYDRATION_ID_CHUNK) {
        // Failed or malformed evidence is not absence. Preserve degradation and
        // do not expose the global query or its IDs through an error payload.
        if let Ok(rows) = reader.query_all(candidate_presence_statement(chunk)).await {
            let present = rows
                .iter()
                .map(|row| row_str(row, "id"))
                .collect::<Option<HashSet<_>>>();
            let Some(present) = present else {
                continue;
            };
            let requested: HashSet<&str> = chunk.iter().map(String::as_str).collect();
            if present.iter().all(|id| requested.contains(id.as_str())) {
                absent.extend(chunk.iter().filter(|id| !present.contains(*id)).cloned());
            }
        }
    }
    stale_ids.extend(absent.iter().cloned());
    hits.retain(|hit| !hit.slug.is_empty() || !absent.contains(&hit.id));
    let failed = hits.iter().filter(|hit| hit.slug.is_empty()).count();
    hits.retain(|hit| !hit.slug.is_empty());
    if failed > 0 {
        tracing::warn!(
            target: "khive_pack_knowledge::knowledge::search",
            namespace = ns,
            requested = ids.len(),
            failed,
            "knowledge candidate hydration returned a degraded pool"
        );
    }
    failed
}

/// Add hydration degradation to a response without disturbing another
/// degradation diagnostic (for example suggest's ANN-unavailable object).
pub(super) fn attach_hydration_degradation(out: &mut Value, hydration_failures: usize) {
    if hydration_failures == 0 {
        return;
    }

    if !out
        .get("degraded")
        .is_some_and(serde_json::Value::is_object)
    {
        out["degraded"] = json!({});
    }
    out["degraded"]["hydration_failures"] = json!(hydration_failures);
}

/// Flag that the lexical/FTS candidate fetch hit the request read deadline
/// (issue #1930). Set alongside whatever ANN-backed results (if any) still
/// made it into the response — a timed-out lexical stage degrades the
/// response, it never fails the verb outright.
pub(super) fn attach_lexical_timeout_degradation(out: &mut Value, timeouts: &[LexicalTimeout]) {
    if timeouts.is_empty() {
        return;
    }
    if !out
        .get("degraded")
        .is_some_and(serde_json::Value::is_object)
    {
        out["degraded"] = json!({});
    }

    // `lexical_timeout` STAYS COARSE: any record at all sets it, exactly as
    // before. It is tempting to narrow it to "a candidate fetch was cut", and
    // issue #2879 asked for that, but it cannot be done without turning this
    // boolean into a cross-namespace disclosure channel.
    //
    // The later phases (`phase_a_rowids`, `phase_b_hydration`,
    // `eligibility_fallback`, the namespace probes) are reachable only when the
    // GLOBAL index matched, which can happen on another namespace's rows when
    // the local ones do not match at all. That is why their records are
    // operator-only and withheld from `lexical_timeout_details`. A boolean that
    // is true when one of them timed out and false when only the ordering probe
    // did would republish the very fact the detail list is censored to hide:
    // the caller would learn that a foreign row matched by observing a flag
    // appear. Measured by
    // `mixed_pass_capability_marker_does_not_reveal_foreign_matches`, which is
    // the guard that catches exactly this and did.
    //
    // So the coarse flag is load-bearing PRECISELY because it is coarse, and
    // the ordering-probe fallback is reported ADDITIVELY beside it instead.
    out["degraded"]["lexical_timeout"] = json!(true);
    out["degraded"]["lexical_timeout_instrumented"] = json!(true);

    if timeouts
        .iter()
        .any(|detail| detail.bound == LexicalBound::OrderingProbe)
    {
        // The ordering hint was skipped and the terms were queried in arrival
        // order. Safe to publish, and this is the reason it is a separate key
        // rather than a narrowing of the one above: the probe runs in
        // `term_frequency`, a PUBLIC phase whose entry does not depend on corpus
        // contents, so its presence is already disclosed in the detail list and
        // this flag reveals nothing new. A flag keyed on the later phases would
        // not have that property.
        out["degraded"]["lexical_ordering_probe_timeout"] = json!(true);
    }

    // ONE details list, with the population it always had. Splitting the
    // BOOLEANS is the fix; splitting the disclosure would have been a
    // regression, because `term_frequency` is one of only two public phases and
    // is by far the most common, so a separate list would have emptied the
    // public timing surface for the majority of timeouts. Each record now
    // carries `bound`, which is what lets a caller tell the two apart without
    // losing the timings.
    let details: Vec<_> = timeouts
        .iter()
        .filter(|detail| detail.phase.public())
        .take(3)
        .collect();
    if !details.is_empty() {
        out["degraded"]["lexical_timeout_details"] = json!(details);
    }
}

/// Flag that the best-effort body-line aggregate hit the request read
/// deadline after the search itself completed. The ranked hits are kept and
/// their atom rows report `body_lines: null`; the timeout degrades metadata,
/// it never fails the verb outright.
pub(super) fn attach_body_lines_timeout_degradation(out: &mut Value) {
    if !out
        .get("degraded")
        .is_some_and(serde_json::Value::is_object)
    {
        out["degraded"] = json!({});
    }
    out["degraded"]["body_lines_timeout"] = json!(true);
}

/// Report unmeasured domains under the stable `member_sizing_timeout` key,
/// whether sizing timed out or the canonical domain row could not be measured.
/// Every affected domain is withheld from `results` — never left in with a
/// `size` the caller cannot price — and listed here instead, as
/// `{id, name, rank, score}`, so the caller sees exactly which ranked hits
/// were excluded and why. `suggest`'s documented contract (issue #105) is
/// that `results` feeds `knowledge.fold`'s `candidates` unmodified;
/// withholding the unpriced domain keeps that passthrough valid instead of
/// turning one unmeasured domain into a hard parse error for the whole fold
/// request.
pub(super) fn attach_member_sizing_timeout_degradation(out: &mut Value, excluded: &[Value]) {
    if excluded.is_empty() {
        return;
    }

    if !out
        .get("degraded")
        .is_some_and(serde_json::Value::is_object)
    {
        out["degraded"] = json!({});
    }
    out["degraded"]["member_sizing_timeout"] = json!({
        "excluded": excluded,
        "note": "no measurement was produced (timeout or missing canonical domain), so these domains \
                 were withheld from `results` — their cost is unknown and a \
                 budgeted knowledge.fold selection cannot safely admit an unpriced \
                 item. Each entry keeps its id/name/rank/score for reference; \
                 measure size separately before folding one of these in.",
    });
}

struct EligibleAnnSearchState {
    hits: Vec<ScoredHit>,
    availability: AnnAvailability,
    hydration_failures: usize,
}

/// Retrieve an ANN pool whose bounded, rank-preserving truncation happens only
/// after canonical hydration and caller eligibility.
///
/// Vamana has no metadata predicate, so a selective status/kind filter may
/// consume the first raw top-k. Widen exponentially and re-evaluate the full
/// deterministic prefix until the eligible target is filled or the vector
/// corpus is exhausted. The common case performs one ANN search and one
/// hydration pass; only filtered/invalid prefixes pay for widening.
async fn search_eligible_ann_with_refill(
    ctx: &SearchCtx<'_>,
    token: &NamespaceToken,
    ann: &vamana::SharedAnn,
    key: &vamana::AnnKey,
    query_embedding: &[f32],
    limits: (usize, usize),
    stale: &mut StaleAnnDiagnostics,
) -> Result<EligibleAnnSearchState, RuntimeError> {
    let (target_eligible, initial_k) = limits;
    let runtime = ctx.runtime;
    let target_eligible = target_eligible.max(1);
    let mut request_k = initial_k.max(target_eligible).max(1);

    loop {
        khive_storage::ensure_request_read_active("knowledge.search")?;
        let AnnSearchState {
            hits: raw_hits,
            availability,
            source_exhausted,
        } = search_ann_with_warm_wait(runtime, token, ann, key, query_embedding, request_k).await;

        let mut seen = HashSet::with_capacity(raw_hits.len());
        let mut hits: Vec<ScoredHit> = raw_hits
            .into_iter()
            .filter(|(id, _)| seen.insert(*id))
            .map(|(id, score)| ScoredHit {
                id: id.to_string(),
                slug: String::new(),
                name: String::new(),
                content: None,
                tags: None,
                atom_embed_text: None,
                finalized: false,
                is_domain: false,
                status: None,
                score,
                provenance: ScoreProvenance::ann(),
            })
            .collect();

        let hydration_failures =
            hydrate_empty_hits_recording(runtime, ctx.ns, &mut hits, &mut stale.dropped_ids).await;
        khive_storage::ensure_request_read_active("knowledge.search")?;
        filter_hits_by_status(&mut hits, ctx.statuses, ctx.exclude_statuses);
        filter_hits_by_type(&mut hits, ctx.type_filter);

        if hits.len() >= target_eligible || source_exhausted {
            hits.truncate(target_eligible);
            return Ok(EligibleAnnSearchState {
                hits,
                availability,
                hydration_failures,
            });
        }

        // The live vector-store count is not a sound upper bound for a serving
        // bridge: a fresh delete removes the canonical vector before its tail
        // tombstone is merged into that older bridge. Widen until the ANN
        // source itself proves exhaustion.
        let next_k = request_k.saturating_mul(2);
        if next_k == request_k {
            hits.truncate(target_eligible);
            return Ok(EligibleAnnSearchState {
                hits,
                availability,
                hydration_failures,
            });
        }
        request_k = next_k;
    }
}

pub(super) struct CandidateStageOutcome {
    pub(super) ann_hits: Vec<ScoredHit>,
    pub(super) ann_availability: Option<AnnAvailability>,
    pub(super) hydration_failures: usize,
    pub(super) query_embedding: QueryEmbeddingCache,
}

pub(super) struct CandidateStageOptions {
    pub(super) query_embedding: QueryEmbeddingCache,
    pub(super) ann_target: usize,
    pub(super) ann_initial_k: usize,
    pub(super) operation: &'static str,
}

pub(super) async fn run_candidate_stages<F>(
    ctx: &SearchCtx<'_>,
    token: &NamespaceToken,
    ann: &vamana::SharedAnn,
    raw_query: &str,
    options: CandidateStageOptions,
    lexical: F,
    stale: &mut StaleAnnDiagnostics,
) -> Result<(SearchCoreOutcome, CandidateStageOutcome), RuntimeError>
where
    F: std::future::Future<Output = Result<SearchCoreOutcome, RuntimeError>>,
{
    vamana::ensure_ann_background(ctx.runtime, token, ann);
    let ann_stage = async {
        let mut query_embedding = options.query_embedding;
        let mut ann_hits = Vec::new();
        let mut ann_availability = None;
        let mut hydration_failures = 0;

        if query_embedding.role_specific.is_not_attempted() {
            query_embedding.role_specific = match khive_storage::await_request_read_phase(
                options.operation,
                ctx.runtime.embed_query(raw_query),
            )
            .await
            {
                Ok(Ok(vector)) => RoleSpecificEmbedding::Vector(vector),
                Ok(Err(_)) => RoleSpecificEmbedding::Failed,
                Err(error) if is_timeout(&error) => RoleSpecificEmbedding::Failed,
                Err(error) => return Err(error.into()),
            };
        }

        if let Some(vector) = query_embedding.role_specific.as_deref() {
            let key = vamana::AnnKey::new(ctx.ns, ctx.runtime.default_embedder_name());
            match khive_storage::await_request_read_phase(
                options.operation,
                search_eligible_ann_with_refill(
                    ctx,
                    token,
                    ann,
                    &key,
                    vector,
                    (options.ann_target, options.ann_initial_k),
                    stale,
                ),
            )
            .await
            {
                Ok(Ok(state)) => {
                    ann_hits = state.hits;
                    ann_availability = Some(state.availability);
                    hydration_failures = state.hydration_failures;
                }
                Ok(Err(error)) if is_read_timeout(&error) => {}
                Ok(Err(error)) => return Err(error),
                Err(error) if is_timeout(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }

        Ok(CandidateStageOutcome {
            ann_hits,
            ann_availability,
            hydration_failures,
            query_embedding,
        })
    };

    let (ann_result, lexical_result) = tokio::join!(ann_stage, lexical);
    let ann_result = ann_result?;
    Ok((lexical_result?, ann_result))
}
