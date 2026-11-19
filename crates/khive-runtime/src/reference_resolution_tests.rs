use super::*;
use crate::config::{NamespaceToken as TokenCtor, RuntimeConfig};
use crate::embedder_registry::EmbedderProvider;
use crate::retrieval::SearchSource;
use khive_gate::ActorRef;
use khive_types::{namespace::Namespace, SubstrateKind};
use lattice_embed::{EmbeddingModel, EmbeddingService};
use std::sync::Arc;

struct ConstantEmbeddingService {
    dimensions: usize,
}

#[async_trait::async_trait]
impl EmbeddingService for ConstantEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0; self.dimensions]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "resolve-test-constant-embedding"
    }
}

struct ConstantEmbedderProvider {
    name: String,
    dimensions: usize,
}

#[async_trait::async_trait]
impl EmbedderProvider for ConstantEmbedderProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(ConstantEmbeddingService {
            dimensions: self.dimensions,
        }))
    }
}

fn runtime_with_constant_embeddings() -> KhiveRuntime {
    let model = EmbeddingModel::AllMiniLmL6V2;
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(model),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    runtime.register_embedder(ConstantEmbedderProvider {
        name: model.to_string(),
        dimensions: model.dimensions(),
    });
    runtime
}

fn actor_token(actor_id: &str) -> NamespaceToken {
    TokenCtor::mint_authorized(Namespace::local(), ActorRef::new("agent", actor_id))
}

#[tokio::test]
async fn id_string_passthrough_resolves_full_uuid() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    let entity = rt
        .create_entity(
            &token,
            "concept",
            None,
            "PassthroughTarget",
            None,
            None,
            vec![],
        )
        .await
        .expect("create entity");

    let resolution = resolve_reference(&rt, &ring, &token, &entity.id.to_string(), 5, None)
        .await
        .expect("resolve_reference");
    assert_eq!(
        resolution,
        ReferenceResolution::Resolved {
            id: entity.id,
            confidence: 1.0
        }
    );
}

#[tokio::test]
async fn id_string_passthrough_never_errors_on_a_miss() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    let missing = Uuid::new_v4();
    let resolution = resolve_reference(&rt, &ring, &token, &missing.to_string(), 5, None)
        .await
        .expect("must not error, only report NotFound");
    assert_eq!(resolution, ReferenceResolution::NotFound);
}

#[tokio::test]
async fn ring_exact_match_resolves_without_search() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();
    let actor = token.actor();
    let actor_key = format!("{}:{}", actor.kind, actor.id);

    let id = Uuid::new_v4();
    ring.admit(
        token.namespace().as_str(),
        &actor_key,
        id,
        Some("the old record".to_string()),
    );

    let resolution = resolve_reference(&rt, &ring, &token, "the old record", 5, None)
        .await
        .expect("resolve_reference");
    assert_eq!(
        resolution,
        ReferenceResolution::Resolved {
            id,
            confidence: RING_EXACT_CONFIDENCE
        }
    );
}

#[tokio::test]
async fn ring_ambiguous_on_multiple_exact_matches() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();
    let actor = token.actor();
    let actor_key = format!("{}:{}", actor.kind, actor.id);

    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();
    ring.admit(
        token.namespace().as_str(),
        &actor_key,
        id_a,
        Some("duplicate name".to_string()),
    );
    ring.admit(
        token.namespace().as_str(),
        &actor_key,
        id_b,
        Some("duplicate name".to_string()),
    );

    let resolution = resolve_reference(&rt, &ring, &token, "duplicate name", 5, None)
        .await
        .expect("resolve_reference");
    match resolution {
        ReferenceResolution::Ambiguous { candidates } => {
            assert_eq!(candidates.len(), 2);
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[tokio::test]
async fn no_ring_entry_and_no_search_hit_is_not_found() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    let resolution = resolve_reference(&rt, &ring, &token, "nothing matches this at all", 5, None)
        .await
        .expect("resolve_reference");
    assert_eq!(resolution, ReferenceResolution::NotFound);
}

#[tokio::test]
async fn actor_isolation_blocks_cross_actor_ring_reads() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token_a = actor_token("actor-a");
    let token_b = actor_token("actor-b");
    let ring = ReferenceRing::new();
    let actor_a = token_a.actor();
    let actor_key_a = format!("{}:{}", actor_a.kind, actor_a.id);

    let id = Uuid::new_v4();
    ring.admit(
        token_a.namespace().as_str(),
        &actor_key_a,
        id,
        Some("shared-namespace-name".to_string()),
    );

    // actor-b, same namespace, must NOT resolve via actor-a's ring entry.
    let resolution = resolve_reference(&rt, &ring, &token_b, "shared-namespace-name", 5, None)
        .await
        .expect("resolve_reference");
    assert_eq!(resolution, ReferenceResolution::NotFound);
}

