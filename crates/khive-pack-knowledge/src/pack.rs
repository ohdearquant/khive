//! `KnowledgePack` struct, factory, and `PackRuntime` impl.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use khive_brain_core::SectionPosteriorState;
use khive_runtime::pack::{PackByIdResolver, PackRuntime};
use khive_runtime::resource_admission::{
    AdmittedJob, BodyOutcome, FinalizationOutcome, JobOutcome, TrackedJob, WorkClass,
};
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackSchemaPlan, Resolved, RuntimeError, VerbRegistry,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{PhaseCancelledPayload, PhaseCompletedPayload, PhaseStartedPayload};
use khive_types::{EventKind, HandlerDef, Pack};

use crate::knowledge::util::{atom_from_row, atom_to_json, domain_from_row, domain_to_json};
use crate::knowledge::vamana;
use crate::knowledge::KnowledgeHandlers;
use crate::vocab::{KNOWLEDGE_HANDLERS, KNOWLEDGE_SCHEMA_PLAN_STMTS};

fn atom_vector_identity(
    row: &khive_storage::types::SqlRow,
) -> Result<(String, bool), khive_storage::StorageError> {
    let namespace = match row.get("namespace") {
        Some(SqlValue::Text(ns)) => ns.clone(),
        _ => {
            return Err(khive_storage::StorageError::Internal(
                "knowledge atom namespace missing or invalid".into(),
            ))
        }
    };
    let live = match row.get("deleted_at") {
        Some(SqlValue::Null) => true,
        Some(SqlValue::Integer(_)) => false,
        _ => {
            return Err(khive_storage::StorageError::Internal(
                "knowledge atom deleted_at missing or invalid".into(),
            ))
        }
    };
    Ok((namespace, live))
}

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
    /// section feedback through the trusted in-process brain hook. When absent, tier-2
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
        Self::new_with_index_role(runtime, true)
    }

    /// As [`Self::new`], but states whether this process may build the atom
    /// index from the full corpus. See `MemoryPack::new_with_index_role` for the
    /// reasoning; the two packs share one index root per model family and the
    /// same cost.
    pub fn new_with_index_role(runtime: KhiveRuntime, builds_corpus_indexes: bool) -> Self {
        let brain_profile = runtime.config().brain_profile.clone();
        Self {
            runtime,
            ann: vamana::new_shared_for_role(builds_corpus_indexes),
            section_posteriors: Mutex::new(HashMap::new()),
            brain_profile,
        }
    }
}

struct KnowledgePackFactory;

impl khive_runtime::PackFactory for KnowledgePackFactory {
    khive_runtime::pack_factory_metadata!(KnowledgePack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(KnowledgePack::new_with_index_role(
            runtime,
            khive_runtime::daemon::is_warm_index_host(),
        ))
    }

    fn create_resolver(
        &self,
        runtime: KhiveRuntime,
    ) -> Option<Box<dyn khive_runtime::PackByIdResolver>> {
        Some(Box::new(KnowledgePack::new(runtime)))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&KnowledgePackFactory) }

