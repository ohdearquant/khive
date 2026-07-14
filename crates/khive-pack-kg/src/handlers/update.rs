//! `update` and `delete` verb handlers.

use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{
    EdgePatch, EntityPatch, NamespaceToken, NotePatch, RuntimeError, VerbRegistry,
};

use super::common::{
    description_patch, deser, immutable_event_error, normalize_entity_timestamps,
    optional_string_patch, parse_relation, resolve_kind_spec, resolve_uuid_unfiltered,
    resolve_uuid_unfiltered_including_deleted, string_value, to_json, DeleteParams, KindSpec,
    UpdateParams,
};
use crate::KgPack;

// Field applicability guard, authoritative field sets per substrate — see
// docs/api/note-crud-fields.md#reject_inapplicable_fields-handlersupdaters. MUST be updated
// whenever UpdateParams or a patch struct changes.
fn reject_inapplicable_fields(spec: &KindSpec, p: &UpdateParams) -> Result<(), RuntimeError> {
    let (bad_field, valid): (Option<&str>, &str) = match spec {
        KindSpec::Entity { .. } => {
            let bad = if p.content.is_some() {
                Some("content")
            } else if p.salience.is_some() {
                Some("salience")
            } else if p.decay_factor.is_some() {
                Some("decay_factor")
            } else if p.relation.is_some() {
                Some("relation")
            } else if p.weight.is_some() {
                Some("weight")
            } else {
                None
            };
            (bad, "name, description, tags, properties")
        }
        KindSpec::Note { .. } => {
            let bad = if p.description.is_some() {
                Some("description")
            } else if p.tags.is_some() {
                Some("tags")
            } else if p.relation.is_some() {
                Some("relation")
            } else if p.weight.is_some() {
                Some("weight")
            } else {
                None
            };
            (bad, "name, content, salience, decay_factor, properties")
        }
        KindSpec::Edge => {
            let bad = if p.name.is_some() {
                Some("name")
            } else if p.description.is_some() {
                Some("description")
            } else if p.content.is_some() {
                Some("content")
            } else if p.tags.is_some() {
                Some("tags")
            } else if p.salience.is_some() {
                Some("salience")
            } else if p.decay_factor.is_some() {
                Some("decay_factor")
            } else {
                None
            };
            (bad, "relation, weight, properties")
        }
        // Event/Proposal are rejected by the match arms below; no field check needed.
        KindSpec::Event | KindSpec::Proposal => return Ok(()),
    };
    if let Some(field) = bad_field {
        let substrate = match spec {
            KindSpec::Entity { .. } => "an entity",
            KindSpec::Note { .. } => "a note",
            KindSpec::Edge => "an edge",
            _ => unreachable!(),
        };
        return Err(RuntimeError::InvalidInput(format!(
            "field '{field}' is not valid for {substrate}; valid fields: {valid}"
        )));
    }
    Ok(())
}

impl KgPack {
    pub(crate) async fn infer_kind_from_uuid(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        id_str: &str,
    ) -> Result<KindSpec, RuntimeError> {
        use khive_runtime::Resolved;
        // PR-A1: by-ID substrate inference must NOT gate on caller namespace.
        // UUID v4 is globally unique — resolve without visible-set or primary-ns check.
        match self.runtime.resolve_by_id(token, id).await? {
            Some(Resolved::Entity(_)) => Ok(KindSpec::Entity { specific: None }),
            Some(Resolved::Note(_)) => Ok(KindSpec::Note { specific: None }),
            _ => {
                if self.runtime.get_edge(token, id).await?.is_some() {
                    Ok(KindSpec::Edge)
                } else {
                    Err(RuntimeError::NotFound(format!("not found: {id_str}")))
                }
            }
        }
    }