// Regression for #849/#852: the stage-3 exact-name lookup used to filter
// `name_prefix` (`LIKE 'RoLoRA%'`) in memory, and `query_entities` ranks
// a `name_prefix` page by `CASE WHEN LOWER(name) = prefix THEN 0 ELSE 1
// END, created_at DESC`. Case-insensitive variants of the target name
// tie for priority 0 with the true exact match, so 100+ *newer*
// lowercase variants can fill the `LIMIT 100` page and page the older,
// case-exact target out entirely — the stage then falls through to
// hybrid search instead of resolving deterministically. The fix issues a
// storage-level `name = ?` (binary) predicate instead, so decoys that
// merely match case-insensitively never enter the result set at all.
#[tokio::test]
async fn exact_name_stage_survives_many_newer_case_variant_decoys() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    let target = rt
        .create_entity(&token, "concept", None, "RoLoRA", None, None, vec![])
        .await
        .expect("create target entity");

    // Case variants of the same name, not suffixed variants: SQLite's
    // `LIKE` is case-insensitive for ASCII, so a `rolora`-named decoy
    // still matches the `LIKE 'RoLoRA%'` pattern the buggy `name_prefix`
    // stage used, and the exact-match-ranking `CASE WHEN LOWER(name) =
    // ...` ties every one of these decoys with the true target at
    // priority 0 — leaving `created_at DESC` as the only tiebreak.
    let decoy_cases = ["rolora", "ROLORA", "RoLoRa", "roLORA"];
    for i in 0..120 {
        rt.create_entity(
            &token,
            "concept",
            None,
            decoy_cases[i % decoy_cases.len()],
            None,
            None,
            vec![],
        )
        .await
        .expect("create decoy entity");
    }

    let resolution = resolve_reference(&rt, &ring, &token, "RoLoRA", 5, None)
        .await
        .expect("resolve_reference");
    assert_eq!(
        resolution,
        ReferenceResolution::Resolved {
            id: target.id,
            confidence: EXACT_NAME_CONFIDENCE,
        }
    );
}

/// Issue #852: the zero/one/many decision must come from the
/// storage-computed `page.total` (a full `COUNT(*)` under the exact-name
/// predicate), not from `page.items.len()`, which the stage's own
/// `LIMIT 10` caps regardless of true cardinality. 11 byte-identical
/// exact names exceed that page limit, so the fetched page can only ever
/// carry 10 rows — `Ambiguous` must still fire (storage says 11 total,
/// not the truncated 10), and the returned `candidates` are a bounded
/// sample of the match set, not its entirety. That truncation is an
/// intentional, documented contract of this stage, not a bug: this test
/// pins both halves so a future change can't silently drop one.
#[tokio::test]
async fn exact_name_ambiguous_decision_uses_storage_total_not_page_len() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    for _ in 0..11 {
        rt.create_entity(&token, "concept", None, "DupeExactName", None, None, vec![])
            .await
            .expect("create duplicate-named entity");
    }

    let resolution = resolve_reference(&rt, &ring, &token, "DupeExactName", 5, None)
        .await
        .expect("resolve_reference");

    match resolution {
        ReferenceResolution::Ambiguous { candidates } => {
            assert_eq!(
                candidates.len(),
                10,
                "candidate set is a bounded 10-row sample of the 11 storage matches, \
                     not the complete set"
            );
        }
        other => panic!("expected Ambiguous driven by storage total (11), got {other:?}"),
    }
}

