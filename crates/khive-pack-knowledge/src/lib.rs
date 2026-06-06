//! pack-knowledge — knowledge corpus verbs for khive.
//!
//! Manages slug-keyed atoms and domain groupings (corpus tier), atom sections
//! (section tier), and KG concept sugar (`learn`, `cite`, `topic`).
//! Load with `KHIVE_PACKS=kg,knowledge` or `--pack knowledge`.

pub(crate) mod handlers;
pub(crate) mod knowledge;
mod vocab;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack};

use crate::knowledge::vamana;
use crate::knowledge::KnowledgeHandlers;
use crate::vocab::KNOWLEDGE_HANDLERS;

/// Knowledge corpus pack — atoms, domains, TF-IDF search, fold, import, and KG concept verbs.
pub struct KnowledgePack {
    pub(crate) runtime: KhiveRuntime,
    pub(crate) ann: vamana::SharedAnn,
}

impl Pack for KnowledgePack {
    const NAME: &'static str = "knowledge";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &KNOWLEDGE_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl KnowledgePack {
    /// Create a new `KnowledgePack` bound to the given runtime, initializing a shared ANN index.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self {
            runtime,
            ann: vamana::new_shared(),
        }
    }
}

// ── inventory self-registration ───────────────────────────────────────────────

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
        knowledge::vamana::warm_known_snapshots(&self.runtime, &self.ann).await;
        // Pre-warm the embedding model so the first reranked knowledge.search does not
        // pay cold model-weight load in the request path. Fire-and-forget so pack warm
        // never blocks daemon startup.
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
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            // corpus tier
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
                KnowledgeHandlers::index(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.fold" => KnowledgeHandlers::fold(&self.runtime, token, params).await,
            "knowledge.search" => {
                KnowledgeHandlers::search(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.suggest" => {
                KnowledgeHandlers::suggest(&self.runtime, token, params, &self.ann).await
            }
            "knowledge.compose" => KnowledgeHandlers::compose(&self.runtime, token, params).await,
            // section tier
            "knowledge.edit" => KnowledgeHandlers::edit(&self.runtime, token, params).await,
            "knowledge.import" => KnowledgeHandlers::import(&self.runtime, token, params).await,
            "knowledge.challenge" => {
                KnowledgeHandlers::challenge(&self.runtime, token, params).await
            }
            "knowledge.adjudicate" => {
                KnowledgeHandlers::adjudicate(&self.runtime, token, params).await
            }
            // concept tier
            "knowledge.learn" => self.handle_learn(token, params).await,
            "knowledge.cite" => self.handle_cite(token, params).await,
            "knowledge.topic" => self.handle_topic(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "knowledge pack does not handle verb {verb:?}"
            ))),
        }
    }
}