    async fn infer_kind_from_uuid_including_deleted(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        id_str: &str,
    ) -> Result<KindSpec, RuntimeError> {
        use khive_runtime::Resolved;
        // PR-A1: hard-delete path must also locate foreign records.
        match self
            .runtime
            .resolve_by_id_including_deleted(token, id)
            .await?
        {
            Some(Resolved::Entity(_)) => Ok(KindSpec::Entity { specific: None }),
            Some(Resolved::Note(_)) => Ok(KindSpec::Note { specific: None }),
            _ => {
                if self
                    .runtime
                    .get_edge_including_deleted(token, id)
                    .await?
                    .is_some()
                {
                    Ok(KindSpec::Edge)
                } else {
                    Err(RuntimeError::NotFound(format!("not found: {id_str}")))
                }
            }
        }
    }

    pub(crate) async fn handle_update(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: UpdateParams = deser(params)?;
        if p.entity_kind.is_some() {
            return Err(RuntimeError::InvalidInput(
                "entity_kind is immutable; to change kind, delete then re-create the entity, or use merge() if this is a deduplication correction".into(),
            ));
        }
        let explicit_spec: Option<KindSpec> = if let Some(k) = p.kind.as_deref() {
            Some(resolve_kind_spec(k, registry)?)
        } else {
            None
        };
        // By-ID resolution (including the hex-prefix form) is namespace-agnostic
        // (ADR-007 Rev 6 / #391 §3) — the Gate is the authz seam, not this lookup.
        let id = resolve_uuid_unfiltered(&p.id, &self.runtime, token).await?;
        let spec: KindSpec = match explicit_spec {
            Some(s) => s,
            None => match self.infer_kind_from_uuid(token, id, &p.id).await {
                Ok(s) => s,
                Err(RuntimeError::NotFound(_)) => {
                    // Check if a pack resolver claims this UUID; if so, update is deferred.
                    for (_pack_name, resolver) in registry.resolvers() {
                        if resolver.resolve_by_id(id).await?.is_some() {
                            return Err(RuntimeError::InvalidInput(
                                "update of pack-private records is not yet supported; \
                                 use the pack's own verbs (e.g. knowledge.upsert_atoms, \
                                 knowledge.upsert_domains, knowledge.edit)"
                                    .into(),
                            ));
                        }
                    }
                    return Err(RuntimeError::NotFound(format!("not found: {}", p.id)));
                }
                Err(e) => return Err(e),
            },
        };

        reject_inapplicable_fields(&spec, &p)?;

        match spec {
            KindSpec::Entity { specific } => {
                let entity = self.runtime.get_entity(token, id).await?;
                if specific.as_ref().is_some_and(|k| entity.kind != *k) {
                    return Err(RuntimeError::NotFound(format!("entity {}", p.id)));
                }
                let patch = EntityPatch {
                    name: string_value(p.name, "name")?,
                    description: description_patch(p.description)?,
                    properties: p.properties,
                    tags: p.tags,
                };
                Ok(normalize_entity_timestamps(to_json(
                    &self.runtime.update_entity(token, id, patch).await?,
                )?))
            }
            KindSpec::Edge => {
                let relation = p.relation.as_deref().map(parse_relation).transpose()?;
                let patch = EdgePatch {
                    relation,
                    weight: p.weight,
                    properties: p.properties,
                };
                to_json(&self.runtime.update_edge(token, id, patch).await?)
            }
            KindSpec::Note { specific } => {
                let note = self
                    .runtime
                    .notes(token)?
                    .get_note(id)
                    .await
                    .map_err(RuntimeError::Storage)?;
                if note
                    .as_ref()
                    .is_none_or(|n| specific.as_ref().is_some_and(|k| n.kind != *k))
                {
                    return Err(RuntimeError::NotFound(format!("note {}", p.id)));
                }
                let patch = NotePatch::new(
                    optional_string_patch(p.name, "name")?,
                    p.content,
                    p.salience,
                    p.decay_factor,
                    p.properties,
                );
                Ok(normalize_entity_timestamps(to_json(
                    &self.runtime.update_note(token, id, patch).await?,
                )?))
            }
            KindSpec::Event => Err(immutable_event_error()),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "proposal events are immutable — use `withdraw` to rescind a proposal".into(),
            )),
        }
    }

    pub(crate) async fn handle_delete(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: DeleteParams = deser(params)?;
        let hard = p.hard.unwrap_or(false);
        let explicit_spec: Option<KindSpec> = if let Some(k) = p.kind.as_deref() {
            Some(resolve_kind_spec(k, registry)?)
        } else {
            None
        };
        // By-ID resolution (including the hex-prefix form) is namespace-agnostic
        // (ADR-007 Rev 6 / #391 §3) — the Gate is the authz seam, not this lookup.
        let id = if hard {
            resolve_uuid_unfiltered_including_deleted(&p.id, &self.runtime, token).await?
        } else {
            resolve_uuid_unfiltered(&p.id, &self.runtime, token).await?
        };
        let spec: Option<KindSpec> = match explicit_spec {
            Some(s) => Some(s),
            None => {
                let infer_result = if hard {
                    self.infer_kind_from_uuid_including_deleted(token, id, &p.id)
                        .await
                } else {
                    self.infer_kind_from_uuid(token, id, &p.id).await
                };
                match infer_result {
                    Ok(s) => Some(s),
                    Err(RuntimeError::NotFound(_)) => {
                        // Second-chance: probe pack resolvers for pack-private records.
                        for (_pack_name, resolver) in registry.resolvers() {
                            let maybe = if hard {
                                resolver.resolve_by_id_including_deleted(id).await?
                            } else {
                                resolver.resolve_by_id(id).await?
                            };
                            if maybe.is_some() {
                                return resolver.delete_by_id(id, hard).await;
                            }
                        }
                        return Err(RuntimeError::NotFound(format!("not found: {}", p.id)));
                    }
                    Err(e) => return Err(e),
                }
            }
        };

        // Unwrap is safe: None branch above always returns early.
        let spec = spec.unwrap();

        match spec {
            KindSpec::Entity { specific } => {
                if let Some(ref expected) = specific {
                    let entity = if hard {
                        self.runtime
                            .get_entity_including_deleted(token, id)
                            .await?
                            .ok_or_else(|| {
                                RuntimeError::NotFound(format!("{} {}", expected, p.id))
                            })?
                    } else {
                        self.runtime.get_entity(token, id).await?
                    };
                    if entity.kind != *expected {
                        return Err(RuntimeError::NotFound(format!("{} {}", expected, p.id)));
                    }
                }
                let deleted = self.runtime.delete_entity(token, id, hard).await?;
                if !deleted {
                    return Err(RuntimeError::NotFound(format!("entity {}", p.id)));
                }
                to_json(&serde_json::json!({ "deleted": deleted, "id": p.id, "kind": p.kind }))
            }
            KindSpec::Note { specific } => {
                if let Some(ref expected) = specific {
                    let note = if hard {
                        self.runtime
                            .get_note_including_deleted(token, id)
                            .await?
                            .ok_or_else(|| {
                                RuntimeError::NotFound(format!("{} {}", expected, p.id))
                            })?
                    } else {
                        self.runtime
                            .notes(token)?
                            .get_note(id)
                            .await
                            .map_err(RuntimeError::Storage)?
                            .ok_or_else(|| {
                                RuntimeError::NotFound(format!("{} {}", expected, p.id))
                            })?
                    };
                    if note.kind != *expected {
                        return Err(RuntimeError::NotFound(format!("{} {}", expected, p.id)));
                    }
                }
                let deleted = self.runtime.delete_note(token, id, hard).await?;
                if !deleted {
                    return Err(RuntimeError::NotFound(format!("note {}", p.id)));
                }
                to_json(&serde_json::json!({ "deleted": deleted, "id": p.id, "kind": p.kind }))
            }
            KindSpec::Edge => {
                let deleted = self.runtime.delete_edge(token, id, hard).await?;
                to_json(&serde_json::json!({ "deleted": deleted, "id": p.id, "kind": "edge" }))
            }
            KindSpec::Event => Err(immutable_event_error()),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "proposal events are immutable — use `withdraw` to rescind a proposal".into(),
            )),
        }
    }
}