// Regression for #908 / #970: an exact canonical-name ref resolves to a
// single id through the stage-3 exact-name lookup, which short-circuits
// BEFORE the stage-4 hybrid fallback and its `limit` bound. Many other
// entities are strong hybrid matches for the same term — enough that
// hybrid alone would rank them close together and return `Ambiguous` — yet
// the exact name resolves deterministically to its owner. The returned
// `EXACT_NAME_CONFIDENCE` (not the stage-4 `SEARCH_RESOLVED_CONFIDENCE`)
// proves the result came from stage 3, regardless of the target's hybrid
// rank. An exact name is an identity, not a ranked candidate.
#[tokio::test]
async fn exact_name_resolves_ahead_of_competing_hybrid_matches() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    let target = rt
        .create_entity(
            &token,
            "concept",
            None,
            "ADR-040",
            Some("khive ADR-040"),
            None,
            vec![],
        )
        .await
        .expect("create target entity");

    // Competitors that also match "ADR-040" strongly in hybrid search but
    // do NOT carry it as an exact name; only the target owns the exact
    // name, so only the target satisfies the stage-3 lookup.
    for i in 0..12 {
        rt.create_entity(
            &token,
            "concept",
            None,
            &format!("ADR-040 companion {i}"),
            Some("ADR-040 ADR-040 ADR-040 ADR-040 ADR-040"),
            None,
            vec![],
        )
        .await
        .expect("create competing entity");
    }

    // Even at limit = 1 — the tightest bound — the exact name resolves to
    // the target, at the stage-3 confidence, never a ranked hybrid pick.
    let resolution = resolve_reference(&rt, &ring, &token, "ADR-040", 1, None)
        .await
        .expect("resolve_reference");
    assert_eq!(
        resolution,
        ReferenceResolution::Resolved {
            id: target.id,
            confidence: EXACT_NAME_CONFIDENCE,
        }
    );
}

// The complement of the exact-name contract: a NON-exact ref (no entity
// carries it as an exact name) that stays ambiguous returns a bounded
// sample capped at the caller's `limit`. Outside exact matches there is no
// oracle for a single "canonical" candidate, so the bound is the
// intentional, documented resolve() contract (raise `limit` to surface
// deeper ranks) — not a withheld identity. Any deterministic identity
// would have resolved upstream in stage 3.
#[tokio::test]
async fn fallback_stage_bounds_non_exact_payload_to_limit() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    // Twelve near-equal matches, each with a distinct name and the same
    // descriptive text — none is the "right" answer, so the result is a
    // genuinely bounded ambiguous sample, not a suppressed target.
    for i in 0..12 {
        rt.create_entity(
            &token,
            "concept",
            None,
            &format!("Retrieval Fusion Note {i}"),
            Some("khive retrieval fusion ranking note"),
            None,
            vec![],
        )
        .await
        .expect("create entity");
    }

    let resolution = resolve_reference(
        &rt,
        &ring,
        &token,
        "khive retrieval fusion ranking",
        5,
        None,
    )
    .await
    .expect("resolve_reference");

    match resolution {
        ReferenceResolution::Ambiguous { candidates } => {
            assert_eq!(
                candidates.len(),
                5,
                "a non-exact ref's ambiguity payload is bounded to the caller's limit"
            );
        }
        other => panic!("expected a bounded Ambiguous sample, got {other:?}"),
    }
}

// Regression for #908: genuine ambiguity — two entities with the exact
// same canonical name — must still return `Ambiguous`, never a silent
// pick, after widening stage 4's retrieval floor.
#[tokio::test]
async fn fallback_stage_still_reports_ambiguous_on_genuine_tie() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    let a = rt
        .create_entity(
            &token,
            "concept",
            None,
            "Twin Record",
            Some("khive Twin Record document"),
            None,
            vec![],
        )
        .await
        .expect("create entity a");
    let b = rt
        .create_entity(
            &token,
            "concept",
            None,
            "Twin Record",
            Some("khive Twin Record document"),
            None,
            vec![],
        )
        .await
        .expect("create entity b");

    // Exact byte-identical names hit stage 3 (exact-name storage
    // lookup), not stage 4 — but the same "must not silently pick"
    // contract applies at both stages, and stage 3 is a cheaper,
    // deterministic way to pin it.
    let resolution = resolve_reference(&rt, &ring, &token, "Twin Record", 5, None)
        .await
        .expect("resolve_reference");

    match resolution {
        ReferenceResolution::Ambiguous { candidates } => {
            let ids: std::collections::HashSet<Uuid> = candidates.iter().map(|c| c.id).collect();
            assert!(ids.contains(&a.id) && ids.contains(&b.id));
        }
        other => panic!("expected Ambiguous on a genuine name tie, got {other:?}"),
    }
}

