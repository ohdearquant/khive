//! `merge` verb handler.

use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{
    entity_merge_guard_error, validate_entity_merge_floor, NamespaceToken, RuntimeError,
    VerbRegistry,
};

use super::common::{
    deser, ensure_entity_kind, ensure_note_kind, immutable_event_error, parse_content_strategy,
    parse_entity_policy, resolve_kind_spec, resolve_uuid_unfiltered, to_json, KindSpec,
    MergeParams,
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
        // An omitted `kind` resolves the substrate from `into_id`, which is what
        // the parameter has always documented. It used to default to "entity",
        // so a caller merging two notes without the hint was told "not found:
        // entity <id>" about records that exist: a refusal naming a substrate
        // the caller never chose, for a default the caller never saw.
        let spec = match p.kind.as_deref() {
            Some(raw_kind) => resolve_kind_spec(raw_kind, registry)?,
            None => {
                let into_spec = diagnose_private_merge(
                    self.infer_kind_from_uuid(token, into_id, &p.into_id).await,
                    into_id,
                    registry,
                )
                .await?;
                let from_spec = diagnose_private_merge(
                    self.infer_kind_from_uuid(token, from_id, &p.from_id).await,
                    from_id,
                    registry,
                )
                .await?;
                // Inferring the substrate makes a disagreement reachable without
                // the caller having typed anything: under the old default both
                // ids were read as entities, so a note on either side failed as
                // a missing entity. Name both sides rather than letting the
                // survivor's substrate turn the other one into a lookup failure.
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
        };
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
                    validate_entity_merge_floor(&into_entity, &from_entity)
                        .map_err(entity_merge_guard_error)?;
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
        Ok(response)
    }
}