// The supervisor outlives its control handle so cancellation can finalize telemetry.
async fn run_knowledge_embedder_warm(
    runtime: KhiveRuntime,
    token: Option<NamespaceToken>,
    job: AdmittedJob,
) -> JobOutcome<Result<Vec<f32>, RuntimeError>> {
    let phase_entry = std::time::Instant::now();
    let entry_cpu = khive_runtime::process_resource_usage();
    let mut start_attempted = false;
    let start_finished = if let Some(token) = &token {
        match job
            .run_body(async {
                // Set on the future's first poll, after run_body's pre-start check.
                start_attempted = true;
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
            })
            .await
        {
            BodyOutcome::Finished(()) => true,
            BodyOutcome::Cancelled => false,
        }
    } else {
        true
    };
    if !start_finished && !start_attempted {
        // No phase append was even polled, so do not invent a terminal pair.
        return JobOutcome::Cancelled;
    }
    let (phase_start, cpu_start, result) = if start_finished {
        // Preserve the ordinary timing boundary after the start append returns.
        let phase_start = std::time::Instant::now();
        let cpu_start = khive_runtime::process_resource_usage();
        let result = job
            .run_body(runtime.embed("__khive_knowledge_warm__"))
            .await;
        (phase_start, cpu_start, result)
    } else {
        tracing::warn!(
            phase = "knowledge_embedder_warm",
            start_attempted,
            start_finished,
            start_append_persistence = "unknown",
            "warm start append was interrupted before returning",
        );
        (phase_entry, entry_cpu, BodyOutcome::Cancelled)
    };
    let cancelled = match &result {
        BodyOutcome::Cancelled => true,
        BodyOutcome::Finished(Err(error)) => khive_runtime::is_benign_shutdown_cancellation(error),
        BodyOutcome::Finished(Ok(_)) => false,
    };
    if let BodyOutcome::Finished(Err(error)) = &result {
        tracing::warn!(error = %error, "knowledge_embedder_warm: warm operation returned an error");
    }
    let finalization = if let Some(token) = &token {
        let wall_us = phase_start.elapsed().as_micros() as i64;
        let cpu_us =
            khive_runtime::cpu_delta_us(cpu_start, khive_runtime::process_resource_usage());
        // Select the terminal once; cancellation only shortens its append budget.
        job.finalize(async {
            if cancelled {
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
            } else {
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
        })
        .await
    } else {
        job.no_telemetry()
    };
    match finalization {
        FinalizationOutcome::AttemptTimedOut
        | FinalizationOutcome::NoAttemptBeforeDeadline
        | FinalizationOutcome::DeadlineOverflow => {
            tracing::warn!(
                ?finalization,
                "knowledge_embedder_warm: terminal append attempt did not finish"
            );
            return JobOutcome::FinalizationTimedOut;
        }
        FinalizationOutcome::AlreadyFinalized => {
            tracing::warn!("knowledge_embedder_warm: terminal finalization was already claimed");
        }
        FinalizationOutcome::AttemptReturned | FinalizationOutcome::NoTelemetryToken => {}
    }
    match result {
        BodyOutcome::Finished(result) => JobOutcome::Completed(result),
        BodyOutcome::Cancelled => JobOutcome::Cancelled,
    }
}

#[async_trait]
impl PackRuntime for KnowledgePack {
    khive_runtime::pack_runtime_metadata!();

    async fn warm(&self) {
        // Vamana warm may register a missing ANN consumer or publish a rebuilt
        // checkpoint. A snapshot runtime keeps only the pure embedder warm
        // below; it must not enter a writer-bearing ANN lifecycle.
        if !self.runtime.is_read_only() {
            crate::knowledge::vamana::warm_known_snapshots(&self.runtime, &self.ann).await;
            crate::knowledge::vamana::start_rotation_watcher(&self.runtime, &self.ann);
        }
        if !self.runtime.default_embedder_name().is_empty() {
            let runtime = self.runtime.background_view();
            let domain = runtime.background_domain();
            // Register synchronously before queueing, without a second tracked task.
            let tracking = TrackedJob::register("knowledge_embedder_warm");
            let token = match runtime.authorize(khive_runtime::Namespace::local()) {
                Ok(token) => Some(token),
                Err(error) => {
                    tracing::warn!(error = %error, "knowledge_embedder_warm: warmup proceeds without telemetry");
                    None
                }
            };
            // This ambient task does admission/handoff only. The phase and embed
            // body run on the isolated domain after admission, never on this task.
            tokio::spawn(async move {
                let admitted = match domain.admit(WorkClass::Warm, &tracking).await {
                    Ok(job) => job,
                    Err(error) => {
                        tracing::warn!(error = %error, "knowledge_embedder_warm: admission refused");
                        return;
                    }
                };
                match domain.spawn_supervised(admitted, move |job| {
                    run_knowledge_embedder_warm(runtime, token, job)
                }) {
                    Ok(task) => task.detach(),
                    Err(error) => {
                        tracing::warn!(error = %error, "knowledge_embedder_warm: supervisor submission refused");
                    }
                }
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
            "knowledge.export" => KnowledgeHandlers::export(&self.runtime, token, params).await,
            "knowledge.lint" => self.handle_lint(token, params, registry).await,
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
                sql: khive_runtime::sql!("knowledge_resolver_live_domain").into(),
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
                sql: khive_runtime::sql!("knowledge_resolver_live_atom").into(),
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
                sql: khive_runtime::sql!("knowledge_resolver_domain_including_deleted").into(),
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
                sql: khive_runtime::sql!("knowledge_resolver_atom_including_deleted").into(),
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

    fn private_record_verbs(&self) -> &'static [&'static str] {
        &[
            "knowledge.upsert_atoms",
            "knowledge.upsert_domains",
            "knowledge.edit",
        ]
    }

    /// Delete a knowledge domain or atom by UUID.
    ///
    /// Soft-delete by default (`hard=false`): sets `deleted_at = now()`.
    /// For domains, also tombstones the mirror atom in `knowledge_atoms`.
    /// Hard-delete (`hard=true`): permanently removes rows from the table(s).
    async fn delete_by_id(&self, id: Uuid, hard: bool) -> Result<serde_json::Value, RuntimeError> {
        let sql = self.runtime.sql();
        let id_str = id.to_string();
        let runtime = self.runtime.clone();
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;
        let outcome = sql
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    // Resolve on the writer so the record and its namespace cannot
                    // change between resolution, tombstoning, and index cleanup.
                    let domain = writer
                        .query_row(SqlStatement::new(
                            khive_runtime::sql!("knowledge_resolver_domain_including_deleted"),
                            vec![SqlValue::Text(id.to_string())],
                        ))
                        .await?;
                    let atom = writer
                        .query_row(SqlStatement::new(
                            khive_runtime::sql!("knowledge_resolver_atom_including_deleted"),
                            vec![SqlValue::Text(id.to_string())],
                        ))
                        .await?;
                    let kind = if domain.as_ref().and_then(domain_from_row).is_some() {
                        "domain"
                    } else if atom.as_ref().and_then(atom_from_row).is_some() {
                        "atom"
                    } else {
                        return Ok(Box::new(None::<String>) as Box<dyn std::any::Any + Send>);
                    };
                    let (atom_namespace, atom_live) = if let Some(row) = atom.as_ref() {
                        let (namespace, live) = atom_vector_identity(row)?;
                        (Some(namespace), live)
                    } else {
                        (None, false)
                    };
                    let tables = if atom_namespace.is_some() && (hard || atom_live) {
                        khive_runtime::atomic_prepare::discover_vector_tables_in_writer(
                            writer, &runtime,
                        )
                        .await?
                    } else {
                        Vec::new()
                    };
                    if hard {
                        writer
                            .execute(SqlStatement::new(
                                khive_runtime::sql!("knowledge_atom_sections_delete"),
                                vec![SqlValue::Text(id.to_string())],
                            ))
                            .await?;
                        writer
                            .execute(SqlStatement::new(
                                khive_runtime::sql!("knowledge_atom_hard_delete"),
                                vec![SqlValue::Text(id.to_string())],
                            ))
                            .await?;
                        if kind == "domain" {
                            writer
                                .execute(SqlStatement::new(
                                    khive_runtime::sql!("knowledge_domain_hard_delete"),
                                    vec![SqlValue::Text(id.to_string())],
                                ))
                                .await?;
                        }
                    } else {
                        if kind == "domain" {
                            writer
                                .execute(SqlStatement::new(
                                    khive_runtime::sql!("knowledge_domain_soft_delete"),
                                    vec![SqlValue::Integer(now_us), SqlValue::Text(id.to_string())],
                                ))
                                .await?;
                        }
                        writer
                            .execute(SqlStatement::new(
                                khive_runtime::sql!("knowledge_atom_soft_delete"),
                                vec![SqlValue::Integer(now_us), SqlValue::Text(id.to_string())],
                            ))
                            .await?;
                    }
                    if hard || atom_live {
                        if let Some(namespace) = atom_namespace {
                            for statement in khive_runtime::atomic_prepare::vector_purge_statements(
                                &tables,
                                &namespace,
                                &[id.to_string()],
                                Some("knowledge.atom"),
                                "knowledge-delete-by-id",
                            )? {
                                writer.execute(statement).await?;
                            }
                        }
                    }
                    Ok(Box::new(Some(kind.to_string())) as Box<dyn std::any::Any + Send>)
                })
            }))
            .await
            .map_err(|e| RuntimeError::Internal(format!("knowledge delete_by_id: {e}")))?;
        let kind = (*outcome.downcast::<Option<String>>().map_err(|_| {
            RuntimeError::Internal("invalid knowledge delete_by_id outcome".into())
        })?)
        .ok_or_else(|| RuntimeError::NotFound(format!("knowledge record not found: {id}")))?;

        Ok(serde_json::json!({
            "deleted": true,
            "id": id_str,
            "kind": kind,
            "hard": hard,
        }))
    }
}

