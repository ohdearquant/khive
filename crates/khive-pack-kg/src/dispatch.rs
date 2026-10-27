//! PackRuntime impl for KgPack plus inventory self-registration.

use async_trait::async_trait;
use serde_json::Value;

use std::sync::Arc;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    EntityTypeValidatorFn, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry,
};
use khive_storage::{PhaseCancelledPayload, PhaseCompletedPayload, PhaseStartedPayload};
use khive_types::EventKind;

use crate::handler_defs::handle_verbs;
use crate::pack::KgPack;

struct KgPackFactory;

impl khive_runtime::PackFactory for KgPackFactory {
    khive_runtime::pack_factory_metadata!(KgPack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(KgPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&KgPackFactory) }

#[async_trait]
impl PackRuntime for KgPack {
    khive_runtime::pack_runtime_metadata!();

    async fn dispatch_search_intercepted(
        &self,
        params: serde_json::Value,
        registry: &khive_runtime::pack::VerbRegistry,
        token: &khive_runtime::NamespaceToken,
    ) -> Result<
        khive_runtime::pack::InterceptedDispatchResult<
            Option<(khive_storage::note::NoteStatusSelector, u64)>,
        >,
        khive_runtime::RuntimeError,
    > {
        self.handle_search_with_metadata(token, params, registry)
            .await
    }

