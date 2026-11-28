//! `resolve` verb handler (unified-verb draft ADR, Slice 1). Thin and read-only — see
//! `docs/api/resolve-verb.md#handler-shape`.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    NamespaceToken, ReferenceResolution, RuntimeError, VerbRegistry, EXACT_NAME_CONFIDENCE,
};

use super::common::{deser, resolve_kind_spec, KindSpec};
use super::params::ResolveParams;
use super::redirect::followed_entity;
use crate::KgPack;

const DEFAULT_LIMIT: u32 = 5;
const MAX_LIMIT: u32 = 20;

impl KgPack {
    pub(crate) async fn handle_resolve(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: ResolveParams = deser(params.clone())?;
        if p.refs.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "resolve requires a non-empty `refs` array".into(),
            ));
        }
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let ring = registry.reference_ring();

        // #849: bare "entity" means no kind filter, not a literal entities.kind value.
        // See docs/api/resolve-verb.md#handler-shape.
        let (entity_kind, entity_type) = match &p.kind {
            Some(raw) => match resolve_kind_spec(raw, registry)? {
                KindSpec::Entity {
                    specific,
                    entity_type,
                } => (specific, entity_type),
                _ => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "resolve only supports entity kinds; kind={raw:?} is not an entity kind"
                    )))
                }
            },
            None => (None, None),
        };

        let mut results = Vec::with_capacity(p.refs.len());
        for (index, nl_ref) in p.refs.iter().enumerate() {
            let mut resolution = khive_runtime::resolve_reference_with_entity_type(
                &self.runtime,
                ring,
                token,
                nl_ref,
                limit,
                entity_kind.as_deref(),
                entity_type.as_deref(),
            )
            .await?;
            let mut redirected_from = Vec::new();
            let mut matched_alias = None;
            if matches!(&resolution, ReferenceResolution::NotFound) {
                let id = if let Ok(id) = Uuid::parse_str(nl_ref.trim()) {
                    Some(id)
                } else if nl_ref.trim().len() >= 8
                    && nl_ref.trim().chars().all(|ch| ch.is_ascii_hexdigit())
                {
                    registry
                        .resolve_kg_read_prefix(&self.runtime, token, nl_ref.trim(), true)
                        .await?
                } else {
                    None
                };
                if let Some(id) = id {
                    if let Some((entity, chain)) =
                        followed_entity(&self.runtime, registry, token, id).await?
                    {
                        if !chain.is_empty() {
                            resolution = ReferenceResolution::Resolved {
                                id: entity.id,
                                confidence: 1.0,
                            };
                            redirected_from = chain;
                        }
                    }
                }
            } else if let ReferenceResolution::Resolved { id, confidence } = &resolution {
                resolution = match followed_entity(&self.runtime, registry, token, *id).await? {
                    Some((entity, chain)) => {
                        // ADR-221 section 2: only a stage-3 hit can come
                        // through an alias, and it names the entity it
                        // matched, never a redirect target.
                        let trimmed = nl_ref.trim();
                        if *confidence == EXACT_NAME_CONFIDENCE
                            && chain.is_empty()
                            && entity.name != trimmed
                            && entity.aliases.iter().any(|held| held.alias == trimmed)
                        {
                            matched_alias = Some(trimmed.to_string());
                        }
                        redirected_from = chain;
                        ReferenceResolution::Resolved {
                            id: entity.id,
                            confidence: *confidence,
                        }
                    }
                    None => ReferenceResolution::NotFound,
                };
            }
            let mut result = render_resolution(nl_ref, resolution);
            if let Some(alias) = matched_alias {
                result["matched_alias"] = json!(alias);
            }
            if !redirected_from.is_empty() {
                let effective_id = result["id"]
                    .as_str()
                    .and_then(|id| Uuid::parse_str(id).ok())
                    .ok_or_else(|| {
                        RuntimeError::Internal("resolved redirect has no entity id".into())
                    })?;
                let mut effective_args = params.clone();
                effective_args["refs"][index] = json!(effective_id.to_string());
                registry
                    .authorize_effective_kg_read(token, "resolve", effective_args, effective_id)
                    .await?;
                result["redirected_from"] = json!(redirected_from);
            }
            results.push(result);
        }

        Ok(json!({ "results": results }))
    }
}

fn render_resolution(nl_ref: &str, resolution: ReferenceResolution) -> Value {
    match resolution {
        ReferenceResolution::Resolved { id, confidence } => json!({
            "ref": nl_ref,
            "status": "resolved",
            "id": id.to_string(),
            "confidence": confidence,
        }),
        ReferenceResolution::Ambiguous { candidates } => json!({
            "ref": nl_ref,
            "status": "ambiguous",
            "candidates": candidates
                .into_iter()
                .map(|c| json!({
                    "id": c.id.to_string(),
                    "name": c.name,
                    "score": c.score,
                }))
                .collect::<Vec<_>>(),
        }),
        ReferenceResolution::NotFound => json!({
            "ref": nl_ref,
            "status": "not_found",
        }),
    }
}
