//! `update` and `delete` verb handlers.

use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{
    EdgePatch, EntityPatch, NamespaceToken, NotePatch, RuntimeError, VerbRegistry,
};

use super::common::{
    description_patch, deser, immutable_event_error, normalize_entity_timestamps,
    optional_string_patch, parse_relation, resolve_kind_spec, resolve_uuid_async,
    resolve_uuid_including_deleted, string_value, to_json, DeleteParams, KindSpec, UpdateParams,
};
use crate::KgPack;

impl KgPack {
    pub(crate) async fn infer_kind_from_uuid(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        id_str: &str,
    ) -> Result<KindSpec, RuntimeError> {
        use khive_runtime::Resolved;
        match self.runtime.resolve(token, id).await? {
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
        match self.runtime.resolve_including_deleted(token, id).await? {
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
        let id = resolve_uuid_async(&p.id, &self.runtime, token).await?;
        let spec: KindSpec = match explicit_spec {
            Some(s) => s,
            None => self.infer_kind_from_uuid(token, id, &p.id).await?,
        };

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
        let id = if hard {
            resolve_uuid_including_deleted(&p.id, &self.runtime, token).await?
        } else {
            resolve_uuid_async(&p.id, &self.runtime, token).await?
        };
        let spec: KindSpec = match explicit_spec {
            Some(s) => s,
            None => {
                if hard {
                    self.infer_kind_from_uuid_including_deleted(token, id, &p.id)
                        .await?
                } else {
                    self.infer_kind_from_uuid(token, id, &p.id).await?
                }
            }
        };

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