    async fn warm(&self) {
        // ADR-103 Amendment 1 Part 2: mint an attribution token for daemon-startup
        // telemetry (see docs/api/pack-lifecycle.md#warm-telemetry-adr-103-amendment-1-part-2); a mint failure
        // only drops telemetry, warmup still runs.
        let token = match self.runtime.authorize(khive_runtime::Namespace::local()) {
            Ok(token) => Some(token),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "kg_embedder_warm: failed to mint ADR-103 phase-span attribution token; \
                     warmup proceeds without telemetry"
                );
                None
            }
        };
        if let Some(token) = &token {
            khive_runtime::emit_phase_event(
                &self.runtime,
                token,
                "kg.embedder_warm",
                EventKind::PhaseStarted,
                PhaseStartedPayload {
                    work_class: "warm".into(),
                    phase: "kg_embedder_warm".into(),
                    corpus_size: None,
                },
            )
            .await;
        }
        let phase_start = std::time::Instant::now();
        let cpu_start = khive_runtime::process_resource_usage();
        let result = self.runtime.embed("khive warmup").await;
        if let Some(token) = &token {
            let wall_us = phase_start.elapsed().as_micros() as i64;
            let cpu_us =
                khive_runtime::cpu_delta_us(cpu_start, khive_runtime::process_resource_usage());
            match &result {
                Err(e) if khive_runtime::is_benign_shutdown_cancellation(e) => {
                    khive_runtime::emit_phase_event(
                        &self.runtime,
                        token,
                        "kg.embedder_warm",
                        EventKind::PhaseCancelled,
                        PhaseCancelledPayload {
                            work_class: "warm".into(),
                            phase: "kg_embedder_warm".into(),
                            wall_us,
                            cpu_us,
                        },
                    )
                    .await;
                }
                _ => {
                    khive_runtime::emit_phase_event(
                        &self.runtime,
                        token,
                        "kg.embedder_warm",
                        EventKind::PhaseCompleted,
                        PhaseCompletedPayload {
                            work_class: "warm".into(),
                            phase: "kg_embedder_warm".into(),
                            wall_us,
                            cpu_us,
                        },
                    )
                    .await;
                }
            }
        }
        // `PackRuntime::warm` is documented infallible: errors are logged, never propagated.
        let _ = result;
    }

    fn registered_embedding_model_names(&self) -> Vec<String> {
        self.runtime.registered_embedding_model_names()
    }

    fn register_entity_type_validator_with_types(
        &self,
        _runtime: &KhiveRuntime,
        pack_entity_types: &[khive_types::EntityTypeDef],
    ) {
        // Installs on the runtime this pack OWNS (`self.runtime`, not the caller-supplied
        // one), composed over every loaded pack's ENTITY_TYPES layered on the builtin
        // registry. See docs/api/pack-lifecycle.md#entity-type-validator-installation.
        let composed = Arc::new(crate::entity_type_registry::EntityTypeRegistry::with_extra(
            pack_entity_types.iter().cloned(),
        ));
        let validator: EntityTypeValidatorFn = Arc::new(move |kind, entity_type| {
            let Some(raw) = entity_type else {
                return Ok(None);
            };
            let ek: khive_types::EntityKind = kind
                .parse()
                .map_err(|_| RuntimeError::InvalidInput(format!("unknown entity kind {kind:?}")))?;
            let resolved = composed
                .resolve(ek, Some(raw))
                .map_err(RuntimeError::from)?;
            Ok(resolved.entity_type)
        });
        self.runtime.install_entity_type_validator(validator);
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        // The `verbs` introspection verb has no namespace side-effect and is routed
        // before graph dispatch.
        if verb == "verbs" {
            return handle_verbs(params, registry).await;
        }

        if verb == "schema" {
            return crate::schema::handle_schema(params, registry);
        }

        // KG graph operations honor the NamespaceToken minted by VerbRegistry::dispatch.
        let graph_token = token;

        // Peek at `kind` for verbs that can operate on both entities and notes.
        let raw_kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let kind_is_entity_or_edge = matches!(
            raw_kind.as_str(),
            "entity"
                | "edge"
                | "concept"
                | "document"
                | "dataset"
                | "project"
                | "person"
                | "org"
                | "artifact"
                | "service"
                | "resource"
        );

        match verb {
            // Built and boxed outside this async body: the canonical apply future is large, and
            // constructed here it would grow the stack frame of every dispatch, whichever verb is
            // called.
            "apply_changeset" => {
                self.handle_apply_changeset_boxed(graph_token, params, registry)
                    .await
            }
            // Kind-discriminated: override only for entity/edge kinds.
            "create" | "list" | "search" => {
                let tok = if kind_is_entity_or_edge {
                    graph_token
                } else {
                    token
                };
                match verb {
                    "create" => self.handle_create(tok, params, registry).await,
                    "list" => self.handle_list(tok, params, registry).await,
                    _ => self.handle_search(tok, params, registry).await,
                }
            }
            // Pure graph verbs: always use graph namespace.
            "link" => self.handle_link(graph_token, params, registry).await,
            "neighbors" => self.handle_neighbors(graph_token, params, registry).await,
            "traverse" => self.handle_traverse(graph_token, params).await,
            "context" => self.handle_context(graph_token, params, registry).await,
            "query" => self.handle_query(graph_token, params).await,
            "propose" => self.handle_propose(graph_token, params, registry).await,
            "review" => self.handle_review(graph_token, params, registry).await,
            "withdraw" => self.handle_withdraw(graph_token, params).await,
            "kg.proposal_cleanup" => self.handle_proposal_cleanup(graph_token, params).await,
            "stats" => self.handle_stats(graph_token, params).await,
            "count" => self.handle_count(token, params).await,
            "stream.append" => self.handle_stream_append(token, params, registry).await,
            "stream.read" => self.handle_stream_read(token, params).await,
            "stream.stat" => self.handle_stream_stat(token, params).await,
            "stream.truncate" => self.handle_stream_truncate(token, params).await,
            "stream.batch" => self.handle_stream_batch(token, params, registry).await,
            "whoami" => self.handle_whoami(graph_token, params).await,
            "scan" => self.handle_scan(graph_token, params).await,
            "db_diagnostics" => {
                self.handle_db_diagnostics(graph_token, params, registry)
                    .await
            }
            "resolve" => self.handle_resolve(graph_token, params, registry).await,
            "merge" => self.handle_merge(graph_token, params, registry).await,
            // UUID-based: entities/edges use graph token, notes/events use caller token.
            "get" => self.handle_get(token, graph_token, params, registry).await,
            "attach" => self.handle_attach(token, params).await,
            "update" => self.handle_update(graph_token, params, registry).await,
            "delete" => self.handle_delete(graph_token, params, registry).await,
            "restore" => self.handle_restore(graph_token, params, registry).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "kg pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
