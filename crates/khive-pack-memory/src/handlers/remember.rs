//! Handler for `memory.remember`.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::keyed_memory::{
    create_keyed_memory_with_receipt_and_report, validate_memory_key, KeyedMemorySpec,
};
use khive_runtime::{micros_to_iso, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{Direction, NeighborQuery};
use khive_storage::EdgeRelation;
use khive_types::{Details, KhiveError};

use crate::ann;
use crate::MemoryPack;

use super::common::{
    deser, to_json, validate_memory_type, RememberParams, DEFAULT_DECAY_EPISODIC,
    DEFAULT_DECAY_SEMANTIC, DEFAULT_SALIENCE_EPISODIC, DEFAULT_SALIENCE_SEMANTIC,
};

fn receipt_issuance_error(error: RuntimeError, memory_id: Uuid, replayed: bool) -> RuntimeError {
    let reason = match error.refusal_source() {
        RuntimeError::Khive(error) => {
            match error.details().and_then(|details| details.get("reason")) {
                Some("visibility_key_unavailable") => "visibility_key_unavailable",
                Some("visibility_nonce_unavailable") => "visibility_nonce_unavailable",
                Some("receipt_store_unavailable") => "receipt_store_unavailable",
                _ => "visibility_receipt_unavailable",
            }
        }
        _ => "visibility_receipt_unavailable",
    };
    KhiveError::unavailable("freshness_unmet: visibility receipt could not be issued")
        .with_details(Details::new_owned([
            ("reason", reason.to_owned()),
            ("memory_id", memory_id.to_string()),
            (
                "receipt_phase",
                if replayed {
                    "exact_replay"
                } else {
                    "post_commit"
                }
                .to_owned(),
            ),
        ]))
        .into()
}

impl MemoryPack {
    pub(crate) async fn handle_remember(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: RememberParams = deser(params)?;
        if let Some(key) = p.key.as_deref() {
            validate_memory_key(key)?;
        }
        if p.content.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "content must not be empty".into(),
            ));
        }

        let memory_type = p.memory_type.as_deref().unwrap_or("episodic");
        validate_memory_type(memory_type)?;

        // Explicit namespace wins; otherwise episodic uses actor scope and semantic uses local.
        // Direct-call defense in depth mirrors dispatch's Rule-3 namespace escape.
        let write_token_owned: Option<NamespaceToken> = if let Some(ns_str) = p.namespace.as_deref()
        {
            let ns = Namespace::parse(ns_str).map_err(|e| {
                RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))
            })?;
            Some(token.with_namespace(ns))
        } else if memory_type == "episodic" {
            let actor_id = token.actor().id.as_str();
            let ns = Namespace::parse(actor_id).map_err(|e| {
                RuntimeError::InvalidInput(format!(
                    "actor id {actor_id:?} is not a valid namespace: {e}"
                ))
            })?;
            Some(token.with_namespace(ns))
        } else {
            None
        };
        let write_token: &NamespaceToken = write_token_owned.as_ref().unwrap_or(token);

        let salience = match p.salience {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "salience must be in [0, 1], got {v}"
                )));
            }
            Some(v) => v,
            // Short-lived episodes start below durable semantic facts.
            None => match memory_type {
                "semantic" => DEFAULT_SALIENCE_SEMANTIC,
                _ => DEFAULT_SALIENCE_EPISODIC,
            },
        };
        let decay_factor = match p.decay_factor {
            Some(v) if !v.is_finite() || v < 0.0 => {
                return Err(RuntimeError::InvalidInput(format!(
                    "decay_factor must be a finite number >= 0, got {v}"
                )));
            }
            Some(v) => v,
            // Episodic context decays at ~35 days; semantic facts at ~139 days.
            None => match memory_type {
                "semantic" => DEFAULT_DECAY_SEMANTIC,
                _ => DEFAULT_DECAY_EPISODIC,
            },
        };

        let mut props = json!({ "memory_type": memory_type });
        if let Some(tags) = &p.tags {
            if !tags.is_empty() {
                props["tags"] = json!(tags);
            }
        }

        let mut annotates: Vec<Uuid> = vec![];
        if let Some(sid) = &p.source_id {
            if let Ok(full_uuid) = sid.parse::<Uuid>() {
                annotates.push(full_uuid);
            } else if sid.len() >= 8 && sid.chars().all(|c| c.is_ascii_hexdigit()) {
                match self.runtime.resolve_prefix(token, sid).await {
                    Ok(Some(uuid)) => annotates.push(uuid),
                    Ok(None) => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "source_id {sid:?}: no record matches this prefix"
                        )));
                    }
                    Err(e) => return Err(e),
                }
            } else {
                return Err(RuntimeError::InvalidInput(format!(
                    "source_id {sid:?} is not a valid UUID or 8-char short ID"
                )));
            }
        }

        if let Some(model_name) = p.embedding_model.as_deref() {
            self.runtime.resolve_embedding_model(Some(model_name))?;
        }

        self.runtime.notes(write_token)?;
        let sealing = self.runtime.ensure_visibility_receipt_key_if_configured()?;

        let annotates_target = annotates.first().copied();

        let (note, keyed_edge_id, replayed, vector_fences, embedding_truncation) =
            if let Some(key) = p.key.as_deref() {
                create_keyed_memory_with_receipt_and_report(
                    &self.runtime,
                    write_token,
                    KeyedMemorySpec {
                        content: &p.content,
                        key,
                        salience,
                        decay_factor,
                        properties: props,
                        source_id: annotates_target,
                        embedding_model: p.embedding_model.as_deref(),
                    },
                )
                .await?
            } else {
                // Retain both diagnostics after the committed write so bounded
                // embedding input does not skip the ANN generation bump below.
                let (note, fences, truncation) = self
                    .runtime
                    .create_note_with_decay_for_embedding_model_with_visibility_and_report(
                        write_token,
                        "memory",
                        None,
                        &p.content,
                        Some(salience),
                        decay_factor,
                        Some(props),
                        annotates,
                        p.embedding_model.as_deref(),
                    )
                    .await?;
                (note, None, false, fences, truncation)
            };

        if !replayed {
            // Preserve the stale graph as a fast fallback; generation is the invalidation signal.
            let affected_models: Vec<String> = match p.embedding_model.as_deref() {
                Some(model) => vec![model.to_owned()],
                None => self.runtime.registered_embedding_model_names(),
            };
            for model in affected_models {
                // Bump BEFORE warming so this write is included in the required generation floor.
                let key = ann::AnnKey::from_token(&model);
                ann::bump_generation(&self.ann, &key).await;
                ann::ensure_ann_background(&self.runtime, write_token, &self.ann, &model).await;
            }
        }

        // A replay answers from the stored memory, not from the request: the
        // annotation edge is the one the stored note still owns (whatever the
        // replay named as `source_id`), and `memory_type` is the value it was
        // written with, so a replay that sends a different type cannot echo it
        // back as if stored.
        let edge_id = if let Some(id) = keyed_edge_id {
            Some(id.to_string())
        } else if replayed || annotates_target.is_some() {
            let neighbors = self
                .runtime
                .neighbors_with_query(
                    write_token,
                    note.id,
                    NeighborQuery {
                        direction: Direction::Out,
                        relations: Some(vec![EdgeRelation::Annotates]),
                        limit: None,
                        min_weight: None,
                    },
                )
                .await;
            let neighbors = match neighbors {
                Ok(hits) => hits,
                // Prune can remove a replayed holder after its key lookup.
                Err(RuntimeError::NotFound(message))
                    if replayed && message == format!("neighbor anchor {} not found", note.id) =>
                {
                    Vec::new()
                }
                Err(error) => return Err(error),
            };
            neighbors
                .into_iter()
                .find(|hit| replayed || annotates_target == Some(hit.node_id))
                .map(|hit| hit.edge_id.to_string())
        } else {
            None
        };
        let response_memory_type: String = if replayed {
            note.properties
                .as_ref()
                .and_then(|pr| pr.get("memory_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("episodic")
                .to_owned()
        } else {
            memory_type.to_owned()
        };

        let visibility_token = if sealing {
            Some(
                self.runtime
                    .seal_visibility_receipt(&note.namespace, &vector_fences)
                    .map_err(|error| receipt_issuance_error(error, note.id, replayed))?,
            )
        } else {
            None
        };
        let mut response = json!({
            "id": note.id.to_string(),
            "kind": note.kind,
            "salience": note.salience,
            "decay_factor": note.decay_factor,
            "memory_type": response_memory_type,
            "created_at": micros_to_iso(note.created_at),
            "visibility_token": visibility_token,
        });
        if visibility_token.is_none() {
            response["visibility_token_reason"] = json!("visibility_key_unavailable");
        }
        if let Some(eid) = edge_id {
            response["edge_id"] = json!(eid);
        }
        if replayed {
            response["replayed"] = json!(true);
        }
        if embedding_truncation.any_truncated() {
            response["warnings"] =
                json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]);
        }
        to_json(&response)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use khive_pack_kg::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistryBuilder};
    use khive_storage::{SqlStatement, SqlValue};
    use lattice_embed::EmbeddingService;

    use crate::{
        test_support::{with_receipt_credentials, HashVecProvider},
        MemoryPack,
    };

    /// `memory.remember` must persist exactly ONE `NoteCreated` event carrying
    /// the calling actor, the new note's id as `target_id`, and
    /// `substrate = Note` so `decode_target_observation` resolves it as a `Note`
    /// referent. That event is now the runtime's, emitted once on the single
    /// note-create funnel. This pack used to emit a second one of its own beside
    /// it; the count assertion below is what makes that duplicate fail, so it is
    /// load-bearing rather than incidental.
    #[tokio::test]
    async fn remember_persists_note_created_event_with_target() {
        let rt = with_receipt_credentials(KhiveRuntime::memory().expect("in-memory runtime"));
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": "memory_remembered event payload coverage",
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("memory.remember must succeed");
        let note_id: uuid::Uuid = result["id"]
            .as_str()
            .expect("response must carry id")
            .parse()
            .expect("id must be a uuid");

        let store = rt.events(&token).expect("event store");
        let page = store
            .query_events(
                khive_storage::event::EventFilter {
                    kinds: vec![khive_types::EventKind::NoteCreated],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .expect("query_events");

        assert_eq!(
            page.items.len(),
            1,
            "exactly one NoteCreated event must be persisted: {page:?}"
        );
        let event = &page.items[0];
        assert_eq!(event.verb, "create");
        assert_eq!(
            event.actor,
            format!("{}:{}", token.actor().kind, token.actor().id)
        );
        assert_eq!(event.target_id, Some(note_id));
        assert_eq!(event.substrate, khive_types::SubstrateKind::Note);
        assert_eq!(event.payload["kind"], serde_json::json!("memory"));
        // `memory_type` is a property of the note the event targets, not a copy
        // in the event payload: the runtime emitter knows the note, not the verb
        // that asked for it. The response is the caller-facing surface for it.
        assert_eq!(result["memory_type"], serde_json::json!("semantic"));
        let receipt = rt
            .open_visibility_receipt(
                result["visibility_token"].as_str().expect("opaque receipt"),
                &["local"],
                &[],
            )
            .expect("a text-only memory has an authenticated empty vector fence set");
        assert_eq!(receipt.namespace(), "local");
        assert_eq!(receipt.sequence_for_model("unwritten-model"), None);
    }

    #[tokio::test]
    async fn remember_returns_the_vector_writes_transactional_ann_sequence() {
        const MODEL: &str = "remember-visibility-test-model";
        let rt = with_receipt_credentials(KhiveRuntime::memory().expect("in-memory runtime"));
        rt.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: 8,
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": "transactional visibility receipt for one memory vector",
                    "memory_type": "semantic",
                }),
            )
            .await
            .expect("remember vector");
        let token = result["visibility_token"].as_str().expect("opaque receipt");
        let receipt = rt
            .open_visibility_receipt(token, &["local"], &[MODEL.to_owned()])
            .expect("authenticated one-model receipt");
        assert_eq!(receipt.namespace(), "local");
        let seq = receipt
            .sequence_for_model(MODEL)
            .expect("positive log sequence");
        assert!(seq > 0);
        for hidden_field in [
            "ann_write_log_seq",
            "issued_at",
            "fences",
            "namespace",
            "model",
        ] {
            assert!(
                result.get(hidden_field).is_none(),
                "receipt surface exposed {hidden_field}"
            );
        }
        assert!(
            !token.contains(MODEL),
            "opaque receipt exposed the model sentinel"
        );

        // AUTOINCREMENT retains the committed high sequence even if an ANN
        // checkpoint compacts the individual log row before this assertion.
        let mut reader = rt.sql().reader().await.expect("sql reader");
        let observed = reader
            .query_scalar(SqlStatement {
                sql: "SELECT seq FROM sqlite_sequence WHERE name = 'ann_write_log'".into(),
                params: vec![],
                label: Some("remember-visibility-sequence-test".into()),
            })
            .await
            .expect("read sequence");
        assert!(matches!(observed, Some(SqlValue::Integer(value)) if value == seq as i64));
    }

    #[tokio::test]
    async fn remember_replay_returns_original_without_creating_a_duplicate() {
        let rt = with_receipt_credentials(KhiveRuntime::memory().expect("in-memory runtime"));
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let args = serde_json::json!({
            "content": "idempotent memory content",
            "memory_type": "semantic",
            "idempotency_key": "remember-replay",
        });
        let first = registry
            .dispatch("memory.remember", args.clone())
            .await
            .expect("first remember");
        let second = registry
            .dispatch("memory.remember", args)
            .await
            .expect("identical replay");

        assert_eq!(second["id"], first["id"]);
        assert_eq!(second["replayed"], serde_json::json!(true));
        assert_ne!(
            second["visibility_token"], first["visibility_token"],
            "replay must reseal with a fresh nonce"
        );
        for response in [&first, &second] {
            let receipt = rt
                .open_visibility_receipt(
                    response["visibility_token"]
                        .as_str()
                        .expect("opaque receipt"),
                    &["local"],
                    &[],
                )
                .expect("authenticated zero-model replay receipt");
            assert_eq!(receipt.namespace(), "local");
        }
        let notes = rt
            .notes(&token)
            .expect("note store")
            .get_live_notes_by_key("local", "remember-replay", Some("memory"))
            .await
            .expect("key lookup");
        assert_eq!(notes.len(), 1);

        let conflict = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": "different content",
                    "memory_type": "semantic",
                    "idempotency_key": "remember-replay",
                }),
            )
            .await
            .expect_err("different content under one key must refuse");
        assert!(conflict.to_string().contains("idempotency_key_conflict"));
        assert!(conflict.to_string().contains("remember-replay"));
        let notes_after = rt
            .notes(&token)
            .expect("note store")
            .get_live_notes_by_key("local", "remember-replay", Some("memory"))
            .await
            .expect("key lookup after refused replay");
        assert_eq!(notes_after.len(), 1);
        assert_eq!(notes_after[0].content, "idempotent memory content");
    }

    #[tokio::test]
    async fn remember_replay_answers_from_the_stored_memory() {
        // #2700: a replay must carry the stored note's annotation edge and the
        // memory_type it was written with, never the replay request's values.
        let rt = with_receipt_credentials(KhiveRuntime::memory().expect("in-memory runtime"));
        let ns = Namespace::parse("local").expect("local namespace");
        let token = rt.authorize(ns).expect("authorize local");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let source = rt
            .create_entity_with_embedding_report(
                &token,
                "concept",
                None,
                "replay source",
                None,
                None,
                vec![],
            )
            .await
            .map(|(row, _report)| row)
            .expect("source entity");
        let args = serde_json::json!({
            "content": "replayed memory answers from the store",
            "memory_type": "semantic",
            "namespace": "local",
            "source_id": source.id.to_string(),
            "idempotency_key": "remember-replay-stored",
        });
        let first = registry
            .dispatch("memory.remember", args.clone())
            .await
            .expect("first remember");
        let first_edge = first["edge_id"]
            .as_str()
            .expect("original keyed create reports its annotation edge")
            .to_owned();
        assert_eq!(first["memory_type"], serde_json::json!("semantic"));

        // Exact replay: same edge, same type, flagged as a replay.
        let exact = registry
            .dispatch("memory.remember", args)
            .await
            .expect("exact replay");
        assert_eq!(exact["id"], first["id"]);
        assert_eq!(exact["replayed"], serde_json::json!(true));
        assert_eq!(exact["edge_id"], serde_json::json!(first_edge));
        assert_eq!(exact["memory_type"], serde_json::json!("semantic"));

        // Same key and content, different type and no source_id: the response
        // still describes the stored memory.
        let drifted = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": "replayed memory answers from the store",
                    "memory_type": "episodic",
                    "namespace": "local",
                    "idempotency_key": "remember-replay-stored",
                }),
            )
            .await
            .expect("replay with a drifted type");
        assert_eq!(drifted["id"], first["id"]);
        assert_eq!(drifted["replayed"], serde_json::json!(true));
        assert_eq!(drifted["memory_type"], serde_json::json!("semantic"));
        assert_eq!(drifted["edge_id"], serde_json::json!(first_edge));
        let notes = rt
            .notes(&token)
            .expect("note store")
            .get_live_notes_by_key("local", "remember-replay-stored", Some("memory"))
            .await
            .expect("key lookup");
        assert_eq!(notes.len(), 1);
        let stored_type = notes[0]
            .properties
            .as_ref()
            .and_then(|pr| pr.get("memory_type"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        assert_eq!(stored_type.as_deref(), Some("semantic"));
    }
    struct CountingService(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl lattice_embed::EmbeddingService for CountingService {
        async fn embed(
            &self,
            texts: &[String],
            _: lattice_embed::EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
        }
        fn supports_model(&self, _: lattice_embed::EmbeddingModel) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "receipt-preflight-counter"
        }
    }

    struct CountingProvider {
        builds: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        embeds: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl khive_runtime::EmbedderProvider for CountingProvider {
        fn name(&self) -> &str {
            "receipt-preflight-counter"
        }
        fn dimensions(&self) -> usize {
            4
        }
        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
            self.builds
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(std::sync::Arc::new(CountingService(self.embeds.clone())))
        }
    }

    async fn domain_counts(runtime: &KhiveRuntime) -> Vec<i64> {
        let mut reader = runtime.sql().reader().await.expect("domain count reader");
        let mut counts = Vec::new();
        for table in [
            "notes",
            "graph_edges",
            "vector_provenance",
            "ann_write_log",
            "memory_visibility_receipts",
            "memory_visibility_fences",
            "memory_visibility_epochs",
        ] {
            let count = reader
                .query_scalar(SqlStatement {
                    sql: format!("SELECT COUNT(*) FROM {table}"),
                    params: vec![],
                    label: Some("receipt-preflight-domain-count".into()),
                })
                .await
                .expect("domain count");
            let Some(SqlValue::Integer(count)) = count else {
                panic!("integer domain count");
            };
            counts.push(count);
        }
        counts
    }

    async fn epoch_of(runtime: &KhiveRuntime, note_id: &str) -> Option<String> {
        let mut reader = runtime.sql().reader().await.expect("epoch reader");
        match reader
            .query_scalar(SqlStatement {
                sql: "SELECT epoch FROM memory_visibility_epochs WHERE note_id = ?1".into(),
                params: vec![SqlValue::Text(note_id.to_owned())],
                label: Some("receipt-custody-absent-epoch".into()),
            })
            .await
            .expect("epoch read")
        {
            Some(SqlValue::Text(epoch)) => Some(epoch),
            _ => None,
        }
    }

    #[tokio::test]
    async fn missing_receipt_key_writes_receipt_rows_without_a_token_for_cold_and_cached_embedders()
    {
        use khive_runtime::RuntimeError;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        // One write per runtime pair keeps a background ANN warm from an earlier
        // write out of the way of the next write's custody preflight.
        for (cached, keyed) in [(false, true), (true, false), (true, true), (false, false)] {
            let build_registry = |rt: &KhiveRuntime| {
                let mut builder = VerbRegistryBuilder::new();
                builder.register(KgPack::new(rt.clone()));
                builder.register(MemoryPack::new(rt.clone()));
                builder.build().expect("registry")
            };
            let builds = Arc::new(AtomicUsize::new(0));
            let embeds = Arc::new(AtomicUsize::new(0));
            let rt = KhiveRuntime::memory().expect("runtime without receipt custody");
            rt.register_embedder(CountingProvider {
                builds: builds.clone(),
                embeds: embeds.clone(),
            });
            let registry = build_registry(&rt);
            // The same write against a runtime that holds custody defines what a
            // sealed write persists; only the sealing differs.
            let sealed = with_receipt_credentials(KhiveRuntime::memory().expect("sealed runtime"));
            sealed.register_embedder(CountingProvider {
                builds: Arc::new(AtomicUsize::new(0)),
                embeds: Arc::new(AtomicUsize::new(0)),
            });
            let sealed_registry = build_registry(&sealed);
            if cached {
                rt.embedder("receipt-preflight-counter")
                    .await
                    .expect("prime the real cached service");
            }
            assert_eq!(builds.load(Ordering::SeqCst), usize::from(cached));
            let before = domain_counts(&rt).await;
            let sealed_before = domain_counts(&sealed).await;
            let mut args = serde_json::json!({
                "content": format!("custody-absent write, cached embedder: {cached}"),
                "memory_type": "semantic",
            });
            if keyed {
                args["key"] = serde_json::json!("receipt-preflight");
            }
            let response = registry
                .dispatch("memory.remember", args.clone())
                .await
                .expect("absent custody must not refuse the write");
            assert_eq!(response["visibility_token"], serde_json::Value::Null);
            assert_eq!(
                response["visibility_token_reason"],
                "visibility_key_unavailable"
            );
            assert!(embeds.load(Ordering::SeqCst) > 0);
            assert_eq!(builds.load(Ordering::SeqCst), 1);
            let sealed_response = sealed_registry
                .dispatch("memory.remember", args)
                .await
                .expect("sealed control write");
            assert!(sealed_response["visibility_token"].is_string());
            assert!(sealed_response.get("visibility_token_reason").is_none());

            // The ANN log is compacted in the background, so its row count is
            // not a stable measure; the rest are.
            let added = |rt_counts: Vec<i64>, before: &[i64]| -> Vec<i64> {
                let mut added: Vec<i64> =
                    rt_counts.iter().zip(before).map(|(a, b)| a - b).collect();
                added.remove(3);
                added
            };
            let added_absent = added(domain_counts(&rt).await, &before);
            assert_eq!(
                added_absent,
                added(domain_counts(&sealed).await, &sealed_before),
                "keyed: {keyed}: only sealing differs from a custody-holding write"
            );
            if keyed {
                // The note, receipt header, model fence and `modern` marker each
                // gain exactly one row.
                assert_eq!(added_absent, [1, 0, 0, 1, 1, 1]);
                let id = response["id"].as_str().expect("memory id");
                assert_eq!(epoch_of(&rt, id).await.as_deref(), Some("modern"));
            } else {
                assert_eq!(added_absent, [1, 0, 0, 0, 0, 0]);
            }
        }
        let rt = KhiveRuntime::memory().expect("runtime without receipt custody");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");
        let before = domain_counts(&rt).await;
        for invalid in [
            serde_json::json!({"content": "valid", "key": "invalid\0key"}),
            serde_json::json!({"content": " ", "memory_type": "semantic"}),
            serde_json::json!({"content": "valid", "namespace": "bad\0namespace"}),
        ] {
            let error = registry
                .dispatch("memory.remember", invalid)
                .await
                .expect_err("invalid arguments still refuse without custody");
            assert!(matches!(
                error.refusal_source(),
                RuntimeError::InvalidInput(_)
            ));
        }
        let error = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": "valid",
                    "memory_type": "semantic",
                    "embedding_model": "unregistered-receipt-model",
                }),
            )
            .await
            .expect_err("model validation still refuses without custody");
        assert!(matches!(
            error.refusal_source(),
            RuntimeError::UnknownModel(_)
        ));
        assert_eq!(domain_counts(&rt).await, before);
    }

    struct FailingReceiptProvider {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail_on_call: usize,
    }

    impl khive_runtime::credentials::CredentialProvider for FailingReceiptProvider {
        fn resolve(
            &self,
            _: &str,
        ) -> Result<
            khive_runtime::credentials::CredentialMaterial,
            khive_runtime::credentials::CredentialError,
        > {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if call >= self.fail_on_call {
                return Err(khive_runtime::credentials::CredentialError::Unavailable {
                    name: "provider-secret-sentinel".into(),
                });
            }
            Ok(khive_runtime::credentials::CredentialMaterial::new(
                vec![b'A'; 43],
            ))
        }
        fn cache_lifetime(&self) -> khive_runtime::credentials::CredentialCacheLifetime {
            khive_runtime::credentials::CredentialCacheLifetime::NoCache
        }
    }

    #[tokio::test]
    async fn configured_key_that_cannot_be_resolved_still_refuses_before_any_write() {
        use khive_runtime::credentials::{
            CredentialConfig, CredentialKind, CredentialRegistry, VisibilityReceiptConfig,
            VisibilityReceiptKeyConfig,
        };
        use khive_runtime::RuntimeError;
        use khive_types::ErrorKind;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let mut credentials = CredentialRegistry::new(vec![CredentialConfig {
            name: "receipt-down".into(),
            kind: CredentialKind::SigningKey,
            provider: "receipt-down-provider".into(),
            env_var: None,
            header: None,
        }])
        .expect("credential declaration");
        credentials
            .register_provider(
                "receipt-down-provider".into(),
                Arc::new(FailingReceiptProvider {
                    calls: calls.clone(),
                    fail_on_call: 1,
                }),
            )
            .expect("outage provider");
        let rt = KhiveRuntime::memory()
            .expect("in-memory runtime")
            .with_visibility_receipt_credentials(
                VisibilityReceiptConfig {
                    keys: vec![VisibilityReceiptKeyConfig {
                        id: "receipt-down-key".into(),
                        credential: "receipt-down".into(),
                        encrypt: true,
                    }],
                },
                Arc::new(credentials),
            )
            .expect("receipt custody");
        let builds = Arc::new(AtomicUsize::new(0));
        let embeds = Arc::new(AtomicUsize::new(0));
        rt.register_embedder(CountingProvider {
            builds: builds.clone(),
            embeds: embeds.clone(),
        });
        assert_eq!(
            rt.registered_embedding_model_names(),
            vec!["receipt-preflight-counter".to_owned()]
        );
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");
        let before = domain_counts(&rt).await;
        assert_eq!(before, [0; 7]);
        for key in [Some("receipt-down-key"), None] {
            let mut args = serde_json::json!({
                "content": "a configured key that cannot be resolved must not write",
                "memory_type": "semantic",
            });
            if let Some(key) = key {
                args["key"] = serde_json::json!(key);
            }
            let error = registry
                .dispatch("memory.remember", args)
                .await
                .expect_err("configured but unresolvable key must refuse");
            assert!(matches!(error.refusal_source(), RuntimeError::Khive(error)
                if error.kind() == ErrorKind::Unavailable
                    && error.details().and_then(|details| details.get("reason"))
                        == Some("visibility_key_unavailable")));
            assert_eq!(builds.load(Ordering::SeqCst), 0);
            assert_eq!(embeds.load(Ordering::SeqCst), 0);
            assert_eq!(domain_counts(&rt).await, before);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn actual_seal_key_failure_retains_holder_and_unknown_disposition_on_create_and_replay() {
        use khive_runtime::credentials::{
            CredentialConfig, CredentialKind, CredentialRegistry, VisibilityReceiptConfig,
            VisibilityReceiptKeyConfig,
        };
        use khive_runtime::{DomainDisposition, RuntimeError};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        for replayed in [false, true] {
            let rt = KhiveRuntime::memory().expect("in-memory runtime");
            let calls = Arc::new(AtomicUsize::new(0));
            let mut credentials = CredentialRegistry::new(vec![CredentialConfig {
                name: "receipt-outage".into(),
                kind: CredentialKind::SigningKey,
                provider: "receipt-outage-provider".into(),
                env_var: None,
                header: None,
            }])
            .expect("credential declaration");
            credentials
                .register_provider(
                    "receipt-outage-provider".into(),
                    Arc::new(FailingReceiptProvider {
                        calls: calls.clone(),
                        fail_on_call: if replayed { 4 } else { 2 },
                    }),
                )
                .expect("outage provider");
            let rt = rt
                .with_visibility_receipt_credentials(
                    VisibilityReceiptConfig {
                        keys: vec![VisibilityReceiptKeyConfig {
                            id: "receipt-outage-key".into(),
                            credential: "receipt-outage".into(),
                            encrypt: true,
                        }],
                    },
                    Arc::new(credentials),
                )
                .expect("receipt custody");
            let token = rt.authorize(Namespace::local()).expect("local token");
            let mut builder = VerbRegistryBuilder::new();
            builder.register(KgPack::new(rt.clone()));
            builder.register(MemoryPack::new(rt.clone()));
            let registry = builder.build().expect("registry");
            let args = serde_json::json!({
                "content": "committed receipt outage memory",
                "memory_type": "semantic",
                "key": "receipt-outage"
            });
            let first = if replayed {
                Some(
                    registry
                        .dispatch("memory.remember", args.clone())
                        .await
                        .expect("initial issuance"),
                )
            } else {
                None
            };
            let error = registry
                .dispatch("memory.remember", args)
                .await
                .expect_err("key becomes unavailable during actual issuance");
            assert_eq!(calls.load(Ordering::SeqCst), if replayed { 4 } else { 2 });
            let notes = rt
                .notes(&token)
                .expect("note store")
                .get_live_notes_by_key("local", "receipt-outage", Some("memory"))
                .await
                .expect("reconcile holder");
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].content, "committed receipt outage memory");
            if let Some(first) = first {
                assert_eq!(first["id"], notes[0].id.to_string());
            }
            let RuntimeError::Khive(domain) = error.refusal_source() else {
                panic!("typed issuance failure: {error:?}");
            };
            assert_eq!(
                domain.details().and_then(|details| details.get("reason")),
                Some("visibility_key_unavailable")
            );
            assert_eq!(
                domain
                    .details()
                    .and_then(|details| details.get("memory_id")),
                Some(notes[0].id.to_string().as_str())
            );
            assert_eq!(
                domain
                    .details()
                    .and_then(|details| details.get("receipt_phase")),
                Some(if replayed {
                    "exact_replay"
                } else {
                    "post_commit"
                })
            );
            let projected = khive_runtime::runtime_error_value(error, DomainDisposition::Unknown);
            assert_eq!(projected["domain_disposition"], "unknown");
            assert_eq!(projected["retryable"], true);
            assert!(!projected.to_string().contains("provider-secret-sentinel"));
            assert!(!projected.to_string().contains("ann_write_log_seq"));
        }
    }
}
