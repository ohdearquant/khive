//! `merge` verb handler.

use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{
    entity_merge_guard_compared_values, entity_merge_guard_error,
    entity_merge_guard_refusal_message, validate_entity_merge_floor, NamespaceToken, RuntimeError,
    VerbRegistry,
};

use super::common::{
    deser, ensure_entity_kind, ensure_note_kind, immutable_event_error, parse_content_strategy,
    parse_entity_policy, resolve_kind_spec, resolve_uuid_unfiltered, to_json, KindSpec,
    MergeParams,
};
use crate::KgPack;

async fn diagnose_private_merge<T>(
    result: Result<T, RuntimeError>,
    id: Uuid,
    registry: &VerbRegistry,
) -> Result<T, RuntimeError> {
    match result {
        Err(original @ RuntimeError::NotFound(_)) => {
            for (_pack_name, resolver) in registry.resolvers() {
                if resolver.resolve_by_id(id).await?.is_some() {
                    return Err(RuntimeError::InvalidInput(
                        "merge of pack-private records is not supported; \
                         use the pack's own verbs (e.g. knowledge.upsert_atoms, \
                         knowledge.upsert_domains, knowledge.edit)"
                            .into(),
                    ));
                }
            }
            Err(original)
        }
        other => other,
    }
}

impl KgPack {
    pub(crate) async fn handle_merge(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: MergeParams = deser(params)?;
        // By-ID resolution (including the hex-prefix form) is namespace-agnostic
        // (ADR-007 Rev 6 / #391 §3) — the Gate is the authz seam, not this lookup.
        let into_id = resolve_uuid_unfiltered(&p.into_id, &self.runtime, token).await?;
        let from_id = resolve_uuid_unfiltered(&p.from_id, &self.runtime, token).await?;
        let raw_kind = p.kind.as_deref().unwrap_or("entity");
        let spec = resolve_kind_spec(raw_kind, registry)?;
        let policy = parse_entity_policy(p.strategy.as_deref().unwrap_or("prefer_into"))?;
        let content_strategy =
            parse_content_strategy(p.content_strategy.as_deref().unwrap_or("append"))?;
        let dry_run = p.dry_run.unwrap_or(false);
        let force = p.force.unwrap_or(false);
        let reason = p.reason.clone();

        let summary = match spec {
            KindSpec::Entity { specific } => {
                diagnose_private_merge(
                    ensure_entity_kind(&self.runtime, token, into_id, specific.as_deref()).await,
                    into_id,
                    registry,
                )
                .await?;
                diagnose_private_merge(
                    ensure_entity_kind(&self.runtime, token, from_id, specific.as_deref()).await,
                    from_id,
                    registry,
                )
                .await?;
                let into_entity = diagnose_private_merge(
                    self.runtime.get_entity(token, into_id).await,
                    into_id,
                    registry,
                )
                .await?;
                let from_entity = diagnose_private_merge(
                    self.runtime.get_entity(token, from_id).await,
                    from_id,
                    registry,
                )
                .await?;
                if !force {
                    if let Err(guard) = validate_entity_merge_floor(&into_entity, &from_entity) {
                        // A dry run is a prediction, so the safety floor it would
                        // hit is part of what there is to predict. Returning the
                        // conflict error here instead would make `dry_run=true`
                        // fail on exactly the merges a caller has most reason to
                        // ask about, and would contradict the parameter's own
                        // contract of returning the plan without mutating.
                        if dry_run {
                            let (into_value, from_value) = entity_merge_guard_compared_values(
                                guard,
                                &into_entity,
                                &from_entity,
                            );
                            return Ok(serde_json::json!({
                                "dry_run": true,
                                "would_merge": false,
                                "refused_by": guard.as_str(),
                                "compared": guard.compared(),
                                "into_id": into_id,
                                "from_id": from_id,
                                "into_value": into_value,
                                "from_value": from_value,
                                "detail": entity_merge_guard_refusal_message(guard),
                            }));
                        }
                        return Err(entity_merge_guard_error(guard));
                    }
                }
                self.runtime
                    .merge_entity_with_reason_and_force(
                        token,
                        into_id,
                        from_id,
                        policy,
                        content_strategy,
                        dry_run,
                        reason,
                        force,
                    )
                    .await?
            }
            KindSpec::Note { specific } => {
                diagnose_private_merge(
                    ensure_note_kind(&self.runtime, token, into_id, specific.as_deref()).await,
                    into_id,
                    registry,
                )
                .await?;
                diagnose_private_merge(
                    ensure_note_kind(&self.runtime, token, from_id, specific.as_deref()).await,
                    from_id,
                    registry,
                )
                .await?;
                self.runtime
                    .merge_note_with_reason(
                        token,
                        into_id,
                        from_id,
                        policy,
                        content_strategy,
                        dry_run,
                        reason,
                    )
                    .await?
            }
            KindSpec::Edge => {
                return Err(RuntimeError::InvalidInput(
                    "merge(kind=\"edge\") is unsupported".into(),
                ))
            }
            KindSpec::Event => return Err(immutable_event_error()),
            KindSpec::Proposal => {
                return Err(RuntimeError::InvalidInput(
                    "proposal events are immutable and cannot be merged".into(),
                ))
            }
        };
        let truncated = summary.embedding_truncation.any_truncated();
        let mut response = to_json(&summary)?;
        super::create::add_embedding_truncation_warning(&mut response, truncated);
        // Every dry run answers the same question, so it answers it with the same
        // field whether the plan is a merge or a refusal. A caller that had to
        // read `would_merge` as present-or-absent would be reading a missing key
        // as a verdict, which is the reading that fails silently.
        if dry_run {
            if let Some(object) = response.as_object_mut() {
                object.insert("would_merge".to_string(), Value::Bool(true));
            }
        }
        Ok(response)
    }
}
