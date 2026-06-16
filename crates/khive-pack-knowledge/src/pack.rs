//! `KnowledgePack` struct, factory, and `PackRuntime` impl.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use khive_brain_core::SectionPosteriorState;
use khive_runtime::pack::{PackByIdResolver, PackRuntime};
use khive_runtime::{KhiveRuntime, NamespaceToken, Resolved, RuntimeError, VerbRegistry};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use khive_types::{HandlerDef, Pack};

use crate::knowledge::vamana;
use crate::knowledge::KnowledgeHandlers;
use crate::vocab::KNOWLEDGE_HANDLERS;

/// Knowledge corpus pack — atoms, domains, TF-IDF search, fold, import, and KG concept verbs.
pub struct KnowledgePack {
    pub(crate) runtime: KhiveRuntime,
    pub(crate) ann: vamana::SharedAnn,
    pub(crate) section_posteriors: Mutex<SectionPosteriorState>,
    /// Explicit brain profile ID from config (ADR-035 §Brain profile configuration).
    ///
    /// Tier-1 of the 3-tier feedback resolution: when set, `knowledge.feedback` directs
    /// feedback to this profile via `brain.feedback`. When absent, tier-2
    /// (namespace-bound profile) and tier-3 (global prior) are tried in order.
    pub(crate) brain_profile: Option<String>,
}

impl Pack for KnowledgePack {
    const NAME: &'static str = "knowledge";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &KNOWLEDGE_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl KnowledgePack {
    /// Create a new pack bound to the given runtime, initializing a shared ANN index.
    pub fn new(runtime: KhiveRuntime) -> Self {
        let brain_profile = runtime.config().brain_profile.clone();
        Self {
            runtime,
            ann: vamana::new_shared(),
            section_posteriors: Mutex::new(SectionPosteriorState::new()),
            brain_profile,
        }
    }
}

struct KnowledgePackFactory;

impl khive_runtime::PackFactory for KnowledgePackFactory {
    fn name(&self) -> &'static str {
        "knowledge"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(KnowledgePack::new(runtime))
    }

    fn create_resolver(
        &self,
        runtime: KhiveRuntime,
    ) -> Option<Box<dyn khive_runtime::PackByIdResolver>> {
        Some(Box::new(KnowledgePack::new(runtime)))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&KnowledgePackFactory) }

#[async_trait]
impl PackRuntime for KnowledgePack {
    fn name(&self) -> &str {
        <KnowledgePack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &KNOWLEDGE_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::REQUIRES
    }

