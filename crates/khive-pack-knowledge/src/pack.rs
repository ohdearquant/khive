//! `KnowledgePack` struct, factory, and `PackRuntime` impl.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use khive_brain_core::SectionPosteriorState;
use khive_runtime::pack::{PackByIdResolver, PackRuntime};
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackSchemaPlan, Resolved, RuntimeError, SchemaPlan,
    VerbRegistry,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{PhaseCancelledPayload, PhaseCompletedPayload, PhaseStartedPayload};
use khive_types::{EventKind, HandlerDef, Pack};

use crate::knowledge::util::{atom_from_row, atom_to_json, domain_from_row, domain_to_json};
use crate::knowledge::vamana;
use crate::knowledge::KnowledgeHandlers;
use crate::vocab::{KNOWLEDGE_HANDLERS, KNOWLEDGE_SCHEMA_PLAN_STMTS};

/// Knowledge corpus pack — atoms, domains, TF-IDF search, fold, import, and KG concept verbs.
pub struct KnowledgePack {
    pub(crate) runtime: KhiveRuntime,
    pub(crate) ann: vamana::SharedAnn,
    /// Pack-local Tier-3 feedback state, isolated by exact namespace.
    ///
    /// This fallback is used only when no configured or bound brain profile
    /// resolves. Keying it by namespace keeps explicit measurement arms from
    /// inheriting live/default feedback while preserving existing local behavior.
    pub(crate) section_posteriors: Mutex<HashMap<String, SectionPosteriorState>>,
    /// Explicit brain profile ID from config (ADR-035 §Brain profile configuration).
    ///
    /// Tier-1 of the 3-tier feedback resolution: when set, `knowledge.feedback` directs
    /// feedback to this profile via `brain.feedback`. When absent, tier-2
    /// (namespace-bound profile) and tier-3 (namespace-local prior) are tried in order.
    pub(crate) brain_profile: Option<String>,
}

impl Pack for KnowledgePack {
    const NAME: &'static str = "knowledge";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] =
        &[khive_brain_core::ConsumerKind::KnowledgeCompose.as_str()];
    const HANDLERS: &'static [HandlerDef] = &KNOWLEDGE_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: "knowledge",
        statements: &KNOWLEDGE_SCHEMA_PLAN_STMTS,
    });
}

impl KnowledgePack {
    /// Create a new pack bound to the given runtime, initializing a shared ANN index.
    pub fn new(runtime: KhiveRuntime) -> Self {
        let brain_profile = runtime.config().brain_profile.clone();
        Self {
            runtime,
            ann: vamana::new_shared(),
            section_posteriors: Mutex::new(HashMap::new()),
            brain_profile,
        }
    }
}

struct KnowledgePackFactory;

impl khive_runtime::PackFactory for KnowledgePackFactory {
    fn name(&self) -> &'static str {
        "knowledge"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(KnowledgePack::new(runtime))
    }

    fn create_resolver(
        &self,
        runtime: KhiveRuntime,
    ) -> Option<Box<dyn khive_runtime::PackByIdResolver>> {
        Some(Box::new(KnowledgePack::new(runtime)))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&KnowledgePackFactory) }

#[async_trait]
impl PackRuntime for KnowledgePack {
    fn name(&self) -> &str {
        <KnowledgePack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::ENTITY_KINDS
    }

