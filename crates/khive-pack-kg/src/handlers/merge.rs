//! `merge` verb handler.

use serde_json::Value;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};

use super::common::{
    deser, ensure_entity_kind, ensure_note_kind, immutable_event_error, parse_content_strategy,
    parse_entity_policy, resolve_kind_spec, resolve_uuid_unfiltered, to_json, KindSpec,
    MergeParams,
};
use crate::KgPack;

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
        let reason = p.reason.clone();

        let summary = match spec {
            KindSpec::Entity { specific } => {
                ensure_entity_kind(&self.runtime, token, into_id, specific.as_deref()).await?;
                ensure_entity_kind(&self.runtime, token, from_id, specific.as_deref()).await?;
                let into_entity = self.runtime.get_entity(token, into_id).await?;
                let from_entity = self.runtime.get_entity(token, from_id).await?;
                if into_entity.kind != from_entity.kind {
                    return Err(RuntimeError::InvalidInput(format!(
                        "cannot merge entities of different kinds: into={} ({}), from={} ({})",
                        into_id, into_entity.kind, from_id, from_entity.kind
                    )));
                }
                self.runtime
                    .merge_entity_with_reason(
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
            KindSpec::Note { specific } => {
                ensure_note_kind(&self.runtime, token, into_id, specific.as_deref()).await?;
                ensure_note_kind(&self.runtime, token, from_id, specific.as_deref()).await?;
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
        to_json(&summary)
    }
}