#[tokio::test]
async fn fallback_stage_resolves_high_similarity_semantic_only_candidate() {
    let rt = runtime_with_constant_embeddings();
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    let entity = rt
        .create_entity(&token, "concept", None, "Canine", None, None, vec![])
        .await
        .expect("create semantic match");

    let raw_hits = rt
        .vector_search(
            &token,
            None,
            Some("domestic dog"),
            5,
            Some(SubstrateKind::Entity),
        )
        .await
        .expect("vector search");
    assert_eq!(raw_hits.len(), 1);
    assert_eq!(raw_hits[0].subject_id, entity.id);
    // Identical constant embeddings have canonical cosine score 1.0,
    // well above the raw-cosine floor of 0.3.
    assert!((raw_hits[0].score.to_f64() - 1.0).abs() < 1e-6);

    let hits = rt
        .hybrid_search(&token, "domestic dog", None, 5, None, None, &[], None)
        .await
        .expect("hybrid search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source, SearchSource::Vector);

    let resolution = resolve_reference(&rt, &ring, &token, "domestic dog", 5, None)
        .await
        .expect("resolve_reference");

    assert_eq!(
        resolution,
        ReferenceResolution::Resolved {
            id: entity.id,
            confidence: SEARCH_RESOLVED_CONFIDENCE,
        }
    );
}

#[tokio::test]
async fn fallback_stage_resolves_moderate_similarity_semantic_only_candidate() {
    // A canonical cosine score of 0.5 clears the raw-cosine floor of 0.3.
    // This guards against reintroducing the legacy `(1 + cos) / 2` floor
    // conversion, which would incorrectly raise the comparison floor to
    // 0.65 after vector-store scores moved to the canonical cosine scale.
    let rt = runtime_with_constant_embeddings();
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();
    let dimensions = EmbeddingModel::AllMiniLmL6V2.dimensions();

    let entity = rt
        .create_entity(
            &token,
            "concept",
            None,
            "Moderately Similar Candidate",
            None,
            None,
            vec![],
        )
        .await
        .expect("create moderate-similarity entity");
    let vectors = rt.vectors(&token).expect("vector store");
    vectors
        .delete(entity.id)
        .await
        .expect("delete generated vector");
    let aligned = dimensions * 3 / 4;
    let mut moderate_vector = vec![1.0f32; aligned];
    moderate_vector.extend(vec![-1.0f32; dimensions - aligned]);
    vectors
        .insert(
            entity.id,
            SubstrateKind::Entity,
            token.namespace().as_str(),
            "entity.body",
            vec![moderate_vector],
        )
        .await
        .expect("insert moderate-similarity vector");

    let query = "totally-nonexistent-moderate-zzz";
    let raw_hits = rt
        .vector_search(&token, None, Some(query), 5, Some(SubstrateKind::Entity))
        .await
        .expect("vector search");
    assert_eq!(raw_hits.len(), 1);
    assert_eq!(raw_hits[0].subject_id, entity.id);
    assert!((raw_hits[0].score.to_f64() - 0.5).abs() < 1e-6);

    let resolution = resolve_reference(&rt, &ring, &token, query, 5, None)
        .await
        .expect("resolve_reference");

    assert_eq!(
        resolution,
        ReferenceResolution::Resolved {
            id: entity.id,
            confidence: SEARCH_RESOLVED_CONFIDENCE,
        }
    );
}

#[tokio::test]
async fn fallback_stage_drops_orthogonal_semantic_only_candidate() {
    // An orthogonal vector has canonical cosine score 0.0, below the
    // raw-cosine floor of 0.3. A sole semantic candidate at this
    // similarity must be dropped, not resolved.
    let rt = runtime_with_constant_embeddings();
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();
    let dimensions = EmbeddingModel::AllMiniLmL6V2.dimensions();

    let entity = rt
        .create_entity(
            &token,
            "concept",
            None,
            "Unrelated Orthogonal",
            None,
            None,
            vec![],
        )
        .await
        .expect("create unrelated entity");
    let vectors = rt.vectors(&token).expect("vector store");
    vectors
        .delete(entity.id)
        .await
        .expect("delete generated vector");
    let mut orthogonal_vector = vec![1.0f32; dimensions / 2];
    orthogonal_vector.extend(vec![-1.0f32; dimensions - dimensions / 2]);
    vectors
        .insert(
            entity.id,
            SubstrateKind::Entity,
            token.namespace().as_str(),
            "entity.body",
            vec![orthogonal_vector],
        )
        .await
        .expect("insert orthogonal vector");

    let raw_hits = rt
        .vector_search(
            &token,
            None,
            Some("totally-nonexistent-orthogonal-zzz"),
            5,
            Some(SubstrateKind::Entity),
        )
        .await
        .expect("vector search");
    assert_eq!(raw_hits.len(), 1);
    assert!((raw_hits[0].score.to_f64() - 0.0).abs() < 1e-6);

    let resolution = resolve_reference(
        &rt,
        &ring,
        &token,
        "totally-nonexistent-orthogonal-zzz",
        5,
        None,
    )
    .await
    .expect("resolve_reference");

    assert_eq!(resolution, ReferenceResolution::NotFound);
}

#[tokio::test]
async fn fallback_stage_drops_low_similarity_semantic_only_candidates() {
    let rt = runtime_with_constant_embeddings();
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();
    let dimensions = EmbeddingModel::AllMiniLmL6V2.dimensions();

    let entity = rt
        .create_entity(
            &token,
            "concept",
            None,
            "Unrelated Alpha",
            None,
            None,
            vec![],
        )
        .await
        .expect("create unrelated entity");
    let vectors = rt.vectors(&token).expect("vector store");
    vectors
        .delete(entity.id)
        .await
        .expect("delete generated vector");
    // A quarter of dimensions aligned, three-quarters opposed against the
    // constant all-ones query embedding: canonical cosine score
    // 2*0.25 - 1 == -0.5. This is below the floor without being the
    // maximally-opposite case, so it exercises a genuine below-floor
    // mismatch rather than the degenerate cosine -1 extreme.
    let quarter = dimensions / 4;
    let mut mismatched_vector = vec![1.0f32; quarter];
    mismatched_vector.extend(vec![-1.0f32; dimensions - quarter]);
    vectors
        .insert(
            entity.id,
            SubstrateKind::Entity,
            token.namespace().as_str(),
            "entity.body",
            vec![mismatched_vector],
        )
        .await
        .expect("insert mismatched vector");

    let raw_hits = rt
        .vector_search(
            &token,
            None,
            Some("totally-nonexistent-zzz"),
            5,
            Some(SubstrateKind::Entity),
        )
        .await
        .expect("vector search");
    assert_eq!(raw_hits.len(), 1);
    assert!((raw_hits[0].score.to_f64() + 0.5).abs() < 1e-6);

    let resolution = resolve_reference(&rt, &ring, &token, "totally-nonexistent-zzz", 5, None)
        .await
        .expect("resolve_reference");

    assert_eq!(resolution, ReferenceResolution::NotFound);
}

// Regression for #908: garbage input that matches nothing must still be
// `NotFound` after widening stage 4's retrieval floor — the widened pool
// must not turn "nothing relevant exists" into a spurious pick.
#[tokio::test]
async fn fallback_stage_still_not_found_on_garbage() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = actor_token("resolver-test");
    let ring = ReferenceRing::new();

    rt.create_entity(
        &token,
        "concept",
        None,
        "ADR-040",
        Some("khive ADR-040"),
        None,
        vec![],
    )
    .await
    .expect("create unrelated entity");

    let resolution = resolve_reference(
        &rt,
        &ring,
        &token,
        "zzqxw completely unrelated garbage nonsense",
        5,
        None,
    )
    .await
    .expect("resolve_reference");
    assert_eq!(resolution, ReferenceResolution::NotFound);
}