    fn brain_consumer_kinds(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::BRAIN_CONSUMER_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &KNOWLEDGE_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::REQUIRES
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "knowledge",
            statements: &KNOWLEDGE_SCHEMA_PLAN_STMTS,
        }
    }

    async fn warm(&self) {
        // Vamana warm may register a missing ANN consumer or publish a rebuilt
        // checkpoint. A snapshot runtime keeps only the pure embedder warm
        // below; it must not enter a writer-bearing ANN lifecycle.
        if !self.runtime.is_read_only() {
            crate::knowledge::vamana::warm_known_snapshots(&self.runtime, &self.ann).await;
        }
        if !self.runtime.default_embedder_name().is_empty() {
            let runtime = self.runtime.clone();
            // ADR-103 Amendment 1 Part 2: the phase span brackets ONLY this
            // configured-embedder branch's spawned embed call, not the
            // unconditional `warm_known_snapshots` above -- an unconditional
            // span would record a phase for an embedder warmup that never
            // ran whenever no embedder is configured. Minted here (before
            // spawn) rather than inside the task: `authorize` is cheap and
            // synchronous, and minting outside keeps the same shape as
            // KgPack::warm's unconditional case.
            let token = match runtime.authorize(khive_runtime::Namespace::local()) {
                Ok(token) => Some(token),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "knowledge_embedder_warm: failed to mint ADR-103 phase-span \
                         attribution token; warmup proceeds without telemetry"
                    );
                    None
                }
            };
            tokio::spawn(async move {
                if let Some(token) = &token {
                    khive_runtime::emit_phase_event(
                        &runtime,
                        token,
                        "knowledge.embedder_warm",
                        EventKind::PhaseStarted,
                        PhaseStartedPayload {
                            work_class: "warm".into(),
                            phase: "knowledge_embedder_warm".into(),
                            corpus_size: None,
                        },
                    )
                    .await;
                }
                let phase_start = std::time::Instant::now();
                let cpu_start = khive_runtime::process_resource_usage();
                let result = runtime.embed("__khive_knowledge_warm__").await;
                if let Some(token) = &token {
                    let wall_us = phase_start.elapsed().as_micros() as i64;
                    let cpu_us = khive_runtime::cpu_delta_us(
                        cpu_start,
                        khive_runtime::process_resource_usage(),
                    );
                    match &result {
                        Err(e) if khive_runtime::is_benign_shutdown_cancellation(e) => {
                            khive_runtime::emit_phase_event(
                                &runtime,
                                token,
                                "knowledge.embedder_warm",
                                EventKind::PhaseCancelled,
                                PhaseCancelledPayload {
                                    work_class: "warm".into(),
                                    phase: "knowledge_embedder_warm".into(),
                                    wall_us,
                                    cpu_us,
                                },
                            )
                            .await;
                        }
                        _ => {
                            khive_runtime::emit_phase_event(
                                &runtime,
                                token,
                                "knowledge.embedder_warm",
                                EventKind::PhaseCompleted,
                                PhaseCompletedPayload {
                                    work_class: "warm".into(),
                                    phase: "knowledge_embedder_warm".into(),
                                    wall_us,
                                    cpu_us,
                                },
                            )
                            .await;
                        }
                    }
                }
                let _ = result;
            });
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "knowledge.upsert_atoms" => {
                KnowledgeHandlers::upsert_atoms(&self.runtime, token, params).await
            }
            "knowledge.upsert_domains" => {
                KnowledgeHandlers::upsert_domains(&self.runtime, token, params).await
            }
            "knowledge.get" => KnowledgeHandlers::get(&self.runtime, token, params).await,
            "knowledge.list" => KnowledgeHandlers::list(&self.runtime, token, params).await,
            "knowledge.delete_atoms" => {
                KnowledgeHandlers::delete_atoms(&self.runtime, token, params).await
            }
            "knowledge.stats" => KnowledgeHandlers::stats(&self.runtime, token, params).await,
            "knowledge.eval_retrieval" => {
                KnowledgeHandlers::eval_retrieval(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.index" => {
                KnowledgeHandlers::index(&self.runtime, token, params, &self.ann, None).await
            }
            "knowledge.fold" => KnowledgeHandlers::fold(&self.runtime, token, params).await,
            "knowledge.search" => {
                KnowledgeHandlers::search(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.suggest" => {
                KnowledgeHandlers::suggest(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.compose" => {
                // Public registry dispatch pre-applies an explicit namespace,
                // while PackRuntime can also be called directly by embedders.
                // A direct caller must supply a token already authorized for
                // the requested namespace; handlers may not mint a stronger
                // capability from an untrusted business parameter.
                let effective_token = match params.get("namespace") {
                    None => token.clone(),
                    Some(Value::String(ns_str)) => {
                        let ns = Namespace::parse(ns_str).map_err(|e| {
                            RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))
                        })?;
                        if &ns != token.namespace() {
                            return Err(RuntimeError::InvalidInput(
                                "knowledge.compose namespace does not match authorized token namespace"
                                    .to_string(),
                            ));
                        }
                        // Equality proves this is capability narrowing, not
                        // elevation. Replace any broader visible set so every
                        // compose leg observes exactly the requested arm.
                        token.with_namespace(ns)
                    }
                    Some(_) => {
                        return Err(RuntimeError::InvalidInput(
                            "invalid namespace: expected a string".to_string(),
                        ));
                    }
                };
                let type_weights = self
                    .resolve_compose_type_weights(registry, &effective_token)
                    .await;
                KnowledgeHandlers::compose(
                    &self.runtime,
                    &effective_token,
                    params,
                    &self.ann,
                    type_weights,
                )
                .await
            }
            "knowledge.edit" => {
                KnowledgeHandlers::edit(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.import" => {
                KnowledgeHandlers::import(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.challenge" => {
                KnowledgeHandlers::challenge(&self.runtime, token, params).await
            }
            "knowledge.adjudicate" => {
                KnowledgeHandlers::adjudicate(&self.runtime, token, params).await
            }
            "knowledge.learn" => self.handle_learn(token, params).await,
            "knowledge.cite" => self.handle_cite(token, params).await,
            "knowledge.topic" => self.handle_topic(token, params).await,
            "knowledge.feedback" => self.handle_feedback(token, params, registry).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "knowledge pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[async_trait]
impl PackByIdResolver for KnowledgePack {
    /// Resolve a live knowledge record by UUID.
    ///
    /// Queries `knowledge_domains` first (canonical record), then
    /// `knowledge_atoms`. The domain mirror atom shares the domain's UUID
    /// (crud.rs:279/300) so domains must win over the mirror.
    async fn resolve_by_id(&self, id: Uuid) -> Result<Option<Resolved>, RuntimeError> {
        let sql = self.runtime.sql();
        let id_str = id.to_string();

        let mut reader = sql
            .reader()
            .await
            .map_err(|e| RuntimeError::Internal(format!("knowledge resolve_by_id reader: {e}")))?;

        // 1. Check knowledge_domains first (canonical over the mirror atom).
        let domain_row = reader
            .query_row(SqlStatement {
                sql: "SELECT d.id, d.namespace, d.slug, d.name, d.description, d.tags, d.members, \
                      (SELECT a.properties FROM knowledge_atoms a WHERE a.id=d.id) AS properties, \
                      d.created_at, d.updated_at, d.deleted_at \
                      FROM knowledge_domains d WHERE d.id = ?1 AND d.deleted_at IS NULL LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(id_str.clone())],
                label: Some("knowledge.resolve_by_id.domain".into()),
            })
            .await
            .map_err(|e| {
                RuntimeError::Internal(format!("knowledge resolve_by_id domain query: {e}"))
            })?;

        if let Some(row) = domain_row {
            if let Some(domain) = domain_from_row(&row) {
                return Ok(Some(Resolved::PackRecord {
                    pack: "knowledge".into(),
                    kind: "domain".into(),
                    data: domain_to_json(&domain),
                }));
            }
        }

        // 2. Check knowledge_atoms.
        let atom_row = reader
            .query_row(SqlStatement {
                sql: "SELECT id, namespace, slug, name, content, tags, properties, \
                      status, finalized, source_uri, source_type, created_at, updated_at, deleted_at \
                      FROM knowledge_atoms WHERE id = ?1 AND deleted_at IS NULL LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(id_str)],
                label: Some("knowledge.resolve_by_id.atom".into()),
            })
            .await
            .map_err(|e| {
                RuntimeError::Internal(format!("knowledge resolve_by_id atom query: {e}"))
            })?;

        if let Some(row) = atom_row {
            if let Some(atom) = atom_from_row(&row) {
                return Ok(Some(Resolved::PackRecord {
                    pack: "knowledge".into(),
                    kind: "atom".into(),
                    data: atom_to_json(&atom),
                }));
            }
        }

        Ok(None)
    }

    /// Resolve a knowledge record including already-soft-deleted records.
    ///
    /// Used by the hard-delete path to locate tombstoned records.
    async fn resolve_by_id_including_deleted(
        &self,
        id: Uuid,
    ) -> Result<Option<Resolved>, RuntimeError> {
        let sql = self.runtime.sql();
        let id_str = id.to_string();

        let mut reader = sql.reader().await.map_err(|e| {
            RuntimeError::Internal(format!(
                "knowledge resolve_by_id_including_deleted reader: {e}"
            ))
        })?;

        let domain_row = reader
            .query_row(SqlStatement {
                sql: "SELECT d.id, d.namespace, d.slug, d.name, d.description, d.tags, d.members, \
                      (SELECT a.properties FROM knowledge_atoms a WHERE a.id=d.id) AS properties, \
                      d.created_at, d.updated_at, d.deleted_at \
                      FROM knowledge_domains d WHERE d.id = ?1 LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(id_str.clone())],
                label: Some("knowledge.resolve_by_id_incl_deleted.domain".into()),
            })
            .await
            .map_err(|e| {
                RuntimeError::Internal(format!(
                    "knowledge resolve_by_id_including_deleted domain query: {e}"
                ))
            })?;

        if let Some(row) = domain_row {
            if let Some(domain) = domain_from_row(&row) {
                return Ok(Some(Resolved::PackRecord {
                    pack: "knowledge".into(),
                    kind: "domain".into(),
                    data: domain_to_json(&domain),
                }));
            }
        }

        let atom_row = reader
            .query_row(SqlStatement {
                sql: "SELECT id, namespace, slug, name, content, tags, properties, \
                      status, finalized, source_uri, source_type, created_at, updated_at, deleted_at \
                      FROM knowledge_atoms WHERE id = ?1 LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(id_str)],
                label: Some("knowledge.resolve_by_id_incl_deleted.atom".into()),
            })
            .await
            .map_err(|e| {
                RuntimeError::Internal(format!(
                    "knowledge resolve_by_id_including_deleted atom query: {e}"
                ))
            })?;

        if let Some(row) = atom_row {
            if let Some(atom) = atom_from_row(&row) {
                return Ok(Some(Resolved::PackRecord {
                    pack: "knowledge".into(),
                    kind: "atom".into(),
                    data: atom_to_json(&atom),
                }));
            }
        }

        Ok(None)
    }

    /// Delete a knowledge domain or atom by UUID.
    ///
    /// Soft-delete by default (`hard=false`): sets `deleted_at = now()`.
    /// For domains, also tombstones the mirror atom in `knowledge_atoms`.
    /// Hard-delete (`hard=true`): permanently removes rows from the table(s).
    async fn delete_by_id(&self, id: Uuid, hard: bool) -> Result<serde_json::Value, RuntimeError> {
        let sql = self.runtime.sql();
        let id_str = id.to_string();

        // First determine the kind (including deleted rows so hard-delete can find tombstones).
        let kind = match self.resolve_by_id_including_deleted(id).await? {
            Some(Resolved::PackRecord { kind, .. }) => kind,
            _ => {
                return Err(RuntimeError::NotFound(format!(
                    "knowledge record not found: {id}"
                )));
            }
        };

        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| RuntimeError::Internal(format!("knowledge delete_by_id writer: {e}")))?;

        match kind.as_str() {
            "domain" => {
                if hard {
                    // Hard-delete: sections → mirror atom → domain (all in one transaction).
                    // knowledge_sections has a FK to knowledge_atoms(id) without ON DELETE
                    // CASCADE, so sections must go before the atom row.
                    writer
                        .execute_batch(vec![
                            SqlStatement {
                                sql: "DELETE FROM knowledge_sections WHERE atom_id = ?1".into(),
                                params: vec![SqlValue::Text(id_str.clone())],
                                label: Some(
                                    "knowledge.delete_by_id.domain_mirror_sections.hard".into(),
                                ),
                            },
                            SqlStatement {
                                sql: "DELETE FROM knowledge_atoms WHERE id = ?1".into(),
                                params: vec![SqlValue::Text(id_str.clone())],
                                label: Some("knowledge.delete_by_id.domain_mirror.hard".into()),
                            },
                            SqlStatement {
                                sql: "DELETE FROM knowledge_domains WHERE id = ?1".into(),
                                params: vec![SqlValue::Text(id_str.clone())],
                                label: Some("knowledge.delete_by_id.domain.hard".into()),
                            },
                        ])
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!(
                                "knowledge delete_by_id domain hard: {e}"
                            ))
                        })?;
                } else {
                    writer
                        .execute(SqlStatement {
                            sql: "UPDATE knowledge_domains SET deleted_at = ?1 \
                                  WHERE id = ?2 AND deleted_at IS NULL"
                                .into(),
                            params: vec![SqlValue::Integer(now_us), SqlValue::Text(id_str.clone())],
                            label: Some("knowledge.delete_by_id.domain.soft".into()),
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!(
                                "knowledge delete_by_id domain soft: {e}"
                            ))
                        })?;
                    // Tombstone the mirror atom so FTS no longer surfaces it.
                    writer
                        .execute(SqlStatement {
                            sql: "UPDATE knowledge_atoms SET deleted_at = ?1 \
                                  WHERE id = ?2 AND deleted_at IS NULL"
                                .into(),
                            params: vec![SqlValue::Integer(now_us), SqlValue::Text(id_str.clone())],
                            label: Some("knowledge.delete_by_id.domain_mirror.soft".into()),
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!(
                                "knowledge delete_by_id domain mirror soft: {e}"
                            ))
                        })?;
                }
            }
            "atom" => {
                if hard {
                    // Hard-delete: sections first (FK constraint), then the atom row.
                    writer
                        .execute_batch(vec![
                            SqlStatement {
                                sql: "DELETE FROM knowledge_sections WHERE atom_id = ?1".into(),
                                params: vec![SqlValue::Text(id_str.clone())],
                                label: Some("knowledge.delete_by_id.atom_sections.hard".into()),
                            },
                            SqlStatement {
                                sql: "DELETE FROM knowledge_atoms WHERE id = ?1".into(),
                                params: vec![SqlValue::Text(id_str.clone())],
                                label: Some("knowledge.delete_by_id.atom.hard".into()),
                            },
                        ])
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!("knowledge delete_by_id atom hard: {e}"))
                        })?;
                } else {
                    writer
                        .execute(SqlStatement {
                            sql: "UPDATE knowledge_atoms SET deleted_at = ?1 \
                                  WHERE id = ?2 AND deleted_at IS NULL"
                                .into(),
                            params: vec![SqlValue::Integer(now_us), SqlValue::Text(id_str.clone())],
                            label: Some("knowledge.delete_by_id.atom.soft".into()),
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!("knowledge delete_by_id atom soft: {e}"))
                        })?;
                }
            }
            other => {
                return Err(RuntimeError::Internal(format!(
                    "knowledge delete_by_id: unexpected kind {other:?}"
                )));
            }
        }

        Ok(serde_json::json!({
            "deleted": true,
            "id": id_str,
            "kind": kind,
            "hard": hard,
        }))
    }
}

