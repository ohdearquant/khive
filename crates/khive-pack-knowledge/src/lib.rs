//! pack-knowledge — knowledge corpus verbs for khive.

pub(crate) mod handlers;
pub(crate) mod knowledge;
mod pack;
mod vocab;

pub use pack::KnowledgePack;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use serde_json::{json, Value};

/// Options for [`reindex_knowledge`].
#[derive(Debug, Clone, Copy)]
pub struct KnowledgeReindexOptions {
    /// Embed atoms (and rebuild the atom Vamana ANN).
    pub atoms: bool,
    /// Embed sections into `knowledge_sections.embedding` (ADR-051).
    pub sections: bool,
    /// Re-embed everything; when false, only fill missing vectors.
    pub drop_existing: bool,
    /// Rebuild the atom Vamana ANN snapshot (only meaningful with `atoms`).
    pub rebuild_ann: bool,
    /// Records per embedding batch.
    pub batch_size: Option<u32>,
}

/// Reindex the knowledge corpus for `token`'s namespace: embed atoms and/or
/// sections with the default embedder and (optionally) rebuild the atom Vamana
/// ANN snapshot.
///
/// Library entry for `kkernel reindex` — callable without an MCP server.
/// Knowledge is single-model: atom indexing, section indexing, and search all
/// use the default embedder. Returns `{atoms_indexed, sections_indexed, failed,
/// ann_failed, sections_failed, truncation_by_model}`.
///
/// Optional progress callbacks receive `(processed, total)` after each batch.
pub async fn reindex_knowledge(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    opts: KnowledgeReindexOptions,
    on_atom_progress: Option<&(dyn Fn(u64, u64) + Send + Sync)>,
    on_section_progress: Option<&(dyn Fn(u64, u64) + Send + Sync)>,
) -> Result<Value, RuntimeError> {
    let mut atoms_indexed = 0u64;
    let mut failed = 0u64;
    let mut ann_failed = false;
    let mut truncation_by_model = serde_json::Map::new();
    if opts.atoms {
        let ann = knowledge::vamana::new_shared();
        let mut params = serde_json::Map::new();
        params.insert("rebuild_ann".into(), Value::Bool(opts.rebuild_ann));
        params.insert("insert_only".into(), Value::Bool(!opts.drop_existing));
        if let Some(bs) = opts.batch_size {
            params.insert("batch_size".into(), Value::from(bs));
        }
        let result = knowledge::KnowledgeHandlers::index(
            runtime,
            token,
            Value::Object(params),
            &ann,
            on_atom_progress,
        )
        .await?;
        atoms_indexed = result.get("indexed").and_then(|n| n.as_u64()).unwrap_or(0);
        failed = result.get("failed").and_then(|n| n.as_u64()).unwrap_or(0);
        ann_failed = result
            .get("ann_failed")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        truncation_by_model = result
            .get("truncation_by_model")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
    }

    let mut sections_indexed = 0u64;
    let mut sections_failed = 0u64;
    if opts.sections {
        let batch = opts
            .batch_size
            .map(|b| b as usize)
            .unwrap_or(knowledge::util::DEFAULT_EMBED_BATCH);
        let (indexed, _skipped, sec_failed, section_truncation) =
            knowledge::sections_index::embed_sections(
                runtime,
                token,
                opts.drop_existing,
                batch,
                on_section_progress,
                None,
            )
            .await?;
        sections_indexed = indexed as u64;
        sections_failed = sec_failed as u64;
        if section_truncation.any_truncated() {
            let model = runtime.default_embedder_name().to_string();
            let existing = truncation_by_model
                .remove(&model)
                .and_then(|value| {
                    serde_json::from_value::<khive_runtime::retrieval::EmbeddingTruncationReport>(
                        value,
                    )
                    .ok()
                })
                .unwrap_or_default();
            let mut combined: khive_runtime::retrieval::EmbeddingTruncationReport = existing;
            combined.merge(section_truncation);
            truncation_by_model.insert(
                model,
                serde_json::to_value(combined)
                    .expect("EmbeddingTruncationReport serialization cannot fail"),
            );
        }
    }

    Ok(json!({
        "atoms_indexed": atoms_indexed,
        "sections_indexed": sections_indexed,
        "failed": failed,
        "ann_failed": ann_failed,
        "sections_failed": sections_failed,
        "truncation_by_model": truncation_by_model,
    }))
}
