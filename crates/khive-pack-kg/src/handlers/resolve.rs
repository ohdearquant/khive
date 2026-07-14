//! `resolve` verb handler (unified-verb draft ADR, Slice 1). Thin and read-only — see
//! `docs/handlers-common.md#resolve-handler-handlersresolveers`.

use serde_json::{json, Value};

use khive_runtime::{NamespaceToken, ReferenceResolution, RuntimeError, VerbRegistry};

use super::common::{deser, resolve_kind_spec, KindSpec};
use super::params::ResolveParams;
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
        let p: ResolveParams = deser(params)?;
        if p.refs.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "resolve requires a non-empty `refs` array".into(),
            ));
        }
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let ring = registry.reference_ring();

        // #849: bare "entity" means no kind filter, not a literal entities.kind value.
        // See docs/handlers-common.md#resolve-handler-handlersresolveers.
        let entity_kind = match &p.kind {
            Some(raw) => match resolve_kind_spec(raw, registry)? {
                KindSpec::Entity { specific } => specific,
                _ => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "resolve only supports entity kinds; kind={raw:?} is not an entity kind"
                    )))
                }
            },
            None => None,
        };

        let mut results = Vec::with_capacity(p.refs.len());
        for nl_ref in &p.refs {
            let resolution = khive_runtime::resolve_reference(
                &self.runtime,
                ring,
                token,
                nl_ref,
                limit,
                entity_kind.as_deref(),
            )
            .await?;
            results.push(render_resolution(nl_ref, resolution));
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