    async fn warm(&self) {
        crate::knowledge::vamana::warm_known_snapshots(&self.runtime, &self.ann).await;
        if !self.runtime.default_embedder_name().is_empty() {
            let runtime = self.runtime.clone();
            tokio::spawn(async move {
                let _ = runtime.embed("__khive_knowledge_warm__").await;
            });
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "knowledge.upsert_atoms" => {
                KnowledgeHandlers::upsert_atoms(&self.runtime, token, params).await
            }
            "knowledge.upsert_domains" => {
                KnowledgeHandlers::upsert_domains(&self.runtime, token, params).await
            }
            "knowledge.get" => KnowledgeHandlers::get(&self.runtime, token, params).await,
            "knowledge.list" => KnowledgeHandlers::list(&self.runtime, token, params).await,
            "knowledge.delete_atoms" => {
                KnowledgeHandlers::delete_atoms(&self.runtime, token, params).await
            }
            "knowledge.stats" => KnowledgeHandlers::stats(&self.runtime, token, params).await,
            "knowledge.index" => {
                KnowledgeHandlers::index(&self.runtime, token, params, &self.ann, None).await
            }
            "knowledge.fold" => KnowledgeHandlers::fold(&self.runtime, token, params).await,
            "knowledge.search" => {
                KnowledgeHandlers::search(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.suggest" => {
                KnowledgeHandlers::suggest(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.compose" => {
                KnowledgeHandlers::compose(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.edit" => {
                KnowledgeHandlers::edit(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.import" => {
                KnowledgeHandlers::import(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.challenge" => {
                KnowledgeHandlers::challenge(&self.runtime, token, params).await
            }
            "knowledge.adjudicate" => {
                KnowledgeHandlers::adjudicate(&self.runtime, token, params).await
            }
            "knowledge.learn" => self.handle_learn(token, params).await,
            "knowledge.cite" => self.handle_cite(token, params).await,
            "knowledge.topic" => self.handle_topic(token, params).await,
            "knowledge.feedback" => self.handle_feedback(token, params, registry).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "knowledge pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[async_trait]
impl PackByIdResolver for KnowledgePack {
    /// Resolve a live knowledge record by UUID.
    ///
    /// Queries `knowledge_domains` first (canonical record), then
    /// `knowledge_atoms`. The domain mirror atom shares the domain's UUID
    /// (crud.rs:279/300) so domains must win over the mirror.
    async fn resolve_by_id(&self, id: Uuid) -> Result<Option<Resolved>, RuntimeError> {
        let sql = self.runtime.sql();
        let id_str = id.to_string();

        let mut reader = sql
            .reader()
            .await
            .map_err(|e| RuntimeError::Internal(format!("knowledge resolve_by_id reader: {e}")))?;

        // 1. Check knowledge_domains first (canonical over the mirror atom).
        let domain_row = reader
            .query_row(SqlStatement {
                sql: "SELECT id, namespace, slug, name, description, tags, members, \
                      created_at, updated_at, deleted_at \
                      FROM knowledge_domains WHERE id = ?1 AND deleted_at IS NULL LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(id_str.clone())],
                label: Some("knowledge.resolve_by_id.domain".into()),
            })
            .await
            .map_err(|e| {
                RuntimeError::Internal(format!("knowledge resolve_by_id domain query: {e}"))
            })?;

        if let Some(row) = domain_row {
            let data = domain_row_to_json(&row);
            return Ok(Some(Resolved::PackRecord {
                pack: "knowledge".into(),
                kind: "domain".into(),
                data,
            }));
        }

        // 2. Check knowledge_atoms.
        let atom_row = reader
            .query_row(SqlStatement {
                sql: "SELECT id, namespace, slug, name, content, tags, properties, \
                      status, finalized, source_uri, source_type, created_at, updated_at, deleted_at \
                      FROM knowledge_atoms WHERE id = ?1 AND deleted_at IS NULL LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(id_str)],
                label: Some("knowledge.resolve_by_id.atom".into()),
            })
            .await
            .map_err(|e| {
                RuntimeError::Internal(format!("knowledge resolve_by_id atom query: {e}"))
            })?;

        if let Some(row) = atom_row {
            let data = atom_row_to_json(&row);
            return Ok(Some(Resolved::PackRecord {
                pack: "knowledge".into(),
                kind: "atom".into(),
                data,
            }));
        }

        Ok(None)
    }

    /// Resolve a knowledge record including already-soft-deleted records.
    ///
    /// Used by the hard-delete path to locate tombstoned records.
    async fn resolve_by_id_including_deleted(
        &self,
        id: Uuid,
    ) -> Result<Option<Resolved>, RuntimeError> {
        let sql = self.runtime.sql();
        let id_str = id.to_string();

        let mut reader = sql.reader().await.map_err(|e| {
            RuntimeError::Internal(format!(
                "knowledge resolve_by_id_including_deleted reader: {e}"
            ))
        })?;

        let domain_row = reader
            .query_row(SqlStatement {
                sql: "SELECT id, namespace, slug, name, description, tags, members, \
                      created_at, updated_at, deleted_at \
                      FROM knowledge_domains WHERE id = ?1 LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(id_str.clone())],
                label: Some("knowledge.resolve_by_id_incl_deleted.domain".into()),
            })
            .await
            .map_err(|e| {
                RuntimeError::Internal(format!(
                    "knowledge resolve_by_id_including_deleted domain query: {e}"
                ))
            })?;

        if let Some(row) = domain_row {
            let data = domain_row_to_json(&row);
            return Ok(Some(Resolved::PackRecord {
                pack: "knowledge".into(),
                kind: "domain".into(),
                data,
            }));
        }

        let atom_row = reader
            .query_row(SqlStatement {
                sql: "SELECT id, namespace, slug, name, content, tags, properties, \
                      status, finalized, source_uri, source_type, created_at, updated_at, deleted_at \
                      FROM knowledge_atoms WHERE id = ?1 LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(id_str)],
                label: Some("knowledge.resolve_by_id_incl_deleted.atom".into()),
            })
            .await
            .map_err(|e| {
                RuntimeError::Internal(format!(
                    "knowledge resolve_by_id_including_deleted atom query: {e}"
                ))
            })?;

        if let Some(row) = atom_row {
            let data = atom_row_to_json(&row);
            return Ok(Some(Resolved::PackRecord {
                pack: "knowledge".into(),
                kind: "atom".into(),
                data,
            }));
        }

        Ok(None)
    }

    /// Delete a knowledge domain or atom by UUID.
    ///
    /// Soft-delete by default (`hard=false`): sets `deleted_at = now()`.
    /// For domains, also tombstones the mirror atom in `knowledge_atoms`.
    /// Hard-delete (`hard=true`): permanently removes rows from the table(s).
    async fn delete_by_id(&self, id: Uuid, hard: bool) -> Result<serde_json::Value, RuntimeError> {
        let sql = self.runtime.sql();
        let id_str = id.to_string();

        // First determine the kind (including deleted rows so hard-delete can find tombstones).
        let kind = match self.resolve_by_id_including_deleted(id).await? {
            Some(Resolved::PackRecord { kind, .. }) => kind,
            _ => {
                return Err(RuntimeError::NotFound(format!(
                    "knowledge record not found: {id}"
                )));
            }
        };

        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| RuntimeError::Internal(format!("knowledge delete_by_id writer: {e}")))?;

        match kind.as_str() {
            "domain" => {
                if hard {
                    writer
                        .execute(SqlStatement {
                            sql: "DELETE FROM knowledge_domains WHERE id = ?1".into(),
                            params: vec![SqlValue::Text(id_str.clone())],
                            label: Some("knowledge.delete_by_id.domain.hard".into()),
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!(
                                "knowledge delete_by_id domain hard: {e}"
                            ))
                        })?;
                    // Also hard-delete the mirror atom.
                    writer
                        .execute(SqlStatement {
                            sql: "DELETE FROM knowledge_atoms WHERE id = ?1".into(),
                            params: vec![SqlValue::Text(id_str.clone())],
                            label: Some("knowledge.delete_by_id.domain_mirror.hard".into()),
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!(
                                "knowledge delete_by_id domain mirror hard: {e}"
                            ))
                        })?;
                } else {
                    writer
                        .execute(SqlStatement {
                            sql: "UPDATE knowledge_domains SET deleted_at = ?1 \
                                  WHERE id = ?2 AND deleted_at IS NULL"
                                .into(),
                            params: vec![SqlValue::Integer(now_us), SqlValue::Text(id_str.clone())],
                            label: Some("knowledge.delete_by_id.domain.soft".into()),
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!(
                                "knowledge delete_by_id domain soft: {e}"
                            ))
                        })?;
                    // Tombstone the mirror atom so FTS no longer surfaces it.
                    writer
                        .execute(SqlStatement {
                            sql: "UPDATE knowledge_atoms SET deleted_at = ?1 \
                                  WHERE id = ?2 AND deleted_at IS NULL"
                                .into(),
                            params: vec![SqlValue::Integer(now_us), SqlValue::Text(id_str.clone())],
                            label: Some("knowledge.delete_by_id.domain_mirror.soft".into()),
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!(
                                "knowledge delete_by_id domain mirror soft: {e}"
                            ))
                        })?;
                }
            }
            "atom" => {
                if hard {
                    writer
                        .execute(SqlStatement {
                            sql: "DELETE FROM knowledge_atoms WHERE id = ?1".into(),
                            params: vec![SqlValue::Text(id_str.clone())],
                            label: Some("knowledge.delete_by_id.atom.hard".into()),
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!("knowledge delete_by_id atom hard: {e}"))
                        })?;
                } else {
                    writer
                        .execute(SqlStatement {
                            sql: "UPDATE knowledge_atoms SET deleted_at = ?1 \
                                  WHERE id = ?2 AND deleted_at IS NULL"
                                .into(),
                            params: vec![SqlValue::Integer(now_us), SqlValue::Text(id_str.clone())],
                            label: Some("knowledge.delete_by_id.atom.soft".into()),
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(format!("knowledge delete_by_id atom soft: {e}"))
                        })?;
                }
            }
            other => {
                return Err(RuntimeError::Internal(format!(
                    "knowledge delete_by_id: unexpected kind {other:?}"
                )));
            }
        }

        Ok(serde_json::json!({
            "deleted": true,
            "id": id_str,
            "kind": kind,
            "hard": hard,
        }))
    }
}

fn domain_row_to_json(row: &SqlRow) -> serde_json::Value {
    let get_str = |col: &str| -> serde_json::Value {
        match row.get(col) {
            Some(SqlValue::Text(s)) => serde_json::Value::String(s.clone()),
            _ => serde_json::Value::Null,
        }
    };
    let get_i64 = |col: &str| -> serde_json::Value {
        match row.get(col) {
            Some(SqlValue::Integer(n)) => serde_json::json!(n),
            _ => serde_json::Value::Null,
        }
    };
    serde_json::json!({
        "id": get_str("id"),
        "namespace": get_str("namespace"),
        "slug": get_str("slug"),
        "name": get_str("name"),
        "description": get_str("description"),
        "tags": get_str("tags"),
        "members": get_str("members"),
        "created_at": get_i64("created_at"),
        "updated_at": get_i64("updated_at"),
        "deleted_at": get_i64("deleted_at"),
        "kind": "domain",
    })
}

fn atom_row_to_json(row: &SqlRow) -> serde_json::Value {
    let get_str = |col: &str| -> serde_json::Value {
        match row.get(col) {
            Some(SqlValue::Text(s)) => serde_json::Value::String(s.clone()),
            _ => serde_json::Value::Null,
        }
    };
    let get_i64 = |col: &str| -> serde_json::Value {
        match row.get(col) {
            Some(SqlValue::Integer(n)) => serde_json::json!(n),
            _ => serde_json::Value::Null,
        }
    };
    let get_bool = |col: &str| -> serde_json::Value {
        match row.get(col) {
            Some(SqlValue::Integer(n)) => serde_json::json!(*n != 0),
            _ => serde_json::json!(false),
        }
    };
    serde_json::json!({
        "id": get_str("id"),
        "namespace": get_str("namespace"),
        "slug": get_str("slug"),
        "name": get_str("name"),
        "content": get_str("content"),
        "tags": get_str("tags"),
        "properties": get_str("properties"),
        "status": get_str("status"),
        "finalized": get_bool("finalized"),
        "source_uri": get_str("source_uri"),
        "source_type": get_str("source_type"),
        "created_at": get_i64("created_at"),
        "updated_at": get_i64("updated_at"),
        "deleted_at": get_i64("deleted_at"),
        "kind": "atom",
    })
}
