//! `GitPack` struct, `Pack` impl, self-registration factory, and `PackRuntime` impl.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, NoteKindSpec, PackSchemaPlan, RuntimeError,
    VerbRegistry,
};
use khive_types::{EdgeEndpointRule, EntityTypeDef, HandlerDef, Pack};

use crate::hook::{CommitHook, IssueLikeHook};
use crate::vocab::{GIT_ENTITY_TYPES, GIT_NOTE_KIND_SPECS, GIT_SCHEMA_PLAN_STMTS};

/// Git-lifecycle pack (ADR-088, amended by ADR-088 Amendments 1 and 2, plus
/// ADR-108) — registers `commit` / `issue` / `pull_request` note kinds populated by
/// the batch ingester in `src/ingest.rs`, the agent-facing verb
/// `git.digest` (`src/handlers.rs`), the read `git.ingest_cursor`, write verbs `git.commit` /
/// `git.branch` / `git.push` (`src/write_handlers.rs`, ADR-108), and `git.update_ref`
/// (`src/local_handlers.rs`). Extends the
/// base edge contract with `precedes` commit→commit (parent→child lineage,
/// ADR-088 Amendment 1 ingest enrichment) — the only new endpoint rule this
/// pack contributes; everything else uses the base `annotates` contract.
pub struct GitPack {
    runtime: KhiveRuntime,
    remote: Arc<dyn crate::remote_transport::RemoteTransport>,
}

impl Pack for GitPack {
    const NAME: &'static str = "git";
    const NOTE_KINDS: &'static [&'static str] = &["commit", "issue", "pull_request"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &crate::vocab::GIT_HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = &crate::vocab::GIT_EDGE_RULES;
    const ENTITY_TYPES: &'static [EntityTypeDef] = &GIT_ENTITY_TYPES;
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
        let remote = crate::remote_transport::GhTransport::new(
            runtime.config().git_write.git_program().to_path_buf(),
        );
        Self {
            runtime,
            remote: Arc::new(remote),
        }
    }

    pub fn with_remote_transport(
        runtime: KhiveRuntime,
        remote: Arc<dyn crate::remote_transport::RemoteTransport>,
    ) -> Self {
        Self { runtime, remote }
    }

    pub(crate) fn remote_transport(&self) -> &dyn crate::remote_transport::RemoteTransport {
        self.remote.as_ref()
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
    khive_runtime::pack_factory_metadata!(GitPack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(GitPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&GitPackFactory) }

#[async_trait]
impl PackRuntime for GitPack {
    khive_runtime::pack_runtime_metadata!();

    fn input_schema(&self, verb: &str) -> Option<Value> {
        crate::input_schema::for_verb(verb)
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
            "git.ingest_cursor" => self.handle_ingest_cursor(token, registry, params).await,
            "git.commit" if params.get("tree").is_some() => {
                self.handle_local(token, registry, verb, params).await
            }
            "git.commit" => self.handle_commit(token, registry, params).await,
            "git.branch" => self.handle_local(token, registry, verb, params).await,
            "git.update_ref" => self.handle_local(token, registry, verb, params).await,
            "git.push" | "git.pr_open" | "git.pr_review" | "git.pr_merge" => {
                self.handle_remote(token, registry, verb, params).await
            }
            "git.init" | "git.checkout" | "git.diff" | "git.reconcile" => {
                self.handle_local(token, registry, verb, params).await
            }
            "git.receipts" => self.handle_receipts(token, registry, params).await,
            "git.gates" => self.handle_gates(token, registry, params).await,
            "git.status" => self.handle_status(token, registry, params).await,
            "git.log" => self.handle_log(token, registry, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "git pack does not handle verb {verb:?}"
            ))),
        }
    }
}
