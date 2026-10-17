//! Note hybrid search stages and candidate hydration.

use super::*;

impl KhiveRuntime {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn search_notes_inner(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        note_kind: Option<&str>,
        include_superseded: bool,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
        text_mode: TextQueryMode,
        tolerate_vector_error: bool,
    ) -> RuntimeResult<(Vec<NoteSearchHit>, Option<String>)> {
        const RRF_K: usize = 60;
        let candidates = limit.saturating_mul(4).max(limit);
        let visible_ns: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();

        // FTS5 over the notes index — search all visible namespaces.
        //
        // `sanitize_fts5_query` strips known-unsafe FTS5 metacharacters, but
        // residual punctuation the sanitizer does not strip can still reach
        // the FTS5 parser and error. This fails loud instead of degrading to
        // vector-only fusion, so callers see the bad query instead of
        // silently losing the lexical leg. Errors from any other leg (vector
        // search, note hydration) still propagate normally.
        //
        // Injection: check FTS_SEARCH_FAIL_NS (armed by `arm_fts_search_fail(ns)`),
        // exercising the propagate branch above. Fires only when the armed
        // namespace is among this call's visible namespaces, then clears (one-shot).
        #[cfg(any(test, feature = "fault-injection"))]
        let fts_search_inject = {
            let mut g = FTS_SEARCH_FAIL_NS.lock().unwrap();
            match g.as_deref() {
                Some(armed) if visible_ns.iter().any(|ns| ns == armed) => {
                    *g = None;
                    true
                }
                _ => false,
            }
        };
        #[cfg(not(any(test, feature = "fault-injection")))]
        let fts_search_inject = false;

        let text_store = self.text_for_notes(token)?;
        let text_fut = async {
            if fts_search_inject {
                return Err(khive_storage::StorageError::Timeout {
                    operation: "fts_search".into(),
                });
            }
            text_store
                .search(TextSearchRequest {
                    query: query_text.to_string(),
                    mode: text_mode,
                    filter: Some(TextFilter {
                        namespaces: visible_ns.clone(),
                        // Push the note-kind filter into the FTS query. Without it the
                        // text arm returns the top `candidates` rows across EVERY note
                        // kind in the namespace and the kind is applied post-fetch, so a
                        // store where short message/session rows outrank task
                        // descriptions under BM25 hands the caller one or two task hits
                        // while the store holds many more carrying the literal.
                        record_kinds: note_kind
                            .map(|kind| vec![kind.to_string()])
                            .unwrap_or_default(),
                        ..TextFilter::default()
                    }),
                    top_k: candidates,
                    snippet_chars: 200,
                })
                .await
        };
        let text_fut = crate::stage_seam::text_stage(text_fut);

        // Vector search filtered to notes; it runs with the text stage, and a text error wins.
        let vector_fut = async {
            if query_vector.is_some() || !self.default_embedder_name().is_empty() {
                self.note_search_vector_search(token, query_vector, query_text, candidates)
                    .await
            } else {
                Ok(vec![])
            }
        };
        let (text_search_result, vector_result) = tokio::join!(text_fut, vector_fut);

        // FtsPasses is counted inside the store's `search()` (khive-db
        // stores/text.rs), only once a real FTS5 statement is prepared —
        // an empty/fully-sanitized query short-circuits there before any
        // statement exists and must not count (nor does the injected-failure
        // branch above, which never reaches the store at all).
        let text_hits = crate::error::fts_text_leg_or_err(
            text_search_result.map_err(RuntimeError::from),
            "search_notes",
            query_text,
        )?;

        let mut vector_error: Option<String> = None;
        let vector_hits = match vector_result {
            Ok(hits) => hits,
            Err(e) if tolerate_vector_error => {
                vector_error = Some(e.to_string());
                Vec::new()
            }
            Err(e) => return Err(e),
        };

        // Keep the full text∪vector union through RRF — salience weighting and
        // soft-delete/kind filtering happen *after* this, and the final
        // `hits.truncate(limit)` is the only result-limiting cut. Truncating to
        // `candidates` here would drop a high-salience note ranked just outside
        // the raw RRF cutoff before salience ever applied.
        let fuse_k = text_hits.len() + vector_hits.len();
        let fused = self
            .fuse_with_strategy(
                text_hits,
                vector_hits,
                &crate::fusion::FusionStrategy::Rrf { k: RRF_K },
                fuse_k,
            )
            .await?;

        let candidate_ids: Vec<Uuid> = fused.iter().map(|hit| hit.entity_id).collect();
        if candidate_ids.is_empty() {
            return Ok((vec![], vector_error));
        }

        // Hydrate every candidate note with one batched read to get salience and
        // apply soft-delete + (optional) kind filtering. The store chunks the read
        // below its bound-parameter ceiling, so the read costs `ceil(candidates / 900)`
        // statements instead of one per candidate. Notes whose `kind` doesn't
        // match `note_kind` are dropped post-fetch — they're a small set
        // bounded by the text∪vector union (≤ 2×candidates), so the read is cheap.
        let note_store = self.notes(token)?;
        let search_pool = self.backend().pool_arc();
        let mailbox_view = crate::MailboxView {
            actor_id: token.actor().id.clone(),
            delegated: false,
        };
        let mut alive_notes: HashMap<Uuid, Note> = HashMap::new();
        for note in note_store.get_notes_batch(&candidate_ids).await? {
            search_pool.record_note_candidate_hydration_row();
            if note.deleted_at.is_some() {
                continue;
            }
            if !mailbox_view.permits_message_note(token, &note) {
                continue;
            }
            if let Some(want_kind) = note_kind {
                if note.kind != want_kind {
                    continue;
                }
            }
            // Apply tag predicate before adding to alive set: tags on notes live
            // inside `properties["tags"]` (a JSON array). This pushes the filter
            // before truncation so matching notes ranked beyond `limit` in the raw
            // fusion are not silently dropped.
            if !tags_any.is_empty() {
                let note_tags: Vec<String> = note
                    .properties
                    .as_ref()
                    .and_then(|p| p.get("tags"))
                    .and_then(serde_json::Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if !note_tags
                    .iter()
                    .any(|t| tags_any.iter().any(|w| t.eq_ignore_ascii_case(w)))
                {
                    continue;
                }
            }
            // Apply properties predicate before truncation, same reasoning as tags above.
            if let Some(pf) = properties_filter {
                if !crate::retrieval::properties_match(note.properties.as_ref(), pf) {
                    continue;
                }
            }
            alive_notes.insert(note.id, note);
        }

        // Drop superseded notes unless include_superseded is true: any note targeted
        // by a `supersedes` edge is obsolete and excluded from default search.
        if !include_superseded && !alive_notes.is_empty() {
            let graph = self.graph(token)?;
            let note_ids: Vec<Uuid> = alive_notes.keys().copied().collect();
            let superseded: std::collections::HashSet<Uuid> = graph
                .batch_neighbors(
                    &note_ids,
                    NeighborQuery {
                        direction: Direction::In,
                        relations: Some(vec![EdgeRelation::Supersedes]),
                        limit: Some(1),
                        min_weight: None,
                    },
                )
                .await?
                .into_iter()
                .map(|(note_id, _)| note_id)
                .collect();
            alive_notes.retain(|id, _| !superseded.contains(id));
        }

        // Apply salience weighting and collect final hits.
        let mut hits: Vec<NoteSearchHit> = fused
            .into_iter()
            .filter_map(|hit| {
                let note = alive_notes.get(&hit.entity_id)?;
                let weighted = salience_weighted_rank(hit.score, note.salience);
                Some(NoteSearchHit {
                    note_id: hit.entity_id,
                    score: weighted,
                    rank_score_kind: hit.rank_score_kind,
                    signals: hit.signals,
                    source: hit.source,
                    title: hit.title.or_else(|| note_title(note)),
                    snippet: hit.snippet.or_else(|| note_snippet(note)),
                })
            })
            .collect();

        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.note_id.cmp(&b.note_id)));
        hits.truncate(limit as usize);
        Ok((hits, vector_error))
    }
}
