//! Note hybrid search stages and candidate hydration.

use super::*;

impl KhiveRuntime {
    /// Search with an explicit top-level note status and strict vector errors.
    ///
    /// The count records distinct candidates whose status mismatches the selector after
    /// namespace, mailbox, kind, tag and property checks. It precedes superseded
    /// filtering and ranking, so it is not a count of missing eligible matches.
    /// The existing bounded four-times-per-arm window is unchanged. A supplied
    /// mailbox view adds a gate; it does not replace the default actor gate.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_notes_with_text_mode_and_status(
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
        note_status: khive_storage::note::NoteStatusSelector,
        additional_mailbox_view: Option<&crate::MailboxView>,
    ) -> RuntimeResult<(Vec<NoteSearchHit>, u64)> {
        let (hits, _vector_error, note_status_dropped) = self
            .search_notes_inner(
                token,
                query_text,
                query_vector,
                limit,
                note_kind,
                include_superseded,
                tags_any,
                properties_filter,
                text_mode,
                false,
                note_status,
                additional_mailbox_view,
            )
            .await?;
        Ok((hits, note_status_dropped))
    }

    /// Note-search outcome with explicit status and bounded status-drop evidence.
    ///
    /// A successful text leg survives a vector error, which remains in the
    /// existing outcome. Count semantics match the strict selector entrypoint.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_notes_outcome_with_text_mode_and_status(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        limit: u32,
        note_kind: Option<&str>,
        include_superseded: bool,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
        text_mode: TextQueryMode,
        note_status: khive_storage::note::NoteStatusSelector,
        additional_mailbox_view: Option<&crate::MailboxView>,
    ) -> RuntimeResult<(NoteSearchOutcome, u64)> {
        let (hits, vector_error, note_status_dropped) = self
            .search_notes_inner(
                token,
                query_text,
                None,
                limit,
                note_kind,
                include_superseded,
                tags_any,
                properties_filter,
                text_mode,
                true,
                note_status,
                additional_mailbox_view,
            )
            .await?;
        Ok((
            NoteSearchOutcome { hits, vector_error },
            note_status_dropped,
        ))
    }

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
        note_status: khive_storage::note::NoteStatusSelector,
        additional_mailbox_view: Option<&crate::MailboxView>,
    ) -> RuntimeResult<(Vec<NoteSearchHit>, Option<String>, u64)> {
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
            let _phase = khive_storage::request_context::enter_search_phase(
                khive_storage::request_context::SearchPhase::TextSearch,
            );
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
            let _phase = khive_storage::request_context::enter_search_phase(
                khive_storage::request_context::SearchPhase::VectorSearch,
            );
            if query_vector.is_some() || !self.default_embedder_name().is_empty() {
                crate::search_audit::record(crate::search_audit::SearchArm::Vector);
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
        let fused = {
            let _phase = khive_storage::request_context::enter_search_phase(
                khive_storage::request_context::SearchPhase::Fusion,
            );
            self.fuse_with_strategy(
                text_hits,
                vector_hits,
                &crate::fusion::FusionStrategy::Rrf { k: RRF_K },
                fuse_k,
            )
            .await?
        };

        let candidate_ids: Vec<Uuid> = fused.iter().map(|hit| hit.entity_id).collect();
        if candidate_ids.is_empty() {
            return Ok((vec![], vector_error, 0));
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
            maintainer_actor: self.config().actor_id.clone().filter(|id| !id.is_empty()),
        };
        let mut alive_notes: HashMap<Uuid, Note> = HashMap::new();
        // Fused candidate IDs and the batched identity read each contain one row per note.
        let mut note_status_dropped = 0_u64;
        let fetched_notes = {
            let _phase = khive_storage::request_context::enter_search_phase(
                khive_storage::request_context::SearchPhase::NoteHydration,
            );
            note_store.get_notes_batch(&candidate_ids).await?
        };
        let _authorization_phase = khive_storage::request_context::enter_search_phase(
            khive_storage::request_context::SearchPhase::MailboxAuthorization,
        );
        for note in fetched_notes {
            search_pool.record_note_candidate_hydration_row();
            // Identity batch reads are neutral. Revalidate the full row even
            // when an index candidate claimed a visible namespace.
            if !visible_ns
                .iter()
                .any(|namespace| namespace == &note.namespace)
            {
                continue;
            }
            if note.deleted_at.is_some() {
                continue;
            }
            if !mailbox_view.permits_message_note(token, &note) {
                continue;
            }
            if additional_mailbox_view.is_some_and(|view| !view.permits_message_note(token, &note))
            {
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
            if !note_status.matches(&note.status) {
                note_status_dropped = note_status_dropped.checked_add(1).ok_or_else(|| {
                    RuntimeError::Internal("note status drop count exceeds u64".into())
                })?;
                continue;
            }
            alive_notes.insert(note.id, note);
        }
        drop(_authorization_phase);

        // Drop superseded notes unless include_superseded is true: any note targeted
        // by a `supersedes` edge is obsolete and excluded from default search.
        if !include_superseded && !alive_notes.is_empty() {
            let graph = self.graph(token)?;
            let note_ids: Vec<Uuid> = alive_notes.keys().copied().collect();
            let superseded: std::collections::HashSet<Uuid> = {
                let _phase = khive_storage::request_context::enter_search_phase(
                    khive_storage::request_context::SearchPhase::SupersededFilter,
                );
                graph
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
                    .collect()
            };
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
        Ok((hits, vector_error, note_status_dropped))
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::embedder_registry::EmbedderProvider;
    use async_trait::async_trait;
    use khive_storage::note::{NoteStatusSelector, NoteStore};
    use khive_storage::types::TextDocument;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::collections::HashSet;
    use std::future::Future;
    use std::sync::Arc;
    use std::time::Duration;

    const QUERY: &str = "archivestatusfixture";

    // Every async setup/search/assertion in these tests is inside this budget.
    // Tokio timeouts bound cooperative async waits, not synchronous native code.
    async fn bounded(future: impl Future<Output = ()>) {
        tokio::time::timeout(Duration::from_secs(20), future)
            .await
            .expect("status fixture exceeded its async budget");
    }

    fn note(namespace: &str, kind: &str, status: &str) -> Note {
        let mut note = Note::new(namespace, kind, QUERY);
        note.status = status.into();
        note
    }

    async fn seed(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: Note,
        indexed_namespace: &str,
        indexed_kind: &str,
    ) -> Uuid {
        let id = note.id;
        // Isolated fixture only: actual backend NoteStore bypasses the live
        // message-forgery guard so private/stale records can be represented.
        runtime.backend().upsert_note(note).await.unwrap();
        runtime
            .text_for_notes(token)
            .unwrap()
            .upsert_document(TextDocument {
                subject_id: id,
                kind: khive_types::SubstrateKind::Note,
                record_kind: Some(indexed_kind.into()),
                namespace: indexed_namespace.into(),
                title: None,
                body: QUERY.into(),
                tags: vec![],
                metadata: None,
                updated_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        id
    }

    async fn raw_candidates(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        top_k: u32,
    ) -> HashSet<Uuid> {
        runtime
            .text_for_notes(token)
            .unwrap()
            .search(TextSearchRequest {
                query: QUERY.into(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![token.namespace().as_str().into()],
                    ..Default::default()
                }),
                top_k,
                snippet_chars: 100,
            })
            .await
            .unwrap()
            .into_iter()
            .map(|hit| hit.subject_id)
            .collect()
    }

    async fn select(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        status: NoteStatusSelector,
        limit: u32,
    ) -> (Vec<NoteSearchHit>, u64) {
        runtime
            .search_notes_with_text_mode_and_status(
                token,
                QUERY,
                None,
                limit,
                None,
                true,
                &[],
                None,
                TextQueryMode::Plain,
                status,
                None,
            )
            .await
            .unwrap()
    }

    fn ids(hits: &[NoteSearchHit]) -> HashSet<Uuid> {
        hits.iter().map(|hit| hit.note_id).collect()
    }

    #[tokio::test]
    async fn legacy_wrappers_default_active_and_explicit_selectors_differ() {
        bounded(async {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            let ns = token.namespace().as_str();
            let mut active = note(ns, "observation", "active");
            active.properties = Some(serde_json::json!({"status": "archived"}));
            let active_id = seed(&runtime, &token, active, ns, "observation").await;
            let active_second = seed(
                &runtime,
                &token,
                note(ns, "observation", "active"),
                ns,
                "observation",
            )
            .await;
            let mut archived = note(ns, "observation", "archived");
            archived.properties = Some(serde_json::json!({"status": "active"}));
            let archived_id = seed(&runtime, &token, archived, ns, "observation").await;
            let archived_second = seed(
                &runtime,
                &token,
                note(ns, "observation", "archived"),
                ns,
                "observation",
            )
            .await;
            let custom_id = seed(
                &runtime,
                &token,
                note(ns, "observation", "custom"),
                ns,
                "observation",
            )
            .await;
            let active_ids = HashSet::from([active_id, active_second]);
            let archived_ids = HashSet::from([archived_id, archived_second]);
            let all_ids = HashSet::from([
                active_id,
                active_second,
                archived_id,
                archived_second,
                custom_id,
            ]);
            assert_eq!(raw_candidates(&runtime, &token, 40).await, all_ids);
            let plain = runtime
                .search_notes(&token, QUERY, None, 10, None, true, &[], None)
                .await
                .unwrap();
            assert_eq!(ids(&plain), active_ids);
            let mode = runtime
                .search_notes_with_text_mode(
                    &token,
                    QUERY,
                    None,
                    10,
                    None,
                    true,
                    &[],
                    None,
                    TextQueryMode::Plain,
                )
                .await
                .unwrap();
            assert_eq!(ids(&mode), active_ids);
            let outcome = runtime
                .search_notes_outcome(&token, QUERY, 10, None, true, &[], None)
                .await
                .unwrap();
            assert_eq!(ids(&outcome.hits), active_ids);
            assert!(outcome.vector_error.is_none());
            let mode_outcome = runtime
                .search_notes_outcome_with_text_mode(
                    &token,
                    QUERY,
                    10,
                    None,
                    true,
                    &[],
                    None,
                    TextQueryMode::Plain,
                )
                .await
                .unwrap();
            assert_eq!(ids(&mode_outcome.hits), active_ids);
            assert!(mode_outcome.vector_error.is_none());
            for (selector, expected_ids, expected_count) in [
                (NoteStatusSelector::Active, active_ids, 3),
                (NoteStatusSelector::Archived, archived_ids, 3),
                (NoteStatusSelector::Any, all_ids, 0),
            ] {
                let (hits, count) = select(&runtime, &token, selector, 10).await;
                assert_eq!(ids(&hits), expected_ids);
                assert_eq!(
                    count, expected_count,
                    "all lifecycle mismatches count, including custom"
                );
                let (outcome, count) = runtime
                    .search_notes_outcome_with_text_mode_and_status(
                        &token,
                        QUERY,
                        10,
                        None,
                        true,
                        &[],
                        None,
                        TextQueryMode::Plain,
                        selector,
                        None,
                    )
                    .await
                    .unwrap();
                assert_eq!(ids(&outcome.hits), expected_ids);
                assert_eq!(count, expected_count);
                assert!(outcome.vector_error.is_none());
            }
            let raw = runtime
                .notes(&token)
                .unwrap()
                .get_notes_batch(&[archived_id, custom_id])
                .await
                .unwrap();
            assert_eq!(raw.len(), 2, "identity hydration remains status neutral");
        })
        .await;
    }

    #[tokio::test]
    async fn invisible_hydrated_candidate_never_becomes_hit_or_status_count() {
        bounded(async {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            let ns = token.namespace().as_str();
            let foreign_id = seed(
                &runtime,
                &token,
                note("foreign-status-fixture", "observation", "archived"),
                ns,
                "observation",
            )
            .await;
            // Prove the seeded mismatch reaches the genuine candidate and raw
            // hydration seams; a retrieval-only refusal cannot pass this test.
            assert!(raw_candidates(&runtime, &token, 40)
                .await
                .contains(&foreign_id));
            let raw = runtime
                .notes(&token)
                .unwrap()
                .get_notes_batch(&[foreign_id])
                .await
                .unwrap();
            assert_eq!(raw.len(), 1);
            assert_eq!(raw[0].namespace, "foreign-status-fixture");
            assert!(!token
                .visible_namespaces()
                .iter()
                .any(|ns| ns.as_str() == raw[0].namespace.as_str()));
            for status in [
                NoteStatusSelector::Active,
                NoteStatusSelector::Archived,
                NoteStatusSelector::Any,
            ] {
                let (hits, dropped) = select(&runtime, &token, status, 10).await;
                assert!(
                    hits.is_empty(),
                    "invisible hydrated candidate must not be a hit"
                );
                assert_eq!(
                    dropped, 0,
                    "invisible archived candidate must not be counted"
                );
            }
            let visible_id = seed(
                &runtime,
                &token,
                note(ns, "observation", "archived"),
                ns,
                "observation",
            )
            .await;
            let (hits, dropped) = select(&runtime, &token, NoteStatusSelector::Active, 10).await;
            assert!(hits.is_empty());
            assert_eq!(
                dropped, 1,
                "positive control: only visible archive contributes"
            );
            let (hits, dropped) = select(&runtime, &token, NoteStatusSelector::Any, 10).await;
            assert_eq!(ids(&hits), HashSet::from([visible_id]));
            assert_eq!(dropped, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn hydration_predicates_run_before_status_accounting() {
        bounded(async {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            let ns = token.namespace().as_str();
            let matched = serde_json::json!({"tags": ["wanted"], "project": "keep"});
            let mut kept_archive = note(ns, "observation", "archived");
            kept_archive.properties = Some(matched.clone());
            let archived_id = seed(&runtime, &token, kept_archive, ns, "observation").await;
            let mut kept_active = note(ns, "observation", "active");
            kept_active.properties = Some(matched.clone());
            let active_id = seed(&runtime, &token, kept_active, ns, "observation").await;
            let mut wrong_kind = note(ns, "question", "archived");
            wrong_kind.properties = Some(matched.clone());
            // Stale indexed kind ensures the full-note kind check is exercised.
            let wrong_kind_id = seed(&runtime, &token, wrong_kind, ns, "observation").await;
            let mut wrong_tag = note(ns, "observation", "archived");
            wrong_tag.properties = Some(serde_json::json!({"tags": ["other"], "project": "keep"}));
            let wrong_tag_id = seed(&runtime, &token, wrong_tag, ns, "observation").await;
            let mut wrong_property = note(ns, "observation", "archived");
            wrong_property.properties =
                Some(serde_json::json!({"tags": ["wanted"], "project": "other"}));
            let wrong_property_id = seed(&runtime, &token, wrong_property, ns, "observation").await;
            let mut deleted = note(ns, "observation", "archived");
            deleted.properties = Some(matched);
            deleted.deleted_at = Some(deleted.updated_at);
            let deleted_id = seed(&runtime, &token, deleted, ns, "observation").await;
            assert_eq!(
                raw_candidates(&runtime, &token, 40).await,
                HashSet::from([
                    archived_id,
                    active_id,
                    wrong_kind_id,
                    wrong_tag_id,
                    wrong_property_id,
                    deleted_id
                ])
            );
            let tags = vec!["wanted".into()];
            let properties = serde_json::json!({"project": "keep"});
            let (hits, dropped) = runtime
                .search_notes_with_text_mode_and_status(
                    &token,
                    QUERY,
                    None,
                    10,
                    Some("observation"),
                    true,
                    &tags,
                    Some(&properties),
                    TextQueryMode::Plain,
                    NoteStatusSelector::Active,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(ids(&hits), HashSet::from([active_id]));
            assert_eq!(
                dropped, 1,
                "only the otherwise matching live archive counts"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn default_mailbox_rejection_precedes_status_count() {
        bounded(async {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            let ns = token.namespace().as_str();
            let mut private = note(ns, "message", "archived");
            private.properties = Some(serde_json::json!({
                "direction": "inbound", "from_actor": "status-fixture-sender",
                "to_actor": "status-fixture-other"
            }));
            let private_id = seed(&runtime, &token, private, ns, "message").await;
            assert_eq!(
                raw_candidates(&runtime, &token, 40).await,
                HashSet::from([private_id])
            );
            for selector in [
                NoteStatusSelector::Active,
                NoteStatusSelector::Archived,
                NoteStatusSelector::Any,
            ] {
                // The real search constructs its own caller view; no test MailboxView literal.
                let (hits, dropped) = select(&runtime, &token, selector, 10).await;
                assert!(hits.is_empty());
                assert_eq!(
                    dropped, 0,
                    "another actor's inbound message contributes no count"
                );
            }
            let archive_id = seed(
                &runtime,
                &token,
                note(ns, "observation", "archived"),
                ns,
                "observation",
            )
            .await;
            let (hits, count) = select(&runtime, &token, NoteStatusSelector::Active, 10).await;
            assert!(hits.is_empty());
            assert_eq!(
                count, 1,
                "positive control: visible non-message archive is counted"
            );
            let (hits, count) = select(&runtime, &token, NoteStatusSelector::Any, 10).await;
            assert_eq!(ids(&hits), HashSet::from([archive_id]));
            assert_eq!(count, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn actual_authorized_additional_view_intersects_default_mailbox() {
        bounded(async {
            // Use the same real policy/authorization constructors as mailbox_view/tests.rs.
            let gate = crate::MailboxReadGate::new(
                Arc::new(crate::AllowAllGate),
                crate::ActorRef::new("actor", "lambda:owner"),
                vec![crate::ActorRef::new("actor", "lambda:reader")],
            )
            .unwrap();
            let runtime = KhiveRuntime::new(crate::RuntimeConfig {
                db_path: None,
                gate: Arc::new(gate),
                actor_id: Some("lambda:owner".into()),
                ..crate::RuntimeConfig::no_embeddings()
            })
            .unwrap();
            let token = NamespaceToken::mint_authorized(
                crate::Namespace::local(),
                crate::ActorRef::new("actor", "lambda:reader"),
            );
            let ns = token.namespace().as_str();
            let own_view = runtime
                .authorize_mailbox_view(
                    &token,
                    "search",
                    None,
                    &serde_json::json!({"query": QUERY, "kind": "note"}),
                )
                .unwrap();
            // This is a genuine separately authorized mailbox view, used only to
            // exercise the API's intersection invariant. Generic search itself
            // does not grant cross-actor mailbox selection.
            let owner_view = runtime
                .authorize_mailbox_view(
                    &token,
                    "comm.inbox",
                    Some("lambda:owner"),
                    &serde_json::json!({"mailbox_actor": "lambda:owner"}),
                )
                .unwrap();
            let mut own = note(ns, "message", "archived");
            own.properties =
                Some(serde_json::json!({"direction": "inbound", "to_actor": "lambda:reader"}));
            let mut other = note(ns, "message", "archived");
            other.properties =
                Some(serde_json::json!({"direction": "inbound", "to_actor": "lambda:owner"}));
            assert!(own_view.permits_message_note(&token, &own));
            assert!(!own_view.permits_message_note(&token, &other));
            assert!(!owner_view.permits_message_note(&token, &own));
            assert!(owner_view.permits_message_note(&token, &other));
            let own_id = seed(&runtime, &token, own, ns, "message").await;
            let other_id = seed(&runtime, &token, other, ns, "message").await;
            assert_eq!(
                raw_candidates(&runtime, &token, 40).await,
                HashSet::from([own_id, other_id])
            );
            let (hits, count) = runtime
                .search_notes_with_text_mode_and_status(
                    &token,
                    QUERY,
                    None,
                    10,
                    None,
                    true,
                    &[],
                    None,
                    TextQueryMode::Plain,
                    NoteStatusSelector::Active,
                    Some(&own_view),
                )
                .await
                .unwrap();
            assert!(hits.is_empty());
            assert_eq!(
                count, 1,
                "own authorized view preserves one otherwise-eligible archive"
            );
            for selector in [
                NoteStatusSelector::Active,
                NoteStatusSelector::Archived,
                NoteStatusSelector::Any,
            ] {
                let (hits, count) = runtime
                    .search_notes_with_text_mode_and_status(
                        &token,
                        QUERY,
                        None,
                        10,
                        None,
                        true,
                        &[],
                        None,
                        TextQueryMode::Plain,
                        selector,
                        Some(&owner_view),
                    )
                    .await
                    .unwrap();
                assert!(hits.is_empty(), "both independent mailbox gates apply");
                assert_eq!(
                    count, 0,
                    "neither mailbox intersection row may be disclosed by count"
                );
                let (outcome, count) = runtime
                    .search_notes_outcome_with_text_mode_and_status(
                        &token,
                        QUERY,
                        10,
                        None,
                        true,
                        &[],
                        None,
                        TextQueryMode::Plain,
                        selector,
                        Some(&owner_view),
                    )
                    .await
                    .unwrap();
                assert!(outcome.hits.is_empty());
                assert!(outcome.vector_error.is_none());
                assert_eq!(count, 0, "tolerant API preserves the additional view too");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn runtime_count_survives_both_underfilled_and_full_pages() {
        bounded(async {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            let ns = token.namespace().as_str();
            let active = seed(
                &runtime,
                &token,
                note(ns, "observation", "active"),
                ns,
                "observation",
            )
            .await;
            seed(
                &runtime,
                &token,
                note(ns, "observation", "archived"),
                ns,
                "observation",
            )
            .await;
            for limit in [1, 4] {
                let (hits, dropped) =
                    select(&runtime, &token, NoteStatusSelector::Active, limit).await;
                assert_eq!(ids(&hits), HashSet::from([active]));
                assert_eq!(
                    dropped, 1,
                    "count is independent of full versus underfilled presentation"
                );
                assert_eq!(hits.len() < limit as usize, limit == 4);
            }
        })
        .await;
    }

    #[tokio::test]
    async fn archive_heavy_search_keeps_existing_four_times_window() {
        bounded(async {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            let ns = token.namespace().as_str();
            let mut all = HashSet::new();
            for _ in 0..5 {
                all.insert(
                    seed(
                        &runtime,
                        &token,
                        note(ns, "observation", "active"),
                        ns,
                        "observation",
                    )
                    .await,
                );
            }
            // Choose the archive set from actual backend ordering rather than
            // assuming UUID/rowid tie-breaking. Status updates do not reindex.
            let gathered = raw_candidates(&runtime, &token, 4).await;
            assert_eq!(gathered.len(), 4);
            let outside: HashSet<_> = all.difference(&gathered).copied().collect();
            assert_eq!(outside.len(), 1);
            for id in &gathered {
                let mut row = runtime
                    .notes(&token)
                    .unwrap()
                    .get_note(*id)
                    .await
                    .unwrap()
                    .unwrap();
                row.status = "archived".into();
                runtime.backend().upsert_note(row).await.unwrap();
            }
            assert_eq!(raw_candidates(&runtime, &token, 4).await, gathered);
            let (hits, dropped) = select(&runtime, &token, NoteStatusSelector::Active, 1).await;
            assert!(
                hits.is_empty(),
                "bounded search must not silently refill outside its four candidates"
            );
            assert_eq!(dropped, 4);
            let outside_id = *outside.iter().next().unwrap();
            let outside_note = runtime
                .notes(&token)
                .unwrap()
                .get_note(outside_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                outside_note.status, "active",
                "the bounded result is not eligible-top-k complete"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn healthy_empty_and_all_matching_searches_return_zero_count() {
        bounded(async {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            for selector in [
                NoteStatusSelector::Active,
                NoteStatusSelector::Archived,
                NoteStatusSelector::Any,
            ] {
                let (hits, count) = select(&runtime, &token, selector, 10).await;
                assert!(hits.is_empty());
                assert_eq!(count, 0);
                let (outcome, count) = runtime
                    .search_notes_outcome_with_text_mode_and_status(
                        &token,
                        QUERY,
                        10,
                        None,
                        true,
                        &[],
                        None,
                        TextQueryMode::Plain,
                        selector,
                        None,
                    )
                    .await
                    .unwrap();
                assert!(outcome.hits.is_empty());
                assert!(outcome.vector_error.is_none());
                assert_eq!(count, 0);
            }
            let ns = token.namespace().as_str();
            let id = seed(
                &runtime,
                &token,
                note(ns, "observation", "active"),
                ns,
                "observation",
            )
            .await;
            let (hits, count) = select(&runtime, &token, NoteStatusSelector::Active, 10).await;
            assert_eq!(ids(&hits), HashSet::from([id]));
            assert_eq!(count, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn later_superseded_filter_does_not_revise_status_count() {
        bounded(async {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            let mut old = runtime
                .create_note(&token, "observation", None, QUERY, None, None, vec![])
                .await
                .unwrap();
            let new = runtime
                .create_note(&token, "observation", None, QUERY, None, None, vec![])
                .await
                .unwrap();
            runtime
                .link(&token, new.id, old.id, EdgeRelation::Supersedes, 1.0, None)
                .await
                .unwrap();
            old.status = "archived".into();
            runtime.backend().upsert_note(old.clone()).await.unwrap();
            assert_eq!(
                raw_candidates(&runtime, &token, 40).await,
                HashSet::from([old.id, new.id])
            );
            let (all, count) = select(&runtime, &token, NoteStatusSelector::Any, 10).await;
            assert_eq!(ids(&all), HashSet::from([old.id, new.id]));
            assert_eq!(count, 0);
            for (selector, expected, count) in [
                (NoteStatusSelector::Active, HashSet::from([new.id]), 1),
                (NoteStatusSelector::Archived, HashSet::new(), 1),
                (NoteStatusSelector::Any, HashSet::from([new.id]), 0),
            ] {
                let (hits, dropped) = runtime
                    .search_notes_with_text_mode_and_status(
                        &token,
                        QUERY,
                        None,
                        10,
                        None,
                        false,
                        &[],
                        None,
                        TextQueryMode::Plain,
                        selector,
                        None,
                    )
                    .await
                    .unwrap();
                assert_eq!(ids(&hits), expected);
                assert_eq!(
                    dropped, count,
                    "superseded filtering is later than status accounting"
                );
            }
        })
        .await;
    }

    struct FixtureEmbedder {
        name: String,
        dimensions: usize,
        fail: bool,
    }
    struct FixtureService {
        dimensions: usize,
        fail: bool,
    }

    #[async_trait]
    impl EmbedderProvider for FixtureEmbedder {
        fn name(&self) -> &str {
            &self.name
        }
        fn dimensions(&self) -> usize {
            self.dimensions
        }
        async fn build(&self) -> crate::error::RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(FixtureService {
                dimensions: self.dimensions,
                fail: self.fail,
            }))
        }
    }

    #[async_trait]
    impl EmbeddingService for FixtureService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            if self.fail {
                Err(EmbedError::InferenceFailed(
                    "search status fixture vector failure".into(),
                ))
            } else {
                Ok(texts
                    .iter()
                    .map(|_| vec![1.0_f32; self.dimensions])
                    .collect())
            }
        }
        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "search-status-fixture"
        }
    }

    #[tokio::test]
    async fn same_archived_note_from_both_arms_counts_once() {
        bounded(async {
            let model = EmbeddingModel::AllMiniLmL6V2;
            let runtime = KhiveRuntime::new(crate::RuntimeConfig {
                db_path: None,
                embedding_model: Some(model),
                ..crate::RuntimeConfig::no_embeddings()
            })
            .unwrap();
            let token = NamespaceToken::local();
            runtime.register_embedder(FixtureEmbedder {
                name: model.to_string(),
                dimensions: model.dimensions(),
                fail: false,
            });
            let mut row = runtime
                .create_note(&token, "observation", None, QUERY, None, None, vec![])
                .await
                .unwrap();
            let id = row.id;
            row.status = "archived".into();
            runtime.backend().upsert_note(row).await.unwrap();
            let (any, count) = select(&runtime, &token, NoteStatusSelector::Any, 10).await;
            assert_eq!(ids(&any), HashSet::from([id]));
            assert_eq!(any.len(), 1);
            assert_eq!(
                any[0].source,
                crate::SearchSource::Both,
                "fixture must actually exercise both arms"
            );
            assert_eq!(count, 0);
            let (explicit_vector, count) = runtime
                .search_notes_with_text_mode_and_status(
                    &token,
                    QUERY,
                    Some(vec![1.0_f32; model.dimensions()]),
                    10,
                    None,
                    true,
                    &[],
                    None,
                    TextQueryMode::Plain,
                    NoteStatusSelector::Any,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(ids(&explicit_vector), HashSet::from([id]));
            assert_eq!(explicit_vector[0].source, crate::SearchSource::Both);
            assert_eq!(count, 0);
            let (active, count) = select(&runtime, &token, NoteStatusSelector::Active, 10).await;
            assert!(active.is_empty());
            assert_eq!(count, 1);
        })
        .await;
    }

    #[tokio::test]
    async fn tolerant_selector_keeps_text_and_count_when_strict_vector_fails() {
        bounded(async {
            let model = EmbeddingModel::AllMiniLmL6V2;
            let runtime = KhiveRuntime::new(crate::RuntimeConfig {
                db_path: None,
                embedding_model: Some(model),
                ..crate::RuntimeConfig::no_embeddings()
            })
            .unwrap();
            let token = NamespaceToken::local();
            let ns = token.namespace().as_str();
            let active = seed(
                &runtime,
                &token,
                note(ns, "observation", "active"),
                ns,
                "observation",
            )
            .await;
            seed(
                &runtime,
                &token,
                note(ns, "observation", "archived"),
                ns,
                "observation",
            )
            .await;
            runtime.register_embedder(FixtureEmbedder {
                name: model.to_string(),
                dimensions: model.dimensions(),
                fail: true,
            });
            let error = runtime
                .search_notes_with_text_mode_and_status(
                    &token,
                    QUERY,
                    None,
                    10,
                    None,
                    true,
                    &[],
                    None,
                    TextQueryMode::Plain,
                    NoteStatusSelector::Active,
                    None,
                )
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("search status fixture vector failure"));
            let (outcome, count) = runtime
                .search_notes_outcome_with_text_mode_and_status(
                    &token,
                    QUERY,
                    10,
                    None,
                    true,
                    &[],
                    None,
                    TextQueryMode::Plain,
                    NoteStatusSelector::Active,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(ids(&outcome.hits), HashSet::from([active]));
            assert_eq!(count, 1);
            assert!(outcome
                .vector_error
                .as_deref()
                .unwrap()
                .contains("search status fixture vector failure"));
        })
        .await;
    }
}