#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "warm_admission_tests.rs"]
mod warm_admission_tests;

#[cfg(test)]
mod atom_vector_identity_tests {
    use super::*;
    use khive_storage::types::{SqlColumn, SqlRow};

    fn row(columns: Vec<(&str, SqlValue)>) -> SqlRow {
        SqlRow {
            columns: columns
                .into_iter()
                .map(|(name, value)| SqlColumn {
                    name: name.to_string(),
                    value,
                })
                .collect(),
        }
    }

    #[test]
    fn rejects_missing_or_wrong_typed_identity_columns() {
        let live = row(vec![
            ("namespace", SqlValue::Text("local".into())),
            ("deleted_at", SqlValue::Null),
        ]);
        assert_eq!(
            atom_vector_identity(&live).expect("valid live row"),
            ("local".into(), true)
        );
        let deleted = row(vec![
            ("namespace", SqlValue::Text("local".into())),
            ("deleted_at", SqlValue::Integer(1)),
        ]);
        assert_eq!(
            atom_vector_identity(&deleted).expect("valid tombstone"),
            ("local".into(), false)
        );
        for invalid in [
            row(vec![("deleted_at", SqlValue::Null)]),
            row(vec![
                ("namespace", SqlValue::Integer(1)),
                ("deleted_at", SqlValue::Null),
            ]),
            row(vec![("namespace", SqlValue::Text("local".into()))]),
            row(vec![
                ("namespace", SqlValue::Text("local".into())),
                ("deleted_at", SqlValue::Text("bad".into())),
            ]),
        ] {
            assert!(atom_vector_identity(&invalid).is_err());
        }
    }
}
