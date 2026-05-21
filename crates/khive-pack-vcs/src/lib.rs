//! pack-vcs — KG versioning verb pack for khive (ADR-042, ADR-043).
//!
//! Provides verbs for content-addressed snapshots, branch management,
//! merge operations, and KG portability (export/import).
//!
//! # VCS pack — implements ADR-015/042/043 snapshot+branch model.
//! ADR-051/053 (proposed) describe a future git-native direction.
//! When those are accepted, this pack will be deprecated; see ADR-053.

pub mod handlers;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry};
use khive_types::{Pack, VerbDef};

pub struct VcsPack {
    runtime: KhiveRuntime,
}

impl Pack for VcsPack {
    const NAME: &'static str = "vcs";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const VERBS: &'static [VerbDef] = &VCS_VERBS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

// VCS pack — implements ADR-015/042/043 snapshot+branch model.
// ADR-051/053 (proposed) describe a future git-native direction.
// When those are accepted, this pack will be deprecated; see ADR-053.
// shortest_path is a graph verb and belongs in the KG pack — removed here.
// diff / apply_diff are Phase 2 (not in scope for this PR).
static VCS_VERBS: [VerbDef; 7] = [
    VerbDef {
        name: "commit",
        description: "Create a content-addressed snapshot of the current namespace state",
    },
    VerbDef {
        name: "branch",
        description: "Create, list, or get branches (action: create | list | get)",
    },
    VerbDef {
        name: "checkout",
        description: "Restore namespace to a branch or snapshot state (params: branch_name?, snapshot_id?, force?; snapshot is an alias for snapshot_id for backward compat; branch_name and snapshot_id are mutually exclusive)",
    },
    VerbDef {
        name: "log",
        description: "Show snapshot history for a branch",
    },
    VerbDef {
        name: "merge_branch",
        description: "Three-way merge into the current working state (params: theirs (required, branch name or snapshot ID), target_branch?, strategy?, force?, message?; source_branch is an alias for theirs for backward compat)",
    },
    VerbDef {
        name: "export_kg",
        description: "Export namespace to a portable JSON archive",
    },
    VerbDef {
        name: "import_kg",
        description: "Import a portable JSON archive into the namespace",
    },
];

impl VcsPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl PackRuntime for VcsPack {
    fn name(&self) -> &str {
        "vcs"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <VcsPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <VcsPack as Pack>::ENTITY_KINDS
    }

    fn verbs(&self) -> &'static [VerbDef] {
        &VCS_VERBS
    }

    fn requires(&self) -> &'static [&'static str] {
        <VcsPack as Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "commit" => self.handle_snapshot(params).await,
            "branch" => self.handle_branch(params).await,
            "checkout" => self.handle_checkout(params).await,
            "log" => self.handle_log(params).await,
            "merge_branch" => self.handle_merge_branch(params).await,
            "export_kg" => self.handle_export(params).await,
            "import_kg" => self.handle_import(params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "vcs pack does not handle verb {verb:?}"
            ))),
        }
    }
}
