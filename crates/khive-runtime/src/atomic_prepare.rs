//! ADR-099 migration step 1 (cont'd) / B3 — the per-verb async prepare pass
//! for the KG-substrate v1 admissible verbs (`update`, `delete`, `link`,
//! `merge`). Each `prepare_*` function reads current state (async, outside
//! any transaction) and returns a plain-data [`crate::atomic_runner::AtomicOpPlan`]
//! ([`crate::atomic_plan`], B1) for the synchronous commit pass ([`crate::
//! atomic_runner::run_atomic_unit`], B2) to apply.
//!
//! `gtd.transition` / `gtd.complete` prepare is deliberately **not** here:
//! their lifecycle vocabulary (`is_terminal`, `can_transition`, ...) lives in
//! `khive-pack-gtd`, a crate that depends on `khive-runtime` — not the other
//! way around. Reproducing that dependency here would invert the crate
//! graph, so their prepare functions live in `kkernel` (which already
//! depends on both `khive-runtime` and `khive-pack-gtd`), calling back into
//! the plain [`PlanStatement`]/[`AffectedRowGuard`] shapes exported from this
//! module's sibling, [`crate::atomic_plan`].
//!
//! `propose` / `review` / `withdraw` (the ADR-046 event-sourced governance
//! lifecycle) are on the v1 admissible list ([`khive_types::pack::
//! ATOMIC_ADMISSIBLE_VERBS`]) but have no prepare implementation here: their
//! apply path is a changeset-interpreter (`apply_worker`) over a dedicated
//! `proposals_open` table, not a small number of guarded DML statements — a
//! faithful, non-stub atomic prepare for them is separate follow-on work.
//! [`prepare_governance_unimplemented`] fails loudly, before any write,
//! naming this as a known scope gap rather than silently no-opping.

use serde_json::Value;
use uuid::Uuid;

use khive_storage::types::SqlValue;
use khive_storage::{EdgeRelation, SqlStatement};

use crate::atomic_plan::{
    AffectedRowGuard, DeletePlan, LinkPlan, MergePlan, PlanStatement, PostCommitEffect, UpdatePlan,
};
use crate::atomic_runner::AtomicOpPlan;
use crate::curation::{merge_properties, EntityDedupMergePolicy};
use crate::error::{RuntimeError, RuntimeResult};
use crate::operations::{
    canonical_edge_endpoints, validate_edge_metadata, validate_edge_weight, Resolved,
};
use crate::runtime::{KhiveRuntime, NamespaceToken};

// ---------------------------------------------------------------------------
// arg extraction helpers
// ---------------------------------------------------------------------------

fn obj(args: &Value) -> RuntimeResult<&serde_json::Map<String, Value>> {
    args.as_object()
        .ok_or_else(|| RuntimeError::InvalidInput("op args must be a JSON object".into()))
}

fn require_str<'a>(args: &'a Value, key: &str) -> RuntimeResult<&'a str> {
    obj(args)?
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing required field {key:?}")))
}

fn require_uuid(args: &Value, key: &str) -> RuntimeResult<Uuid> {
    let raw = require_str(args, key)?;
    Uuid::parse_str(raw)
        .map_err(|_| RuntimeError::InvalidInput(format!("{key} must be a full UUID; got {raw:?}")))
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    obj(args).ok()?.get(key).and_then(|v| v.as_str())
}

/// Three-way patch semantics for a nullable string field (mirrors
/// `khive-pack-kg::handlers::common::optional_string_patch`, reimplemented
/// here rather than imported — that module is `pub(crate)` to a sibling
/// crate with no dependency edge back to `khive-runtime`):
/// key absent -> `None` (leave unchanged); key present and JSON `null` ->
/// `Some(None)` (clear); key present and a string -> `Some(Some(s))` (set).
fn optional_string_patch(args: &Value, key: &str) -> RuntimeResult<Option<Option<String>>> {
    match obj(args)?.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string or null"
        ))),
    }
}