#[cfg(test)]
mod tests {
    use khive_runtime::pack::PackRuntime;
    use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
    use lattice_embed::EmbeddingModel;

    use super::KnowledgePack;

    // ADR-103 Amendment 1 Part 2: knowledge_embedder_warm phase-span events.
    //
    // `KhiveRuntime::memory()` builds on `RuntimeConfig::no_embeddings()`
    // (runtime.rs), so `default_embedder_name()` is empty and the
    // configured-embedder branch in `KnowledgePack::warm()` never runs. This
    // pins that gating: the unconditional `vamana::warm_known_snapshots` call
    // above the branch must never itself emit a phase event, and the
    // configured-only branch must not fire when unconfigured.
    #[tokio::test]
    async fn knowledge_warm_emits_no_phase_events_when_no_embedder_configured() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let event_store = rt.events(&token).expect("event store must be available");

        let pack = KnowledgePack::new(rt.clone());
        pack.warm().await;

        let page = event_store
            .query_events(
                khive_storage::EventFilter::default(),
                khive_storage::PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .expect("query events");

        assert!(
            page.items.is_empty(),
            "no embedder configured -> KnowledgePack::warm() must emit zero phase events \
             (configured-only branch must not fire, and warm_known_snapshots must not \
             independently emit): {:?}",
            page.items
        );
    }

