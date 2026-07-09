//! `GitPack` struct, `Pack` impl, self-registration factory, and `PackRuntime` impl.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, NoteKindSpec, PackSchemaPlan, RuntimeError, SchemaPlan,
    VerbRegistry,
};
use khive_types::{EdgeEndpointRule, HandlerDef, Pack};

use crate::hook::{CommitHook, IssueLikeHook};
use crate::vocab::{GIT_NOTE_KIND_SPECS, GIT_SCHEMA_PLAN_STMTS};

/// Git-lifecycle pack (ADR-088, amended by ADR-088 Amendment 1) — registers
/// `commit` / `issue` / `pull_request` note kinds populated by the batch
/// ingester in `src/ingest.rs`, and one agent-facing verb, `git.digest`
/// (`src/handlers.rs`). Extends the base edge contract with `precedes`
/// commit→commit (parent→child lineage, ADR-088 Amendment 1 ingest
/// enrichment) — the only new endpoint rule this pack contributes;
/// everything else uses the base `annotates` contract.
pub struct GitPack {
    runtime: KhiveRuntime,
}

impl Pack for GitPack {
    const NAME: &'static str = "git";
    const NOTE_KINDS: &'static [&'static str] = &["commit", "issue", "pull_request"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &crate::vocab::GIT_HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = &crate::vocab::GIT_EDGE_RULES;
    const REQUIRES: &'static [&'static str] = &["kg"];
    const NOTE_KIND_SPECS: &'static [NoteKindSpec] = &GIT_NOTE_KIND_SPECS;
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: "git",
        statements: &GIT_SCHEMA_PLAN_STMTS,
    });
}

impl GitPack {
    /// Create a new `GitPack` bound to the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    /// Accessor for `src/handlers.rs`, which lives in a sibling module and
    /// so cannot reach the private `runtime` field directly (mirrors
    /// `khive-pack-gtd`'s identical `GtdPack::runtime()` accessor).
    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

// -- inventory self-registration --------------------------------------------

struct GitPackFactory;

impl khive_runtime::PackFactory for GitPackFactory {
    fn name(&self) -> &'static str {
        "git"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(GitPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&GitPackFactory) }

#[async_trait]
impl PackRuntime for GitPack {
    fn name(&self) -> &str {
        <GitPack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <GitPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <GitPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <GitPack as Pack>::HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        <GitPack as Pack>::EDGE_RULES
    }

    fn requires(&self) -> &'static [&'static str] {
        <GitPack as Pack>::REQUIRES
    }

    fn note_kind_specs(&self) -> &'static [NoteKindSpec] {
        <GitPack as Pack>::NOTE_KIND_SPECS
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "git",
            statements: &GIT_SCHEMA_PLAN_STMTS,
        }
    }

    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        match kind {
            "commit" => Some(Arc::new(CommitHook)),
            "issue" => Some(Arc::new(IssueLikeHook { kind: "issue" })),
            "pull_request" => Some(Arc::new(IssueLikeHook {
                kind: "pull_request",
            })),
            _ => None,
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
            "git.digest" => self.handle_digest(token, registry, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "git pack does not handle verb {verb:?}"
            ))),
        }
    }
}