fn optional_tags(args: &Value) -> RuntimeResult<Option<Vec<String>>> {
    match obj(args)?.get("tags") {
        None => Ok(None),
        Some(Value::Array(items)) => {
            let mut tags = Vec::with_capacity(items.len());
            for item in items {
                let s = item.as_str().ok_or_else(|| {
                    RuntimeError::InvalidInput("tags must be an array of strings".into())
                })?;
                tags.push(s.to_string());
            }
            Ok(Some(tags))
        }
        Some(_) => Err(RuntimeError::InvalidInput(
            "tags must be an array of strings".into(),
        )),
    }
}

fn optional_f64(args: &Value, key: &str) -> RuntimeResult<Option<f64>> {
    match obj(args)?.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} must be a number"))),
    }
}

fn properties_string(properties: &Option<Value>) -> Option<String> {
    properties
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Build the prepared [`AtomicOpPlan`] for one KG-substrate admissible op
/// (`update`, `delete`, `link`, `merge`). Returns a loud [`RuntimeError`] for
/// `propose`/`review`/`withdraw` (known scope gap, see module doc) and any
/// other verb (the CLI boundary must reject those before calling this — a
/// verb reaching here is either KG-substrate-admissible or a bug upstream).
pub async fn prepare_op(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tool: &str,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    match tool {
        "update" => prepare_update(runtime, token, args).await,
        "delete" => prepare_delete(runtime, token, args).await,
        "link" => prepare_link(runtime, token, args).await,
        "merge" => prepare_merge(runtime, token, args).await,
        "propose" | "review" | "withdraw" => prepare_governance_unimplemented(tool),
        other => Err(RuntimeError::InvalidInput(format!(
            "{other:?} has no atomic_prepare::prepare_op implementation; the CLI \
             admissibility check should have rejected this before prepare"
        ))),
    }
}

fn prepare_governance_unimplemented(tool: &str) -> RuntimeResult<AtomicOpPlan> {
    Err(RuntimeError::InvalidInput(format!(
        "{tool:?} is on the ADR-099 v1 admissible verb list but has no --atomic \
         prepare/apply implementation yet: its lifecycle (ADR-046) is an \
         event-sourced changeset-interpreter over a dedicated `proposals_open` \
         table, not a small guarded-DML plan — a faithful non-stub atomic \
         prepare for it is tracked as ADR-099 follow-up work, not implemented \
         in slice B3. No write was attempted."
    )))
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

async fn prepare_update(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let id = require_uuid(args, "id")?;
    match runtime.resolve_by_id(token, id).await? {
        Some(Resolved::Entity(entity)) => {
            let name = optional_str(args, "name").map(|s| s.to_string());
            let description = optional_string_patch(args, "description")?;
            let properties = obj(args)?.get("properties").cloned();
            let tags = optional_tags(args)?;

            if let Some(ref n) = name {
                crate::secret_gate::check(n)?;
            }
            if let Some(Some(ref d)) = description {
                crate::secret_gate::check(d)?;
            }
            if let Some(ref p) = properties {
                crate::secret_gate::check_json(p)?;
            }
            if let Some(ref t) = tags {
                crate::secret_gate::check_tags(t)?;
            }

            let mut final_name = entity.name.clone();
            let mut final_description = entity.description.clone();
            let mut final_properties = entity.properties.clone();
            let mut final_tags = entity.tags.clone();
            let mut text_changed = false;

            if let Some(n) = name {
                text_changed |= final_name != n;
                final_name = n;
            }
            if let Some(d) = description {
                text_changed |= final_description != d;
                final_description = d;
            }
            if let Some(p) = properties {
                let (merged, _) = merge_properties(
                    &final_properties,
                    &Some(p),
                    EntityDedupMergePolicy::PreferFrom,
                );
                final_properties = merged;
            }
            if let Some(t) = tags {
                final_tags = t;
            }

            let updated_at = chrono::Utc::now().timestamp_micros();
            let statement = SqlStatement {
                sql: "UPDATE entities SET name = ?1, description = ?2, properties = ?3, \
                      tags = ?4, updated_at = ?5 WHERE id = ?6 AND deleted_at IS NULL"
                    .to_string(),
                params: vec![
                    SqlValue::Text(final_name),
                    match final_description {
                        Some(d) => SqlValue::Text(d),
                        None => SqlValue::Null,
                    },
                    match properties_string(&final_properties) {
                        Some(p) => SqlValue::Text(p),
                        None => SqlValue::Null,
                    },
                    SqlValue::Text(
                        serde_json::to_string(&final_tags).unwrap_or_else(|_| "[]".into()),
                    ),
                    SqlValue::Integer(updated_at),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("atomic-update-entity".to_string()),
            };
            let post_commit = if text_changed {
                PostCommitEffect::ReindexEntity { entity_id: id }
            } else {
                PostCommitEffect::None
            };
            Ok(AtomicOpPlan::Update(UpdatePlan {
                target_id: id,
                statements: vec![PlanStatement {
                    statement,
                    guard: Some(AffectedRowGuard::exactly(1)),
                }],
                post_commit,
            }))
        }
        Some(Resolved::Note(note)) => {
            let name = optional_string_patch(args, "name")?;
            let content = optional_str(args, "content").map(|s| s.to_string());
            let properties = obj(args)?.get("properties").cloned();
            let salience = optional_f64(args, "salience")?;
            let decay_factor = optional_f64(args, "decay_factor")?;

            if let Some(ref c) = content {
                crate::secret_gate::check(c)?;
            }
            if let Some(Some(ref n)) = name {
                crate::secret_gate::check(n)?;
            }
            if let Some(ref p) = properties {
                crate::secret_gate::check_json(p)?;
            }

            let mut final_name = note.name.clone();
            let mut final_content = note.content.clone();
            let mut final_properties = note.properties.clone();
            let mut final_salience = note.salience;
            let mut final_decay = note.decay_factor;
            let mut text_changed = false;

            if let Some(n) = name {
                text_changed |= final_name != n;
                final_name = n;
            }
            if let Some(c) = content {
                text_changed |= final_content != c;
                final_content = c;
            }
            if let Some(p) = properties {
                let (merged, _) = merge_properties(
                    &final_properties,
                    &Some(p),
                    EntityDedupMergePolicy::PreferFrom,
                );
                final_properties = merged;
            }
            if obj(args)?.contains_key("salience") {
                if let Some(s) = salience {
                    if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                        return Err(RuntimeError::InvalidInput(format!(
                            "salience must be a finite value in [0.0, 1.0]; got {s}"
                        )));
                    }
                }
                final_salience = salience;
            }
            if obj(args)?.contains_key("decay_factor") {
                if let Some(d) = decay_factor {
                    if !d.is_finite() || d < 0.0 {
                        return Err(RuntimeError::InvalidInput(format!(
                            "decay_factor must be a finite value >= 0.0; got {d}"
                        )));
                    }
                }
                final_decay = decay_factor;
            }

            let updated_at = chrono::Utc::now().timestamp_micros();
            let statement = SqlStatement {
                sql: "UPDATE notes SET name = ?1, content = ?2, properties = ?3, \
                      salience = ?4, decay_factor = ?5, updated_at = ?6 \
                      WHERE id = ?7 AND deleted_at IS NULL"
                    .to_string(),
                params: vec![
                    match final_name {
                        Some(n) => SqlValue::Text(n),
                        None => SqlValue::Null,
                    },
                    SqlValue::Text(final_content),
                    match properties_string(&final_properties) {
                        Some(p) => SqlValue::Text(p),
                        None => SqlValue::Null,
                    },
                    match final_salience {
                        Some(s) => SqlValue::Float(s),
                        None => SqlValue::Null,
                    },
                    match final_decay {
                        Some(d) => SqlValue::Float(d),
                        None => SqlValue::Null,
                    },
                    SqlValue::Integer(updated_at),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("atomic-update-note".to_string()),
            };
            let post_commit = if text_changed {
                PostCommitEffect::ReindexNote { note_id: id }
            } else {
                PostCommitEffect::None
            };
            Ok(AtomicOpPlan::Update(UpdatePlan {
                target_id: id,
                statements: vec![PlanStatement {
                    statement,
                    guard: Some(AffectedRowGuard::exactly(1)),
                }],
                post_commit,
            }))
        }
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "update target {id} must be an entity or note"
        ))),
        None => Err(RuntimeError::NotFound(format!("entity/note {id}"))),
    }
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

async fn prepare_delete(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let id = require_uuid(args, "id")?;
    let hard = obj(args)?
        .get("hard")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match runtime.resolve_by_id(token, id).await? {
        Some(Resolved::Entity(_)) => {
            let mut statements = if hard {
                vec![PlanStatement {
                    statement: SqlStatement {
                        sql: "DELETE FROM entities WHERE id = ?1 AND deleted_at IS NULL"
                            .to_string(),
                        params: vec![SqlValue::Text(id.to_string())],
                        label: Some("atomic-delete-entity-hard".to_string()),
                    },
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            } else {
                let deleted_at = chrono::Utc::now().timestamp_micros();
                vec![PlanStatement {
                    statement: SqlStatement {
                        sql: "UPDATE entities SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL"
                            .to_string(),
                        params: vec![SqlValue::Integer(deleted_at), SqlValue::Text(id.to_string())],
                        label: Some("atomic-delete-entity-soft".to_string()),
                    },
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            };
            if hard {
                statements.push(PlanStatement {
                    statement: SqlStatement {
                        sql: "DELETE FROM graph_edges WHERE source_id = ?1 OR target_id = ?1"
                            .to_string(),
                        params: vec![SqlValue::Text(id.to_string())],
                        label: Some("atomic-delete-entity-cascade-edges".to_string()),
                    },
                    guard: None,
                });
            }
            Ok(AtomicOpPlan::Delete(DeletePlan {
                target_id: id,
                statements,
            }))
        }
        Some(Resolved::Note(_)) => {
            let mut statements = if hard {
                vec![PlanStatement {
                    statement: SqlStatement {
                        sql: "DELETE FROM notes WHERE id = ?1 AND deleted_at IS NULL".to_string(),
                        params: vec![SqlValue::Text(id.to_string())],
                        label: Some("atomic-delete-note-hard".to_string()),
                    },
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            } else {
                let deleted_at = chrono::Utc::now().timestamp_micros();
                vec![PlanStatement {
                    statement: SqlStatement {
                        sql: "UPDATE notes SET status = 'deleted', deleted_at = ?1 \
                              WHERE id = ?2 AND deleted_at IS NULL"
                            .to_string(),
                        params: vec![
                            SqlValue::Integer(deleted_at),
                            SqlValue::Text(id.to_string()),
                        ],
                        label: Some("atomic-delete-note-soft".to_string()),
                    },
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            };
            if hard {
                statements.push(PlanStatement {
                    statement: SqlStatement {
                        sql: "DELETE FROM graph_edges WHERE source_id = ?1 OR target_id = ?1"
                            .to_string(),
                        params: vec![SqlValue::Text(id.to_string())],
                        label: Some("atomic-delete-note-cascade-edges".to_string()),
                    },
                    guard: None,
                });
            }
            Ok(AtomicOpPlan::Delete(DeletePlan {
                target_id: id,
                statements,
            }))
        }
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "delete target {id} must be an entity or note"
        ))),
        None => Err(RuntimeError::NotFound(format!("entity/note {id}"))),
    }
}

// ---------------------------------------------------------------------------
// link
// ---------------------------------------------------------------------------

fn parse_edge_relation(raw: &str) -> RuntimeResult<EdgeRelation> {
    raw.parse::<EdgeRelation>()
        .map_err(|e| RuntimeError::InvalidInput(format!("unknown edge relation {raw:?}: {e}")))
}

async fn prepare_link(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let source_id = require_uuid(args, "source_id")?;
    let target_id = require_uuid(args, "target_id")?;
    let relation = parse_edge_relation(require_str(args, "relation")?)?;
    let weight = optional_f64(args, "weight")?.unwrap_or(1.0);
    let metadata = obj(args)?.get("metadata").cloned();

    validate_edge_weight(weight)?;
    validate_edge_metadata(relation, metadata.as_ref())?;
    runtime
        .validate_edge_relation_endpoints(token, source_id, target_id, relation)
        .await?;

    let (canon_source, canon_target) = canonical_edge_endpoints(relation, source_id, target_id);
    let edge_id = Uuid::new_v4();
    let namespace = token.namespace().as_str().to_string();
    let now = chrono::Utc::now().timestamp_micros();
    let metadata_str = metadata.map(|m| serde_json::to_string(&m).unwrap_or_default());

    let statement = SqlStatement {
        sql: "INSERT INTO graph_edges \
              (namespace, id, source_id, target_id, relation, weight, created_at, updated_at, metadata) \
              SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8 \
              WHERE (EXISTS (SELECT 1 FROM entities WHERE id = ?3 AND deleted_at IS NULL) \
                     OR EXISTS (SELECT 1 FROM notes WHERE id = ?3 AND deleted_at IS NULL)) \
                AND (EXISTS (SELECT 1 FROM entities WHERE id = ?4 AND deleted_at IS NULL) \
                     OR EXISTS (SELECT 1 FROM notes WHERE id = ?4 AND deleted_at IS NULL))"
            .to_string(),
        params: vec![
            SqlValue::Text(namespace),
            SqlValue::Text(edge_id.to_string()),
            SqlValue::Text(canon_source.to_string()),
            SqlValue::Text(canon_target.to_string()),
            SqlValue::Text(relation.as_str().to_string()),
            SqlValue::Float(weight),
            SqlValue::Integer(now),
            match metadata_str {
                Some(m) => SqlValue::Text(m),
                None => SqlValue::Null,
            },
        ],
        label: Some("atomic-link-insert-edge-where-exists".to_string()),
    };

    Ok(AtomicOpPlan::Link(LinkPlan {
        source_id: canon_source,
        target_id: canon_target,
        statement: PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        },
    }))
}

// ---------------------------------------------------------------------------
// merge (entity-only, ADR-099 B3 scope decision — see final report)
// ---------------------------------------------------------------------------

async fn prepare_merge(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let into_id = require_uuid(args, "into_id")?;
    let from_id = require_uuid(args, "from_id")?;
    if into_id == from_id {
        return Err(RuntimeError::InvalidInput(
            "cannot merge an entity into itself".into(),
        ));
    }

    let entities = runtime.entities(token)?;
    entities
        .get_entity(into_id)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("entity {into_id}")))?;
    entities
        .get_entity(from_id)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("entity {from_id}")))?;

    let now = chrono::Utc::now().timestamp_micros();
    let rewires = vec![
        crate::atomic_plan::PlanPredicate {
            description: "source_id = :from".to_string(),
            statement: SqlStatement {
                sql: "UPDATE graph_edges SET source_id = ?1, updated_at = ?2 WHERE source_id = ?3"
                    .to_string(),
                params: vec![
                    SqlValue::Text(into_id.to_string()),
                    SqlValue::Integer(now),
                    SqlValue::Text(from_id.to_string()),
                ],
                label: Some("atomic-merge-rewire-source".to_string()),
            },
        },
        crate::atomic_plan::PlanPredicate {
            description: "target_id = :from".to_string(),
            statement: SqlStatement {
                sql: "UPDATE graph_edges SET target_id = ?1, updated_at = ?2 WHERE target_id = ?3"
                    .to_string(),
                params: vec![
                    SqlValue::Text(into_id.to_string()),
                    SqlValue::Integer(now),
                    SqlValue::Text(from_id.to_string()),
                ],
                label: Some("atomic-merge-rewire-target".to_string()),
            },
        },
    ];
    let lifecycle = vec![PlanStatement {
        statement: SqlStatement {
            sql: "UPDATE entities SET deleted_at = ?1, merged_into = ?2 \
                  WHERE id = ?3 AND deleted_at IS NULL"
                .to_string(),
            params: vec![
                SqlValue::Integer(now),
                SqlValue::Text(into_id.to_string()),
                SqlValue::Text(from_id.to_string()),
            ],
            label: Some("atomic-merge-tombstone-from-entity".to_string()),
        },
        guard: Some(AffectedRowGuard::exactly(1)),
    }];

    Ok(AtomicOpPlan::Merge(MergePlan {
        into_id,
        from_id,
        rewires,
        lifecycle,
    }))
}

// ---------------------------------------------------------------------------
// post-commit effects
// ---------------------------------------------------------------------------

/// Run every deferred [`PostCommitEffect`] after a committed atomic unit,
/// outside any transaction (ADR-099 D1 phase 3). Re-fetches each target's
/// now-committed row and reuses the existing `reindex_entity`/`reindex_note`
/// (FTS + embedding, same as the non-atomic path) for exact parity.
pub async fn apply_post_commit_effects(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    effects: Vec<PostCommitEffect>,
) -> RuntimeResult<()> {
    for effect in effects {
        match effect {
            PostCommitEffect::None => {}
            PostCommitEffect::ReindexEntity { entity_id } => {
                if let Some(entity) = runtime.entities(token)?.get_entity(entity_id).await? {
                    runtime.reindex_entity(token, &entity).await?;
                }
            }
            PostCommitEffect::ReindexNote { note_id } => {
                if let Some(note) = runtime.notes(token)?.get_note(note_id).await? {
                    runtime.reindex_note(token, &note).await?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use serde_json::json;

    use khive_types::Namespace;

    use crate::embedder_registry::EmbedderProvider;
    use crate::runtime::RuntimeConfig;

    const STUB_MODEL: &str = "stub-adr099-b3";
    const STUB_DIMS: usize = 4;

    struct StubService;

    #[async_trait]
    impl EmbeddingService for StubService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![0.5_f32; STUB_DIMS]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            STUB_MODEL
        }
    }

    struct StubProvider;

    #[async_trait]
    impl EmbedderProvider for StubProvider {
        fn name(&self) -> &str {
            STUB_MODEL
        }

        fn dimensions(&self) -> usize {
            STUB_DIMS
        }

        async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(StubService))
        }
    }

    fn scratch_runtime() -> KhiveRuntime {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("atomic_prepare_reindex.db");
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        std::mem::forget(dir);
        rt
    }

    /// ADR-099 B3 acceptance test 3: updating a note's content inside an
    /// atomic unit must, after commit, leave the note recallable via FTS
    /// under its NEW content and its vector row refreshed — parity with the
    /// non-atomic `update_note` -> `reindex_note` path.
    #[tokio::test]
    async fn atomic_update_note_content_is_fts_and_vector_reindexed_post_commit() {
        let runtime = scratch_runtime();
        runtime.register_embedder(StubProvider);
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        let mut note = khive_storage::note::Note::new("local", "observation", "original content");
        note.name = Some("reindex-target".to_string());
        let note_id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("seed note");

        // Sanity: no vector row yet for the stub model.
        let vec_store = runtime
            .vectors_for_model(&token, STUB_MODEL)
            .expect("vec store");
        assert_eq!(vec_store.count().await.expect("count before"), 0);

        let plan = prepare_update(
            &runtime,
            &token,
            &json!({"id": note_id.to_string(), "content": "freshly-updated-content-xyz"}),
        )
        .await
        .expect("prepare update");

        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        let post_commit = match outcome {
            crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected Committed, got {other:?}"),
        };
        assert_eq!(
            post_commit,
            vec![PostCommitEffect::ReindexNote { note_id }],
            "content change must schedule exactly one ReindexNote post-commit effect"
        );

        apply_post_commit_effects(&runtime, &token, post_commit)
            .await
            .expect("apply post-commit effects");

        // FTS: the note must be recallable under its NEW content.
        let doc = runtime
            .text_for_notes(&token)
            .expect("text store")
            .get_document("local", note_id)
            .await
            .expect("get_document")
            .expect("document must be indexed after post-commit reindex");
        assert!(
            doc.body.contains("freshly-updated-content-xyz"),
            "FTS body must reflect the committed content: {:?}",
            doc.body
        );

        // Vector: a row must now exist for the registered stub model.
        assert_eq!(
            vec_store.count().await.expect("count after"),
            1,
            "post-commit reindex must have inserted a vector row for the stub model"
        );
    }
}