    #[tokio::test]
    async fn read_only_knowledge_cache_miss_does_not_start_ann_writer_lifecycle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = RuntimeConfig {
            db_path: Some(dir.path().join("read-only-knowledge-ann.db")),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            ..RuntimeConfig::no_embeddings()
        };
        drop(KhiveRuntime::new(config.clone()).expect("migrate snapshot source"));

        #[cfg(unix)]
        {
            let db_path = config.db_path.as_ref().expect("db path");
            khive_storage::test_support::freeze_snapshot_sidecars(db_path);
        }

        let read_only = KhiveRuntime::new_readonly(config).expect("open snapshot read-only");
        let token = read_only
            .authorize(Namespace::local())
            .expect("authorize local");
        let pack = KnowledgePack::new(read_only.clone());
        let key = crate::knowledge::vamana::AnnKey::new(
            token.namespace().as_str(),
            read_only.default_embedder_name(),
        );
        let before = read_only.backend().pool().writer_acquisition_snapshot();

        crate::knowledge::vamana::ensure_ann_background(&read_only, &token, &pack.ann);

        assert!(
            !crate::knowledge::vamana::is_warming_not_loaded(&pack.ann, &key),
            "a read-only search cache miss must not claim or spawn an ANN warm slot"
        );
        assert_eq!(
            read_only.backend().pool().writer_acquisition_snapshot(),
            before,
            "read-only ANN admission must not acquire a writer"
        );
    }
}
