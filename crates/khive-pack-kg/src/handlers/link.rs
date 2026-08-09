//! `link` verb handler.

use serde_json::{json, Value};

use khive_runtime::{merge_entry_metadata, LinkSpec, NamespaceToken, RuntimeError, VerbRegistry};

use super::common::{
    deser, enrich_allowlist_error, format_edge_output, parse_relation, resolve_uuid_unfiltered,
    to_json, validate_weight, LinkParams,
};
use crate::KgPack;

impl KgPack {
    pub(crate) async fn handle_link(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: LinkParams = deser(params)?;
        let verbose = p.verbose.unwrap_or(false);

        if let Some(entries) = p.links {
            let attempted = entries.len();
            if attempted > 1000 {
                return Err(RuntimeError::InvalidInput(
                    "bulk link limited to 1000 entries per request".into(),
                ));
            }
            let atomic = p.atomic.unwrap_or(true);
            if atomic {
                let mut specs = Vec::with_capacity(attempted);
                let mut seen = std::collections::HashSet::new();
                let mut skipped = 0usize;
                for entry in entries {
                    let source =
                        resolve_uuid_unfiltered(&entry.source_id, &self.runtime, token).await?;
                    let target =
                        resolve_uuid_unfiltered(&entry.target_id, &self.runtime, token).await?;
                    let relation = parse_relation(&entry.relation)?;
                    // Canonicalize only the duplicate-detection key. Runtime validation
                    // must see caller order so a rejected symmetric relation reports the
                    // legal set for the requested ordered pair; `build_edge` canonicalizes
                    // the accepted edge before persistence.
                    let (key_source, key_target) = if relation.is_symmetric() && target < source {
                        (target, source)
                    } else {
                        (source, target)
                    };
                    let key = format!("{key_source}::{key_target}::{}", relation.as_str());
                    if !seen.insert(key) {
                        skipped += 1;
                        continue;
                    }
                    let weight = validate_weight(entry.weight)?;
                    let metadata = merge_entry_metadata(entry.metadata, entry.dependency_kind)?;
                    specs.push(LinkSpec {
                        namespace: Some(token.namespace().as_str().to_owned()),
                        source_id: source,
                        target_id: target,
                        relation,
                        weight,
                        metadata,
                    });
                }
                registry
                    .validate_link_hooks(&self.runtime, token, &specs)
                    .await?;
                let outcomes = self.runtime.link_many_with_outcomes(token, specs).await?;
                let created = outcomes.iter().filter(|outcome| outcome.created).count();
                let reused = outcomes.len() - created;
                let mut resp = serde_json::json!({
                    "attempted": attempted,
                    "created": created,
                    "reused": reused,
                    "skipped": skipped,
                    "failed": 0,
                });
                if verbose {
                    resp["edges"] = serde_json::to_value(
                        outcomes
                            .iter()
                            .map(|outcome| &outcome.edge)
                            .collect::<Vec<_>>(),
                    )
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
                }
                return to_json(&resp);
            } else {
                let mut results: Vec<Value> = Vec::new();
                let mut error_list: Vec<Value> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                let mut skipped = 0usize;
                let mut created = 0usize;
                let mut reused = 0usize;
                for (idx, entry) in entries.into_iter().enumerate() {
                    let source =
                        match resolve_uuid_unfiltered(&entry.source_id, &self.runtime, token).await
                        {
                            Ok(id) => id,
                            Err(e) => {
                                error_list.push(json!({"index": idx, "error": format!("{e}")}));
                                continue;
                            }
                        };
                    let target =
                        match resolve_uuid_unfiltered(&entry.target_id, &self.runtime, token).await
                        {
                            Ok(id) => id,
                            Err(e) => {
                                error_list.push(json!({"index": idx, "error": format!("{e}")}));
                                continue;
                            }
                        };
                    let relation = match parse_relation(&entry.relation) {
                        Ok(r) => r,
                        Err(e) => {
                            error_list.push(json!({"index": idx, "error": format!("{e}")}));
                            continue;
                        }
                    };
                    // Keep caller order for validation/diagnostics; only the dedup key
                    // needs UUID-canonical endpoints. `link` canonicalizes on success.
                    let (key_source, key_target) = if relation.is_symmetric() && target < source {
                        (target, source)
                    } else {
                        (source, target)
                    };
                    let key = format!("{key_source}::{key_target}::{}", relation.as_str());
                    if !seen.insert(key) {
                        skipped += 1;
                        continue;
                    }
                    let weight = match validate_weight(entry.weight) {
                        Ok(w) => w,
                        Err(e) => {
                            error_list.push(json!({"index": idx, "error": format!("{e}")}));
                            continue;
                        }
                    };
                    let metadata = match merge_entry_metadata(entry.metadata, entry.dependency_kind)
                    {
                        Ok(m) => m,
                        Err(e) => {
                            error_list.push(json!({"index": idx, "error": format!("{e}")}));
                            continue;
                        }
                    };
                    let spec = LinkSpec {
                        namespace: Some(token.namespace().as_str().to_owned()),
                        source_id: source,
                        target_id: target,
                        relation,
                        weight,
                        metadata: metadata.clone(),
                    };
                    if let Err(e) = registry
                        .validate_link_hooks(&self.runtime, token, std::slice::from_ref(&spec))
                        .await
                    {
                        error_list.push(json!({"index": idx, "error": format!("{e}")}));
                        continue;
                    }
                    match self
                        .runtime
                        .link_with_outcome(token, source, target, relation, weight, metadata)
                        .await
                    {
                        Ok(outcome) => {
                            if outcome.created {
                                created += 1;
                            } else {
                                reused += 1;
                            }
                            results.push(to_json(&outcome.edge)?);
                        }
                        Err(e) => error_list.push(json!({"index": idx, "error": format!("{e}")})),
                    }
                }
                let mut resp = serde_json::json!({
                    "attempted": attempted,
                    "created": created,
                    "reused": reused,
                    "skipped": skipped,
                    "failed": error_list.len(),
                    "errors": error_list,
                });
                if verbose {
                    resp["edges"] = serde_json::Value::Array(results);
                }
                return to_json(&resp);
            }
        }

        let source_id_str = p.source_id.ok_or_else(|| {
            RuntimeError::InvalidInput("link requires source_id (or links for bulk)".into())
        })?;
        let target_id_str = p.target_id.ok_or_else(|| {
            RuntimeError::InvalidInput("link requires target_id (or links for bulk)".into())
        })?;
        let relation_str = p.relation.ok_or_else(|| {
            RuntimeError::InvalidInput("link requires relation (or links for bulk)".into())
        })?;
        let source = resolve_uuid_unfiltered(&source_id_str, &self.runtime, token).await?;
        let target = resolve_uuid_unfiltered(&target_id_str, &self.runtime, token).await?;
        let weight = validate_weight(p.weight)?;
        let relation = parse_relation(&relation_str)?;
        let metadata = merge_entry_metadata(p.metadata, p.dependency_kind)?;
        let spec = LinkSpec {
            namespace: Some(token.namespace().as_str().to_owned()),
            source_id: source,
            target_id: target,
            relation,
            weight,
            metadata: metadata.clone(),
        };
        registry
            .validate_link_hooks(&self.runtime, token, std::slice::from_ref(&spec))
            .await?;

        let outcome = match self
            .runtime
            .link_with_outcome(token, source, target, relation, weight, metadata)
            .await
        {
            Ok(e) => e,
            Err(RuntimeError::InvalidInput(ref msg))
                if msg.contains("not in the base endpoint allowlist") =>
            {
                let enriched =
                    enrich_allowlist_error(msg, &self.runtime, token, source, target, relation)
                        .await;
                return Err(RuntimeError::InvalidInput(enriched));
            }
            Err(e) => return Err(e),
        };
        let mut raw = to_json(&outcome.edge)?;
        if let Some(obj) = raw.as_object_mut() {
            obj.insert("created".to_string(), json!(outcome.created));
            obj.insert("reused".to_string(), json!(!outcome.created));
        }
        if relation.is_symmetric() {
            if let Some(obj) = raw.as_object_mut() {
                obj.insert("source_id".to_string(), json!(source.to_string()));
                obj.insert("target_id".to_string(), json!(target.to_string()));
            }
        }
        Ok(format_edge_output(raw, verbose))
    }
}
