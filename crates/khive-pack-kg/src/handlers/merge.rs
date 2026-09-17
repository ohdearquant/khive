//! `merge` verb handler.

use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{
    entity_merge_guard_compared_values, entity_merge_guard_error,
    entity_merge_guard_refusal_message, validate_entity_merge_floor, KhiveRuntime, NamespaceToken,
    RuntimeError, VerbRegistry,
};

use super::common::{
    deser, ensure_entity_kind, ensure_note_kind, immutable_event_error, pack_private_record_error,
    parse_content_strategy, parse_entity_policy, resolve_kind_spec, resolve_uuid_unfiltered,
    to_json, KindSpec, MergeParams,
};
use crate::KgPack;

/// Substrate word for a resolved kind, for a refusal that has to name both sides.
fn substrate_name(spec: &KindSpec) -> &'static str {
    match spec {
        KindSpec::Entity { .. } => "entity",
        KindSpec::Note { .. } => "note",
        KindSpec::Edge => "edge",
        KindSpec::Event => "event",
        KindSpec::Proposal => "proposal",
    }
}

/// Turn a merge operand's `NotFound` into a directing refusal when a pack's
/// resolver owns the id.
async fn diagnose_private_merge<T>(
    result: Result<T, RuntimeError>,
    id: Uuid,
    registry: &VerbRegistry,
) -> Result<T, RuntimeError> {
    match result {
        Err(original @ RuntimeError::NotFound(_)) => {
            for (pack_name, resolver) in registry.resolvers() {
                if resolver.resolve_by_id(id).await?.is_some() {
                    return Err(pack_private_record_error(
                        "merge of pack-private records is not supported",
                        pack_name,
                        resolver.as_ref(),
                    ));
                }
            }
            Err(original)
        }
        other => other,
    }
}

/// The refusal for an omitted-kind operand that resolves to nothing.
///
/// There is no substrate to name, so the refusal is the id alone, with one
/// exception that is a fact about a record rather than a default: an entity an
/// earlier merge consumed. The runtime's entity lookup names the id it was merged
/// into, which is the caller's next step, and omitting `kind` must not cost the
/// caller a pointer that naming `kind="entity"` would have kept.
async fn unresolvable_merge_operand(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> RuntimeError {
    match runtime.get_entity_including_deleted(token, id).await {
        Ok(Some(tombstone)) if tombstone.merged_into.is_some() => {
            match runtime.get_entity(token, id).await {
                Err(redirect) => redirect,
                Ok(_) => RuntimeError::NotFound(id.to_string()),
            }
        }
        Ok(_) => RuntimeError::NotFound(id.to_string()),
        Err(error) => error,
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
        let explicit_spec = match p.kind.as_deref() {
            Some(raw_kind) => Some(resolve_kind_spec(raw_kind, registry)?),
            None => None,
        };
        let policy = parse_entity_policy(p.strategy.as_deref().unwrap_or("prefer_into"))?;
        let content_strategy =
            parse_content_strategy(p.content_strategy.as_deref().unwrap_or("append"))?;
        // An omitted `kind` resolves the substrate from `into_id`, which is what
        // the parameter has always documented. It used to default to "entity",
        // so a caller merging two notes without the hint was told "not found:
        // entity <id>" about records that exist: a refusal naming a substrate
        // the caller never chose, for a default the caller never saw.
        //
        // The inference reads records, so it runs after every argument the call
        // can be refused on without a read. A bad `strategy` was a pre-read error
        // before this change and stays one: putting a lookup in front of it would
        // answer a malformed call with a record's problem.
        let spec = match explicit_spec {
            Some(spec) => spec,
            None => {
                let into_spec = diagnose_private_merge(
                    self.infer_kind_from_uuid(token, into_id, &p.into_id).await,
                    into_id,
                    registry,
                )
                .await;
                let from_spec = diagnose_private_merge(
                    self.infer_kind_from_uuid(token, from_id, &p.from_id).await,
                    from_id,
                    registry,
                )
                .await;
                match (into_spec, from_spec) {
                    (Ok(into_spec), Ok(from_spec)) => {
                        // Inferring the substrate makes a disagreement reachable
                        // without the caller having typed anything: under the old
                        // default both ids were read as entities, so a note on
                        // either side failed as a missing entity. Name both sides
                        // rather than letting the survivor's substrate turn the
                        // other one into a lookup failure.
                        if substrate_name(&into_spec) != substrate_name(&from_spec) {
                            return Err(RuntimeError::InvalidInput(format!(
                                "cannot merge across substrates: into_id {into_id} resolves as \
                                 {}, from_id {from_id} resolves as {}; merge joins two records of \
                                 one substrate, so pass the pair you meant or name the kind \
                                 explicitly",
                                substrate_name(&into_spec),
                                substrate_name(&from_spec)
                            )));
                        }
                        into_spec
                    }
                    // An id that resolves to nothing has no substrate, so the
                    // refusal names none (an entity a merge consumed still names
                    // the id it was merged into; see `unresolvable_merge_operand`).
                    // The historical default answered "entity <id>" here, which is the default's fingerprint rather than a
                    // fact about a record, and it sent a caller merging two notes
                    // looking for entities that never existed.
                    //
                    // `into_id` is still reported before `from_id` is considered,
                    // which `khive-pack-knowledge`'s issue-558 arms pin: a caller
                    // whose first id is wrong hears about that one. The ids are the
                    // resolved uuids because the parameter accepts a hex prefix,
                    // and the id the lookup used is the fact worth naming.
                    (Err(RuntimeError::NotFound(_)), _) => {
                        return Err(unresolvable_merge_operand(&self.runtime, token, into_id).await)
                    }
                    (_, Err(RuntimeError::NotFound(_))) => {
                        return Err(unresolvable_merge_operand(&self.runtime, token, from_id).await)
                    }
                    (Err(error), _) | (_, Err(error)) => return Err(error),
                }
            }
        };
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
