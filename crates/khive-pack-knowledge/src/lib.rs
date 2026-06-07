//! pack-knowledge — knowledge corpus verbs for khive.

pub(crate) mod handlers;
pub(crate) mod knowledge;
mod pack;
mod vocab;

pub use pack::KnowledgePack;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use serde_json::Value;

/// Reindex the knowledge corpus for `token`'s namespace: embed atoms with the
/// default embedder and (optionally) rebuild the Vamana ANN snapshot.
///
/// Library entry for `kkernel reindex` — equivalent to the `knowledge.index`
/// verb over the full corpus, callable without an MCP server. Knowledge search
/// is single-model (it retrieves via the default embedder's ANN), so this does
/// not fan out across registered models the way entity/note reindex does.
pub async fn reindex_knowledge(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    rebuild_ann: bool,
    batch_size: Option<u32>,
) -> Result<Value, RuntimeError> {
    let ann = knowledge::vamana::new_shared();
    let mut params = serde_json::Map::new();
    params.insert("rebuild_ann".into(), Value::Bool(rebuild_ann));
    if let Some(bs) = batch_size {
        params.insert("batch_size".into(), Value::from(bs));
    }
    knowledge::KnowledgeHandlers::index(runtime, token, Value::Object(params), &ann).await
}
